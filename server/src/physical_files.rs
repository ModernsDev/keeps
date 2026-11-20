use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = "
CREATE TABLE catalog_asset_directories(
    library_id TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    directory TEXT NOT NULL,
    PRIMARY KEY(library_id,asset_id),
    UNIQUE(library_id,asset_id,directory),
    FOREIGN KEY(library_id,asset_id) REFERENCES catalog_assets(library_id,id) ON DELETE CASCADE
);
CREATE TABLE catalog_physical_files(
    library_id TEXT NOT NULL,
    path TEXT NOT NULL CHECK(substr(path,1,1)='/' AND instr(path,'//')=0
        AND instr(path,'/../')=0 AND instr(path,'/./')=0 AND instr(path,char(0))=0),
    asset_id TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    role TEXT NOT NULL,
    versioned INTEGER NOT NULL DEFAULT 0 CHECK(versioned IN (0,1) AND (role<>'sidecar' OR versioned=0)),
    basename TEXT GENERATED ALWAYS AS (json_extract(replace(json_array(path), '/', '\",\"'), '$[#-1]')) STORED
        CHECK(basename NOT IN ('','.','..')),
    directory TEXT GENERATED ALWAYS AS (CASE WHEN length(path)=length(basename)+1
        THEN '/' ELSE substr(path,1,length(path)-length(basename)-1) END) STORED,
    PRIMARY KEY(library_id,path),
    FOREIGN KEY(library_id,asset_id,directory)
        REFERENCES catalog_asset_directories(library_id,asset_id,directory) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX catalog_physical_files_asset ON catalog_physical_files(library_id,asset_id,path);
CREATE INDEX catalog_physical_files_version ON catalog_physical_files(library_id,asset_id,content_hash,versioned,path);
CREATE TABLE catalog_version_path_history(
    library_id TEXT NOT NULL,
    path TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    PRIMARY KEY(library_id,path)
);
CREATE INDEX catalog_version_path_history_asset ON catalog_version_path_history(library_id,asset_id,content_hash,path);
CREATE TRIGGER catalog_physical_files_directory_insert AFTER INSERT ON catalog_physical_files BEGIN
    INSERT INTO catalog_asset_directories VALUES(NEW.library_id,NEW.asset_id,NEW.directory)
        ON CONFLICT(library_id,asset_id) DO NOTHING;
END;
CREATE TRIGGER catalog_physical_files_directory_update AFTER UPDATE OF asset_id ON catalog_physical_files
WHEN NEW.asset_id<>OLD.asset_id BEGIN
    INSERT INTO catalog_asset_directories VALUES(NEW.library_id,NEW.asset_id,NEW.directory)
        ON CONFLICT(library_id,asset_id) DO NOTHING;
END;
";

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    if db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='catalog_physical_files')", [], |r| r.get::<_, bool>(0))? {
        return Ok(());
    }
    let conflict: Option<String> = db
        .query_row(
            "SELECT p.path FROM catalog_paths p JOIN catalog_version_paths v USING(library_id,path)
         WHERE p.asset_id<>v.asset_id OR p.content_hash<>v.content_hash OR v.available=0 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        conflict.is_none(),
        "legacy path indexes disagree, run the photo placement audit: {conflict:?}"
    );
    db.execute_batch(SCHEMA)?;
    db.execute_batch("
        INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role,versioned)
        SELECT p.library_id,p.path,p.asset_id,p.content_hash,p.role,coalesce(v.available,0)
        FROM catalog_paths p LEFT JOIN catalog_version_paths v USING(library_id,path) ORDER BY p.rowid;
        INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role,versioned)
        SELECT v.library_id,v.path,v.asset_id,v.content_hash,
            coalesce((SELECT role FROM catalog_files f WHERE f.library_id=v.library_id AND f.asset_id=v.asset_id
                AND f.content_hash=v.content_hash AND f.role IN ('raw_original','jpeg_original','video_original')
                ORDER BY (holder='keeps-nas') DESC,role LIMIT 1),
                CASE WHEN EXISTS(SELECT 1 FROM videos t WHERE t.library_id=v.library_id AND t.id=v.asset_id)
                    THEN 'video_original' ELSE 'jpeg_original' END),1
        FROM catalog_version_paths v WHERE v.available=1
        AND NOT EXISTS(SELECT 1 FROM catalog_paths p WHERE p.library_id=v.library_id AND p.path=v.path);
        INSERT INTO catalog_version_path_history SELECT library_id,path,asset_id,content_hash
            FROM catalog_version_paths WHERE available=0;
    ")?;
    let invalid: Option<String> = db.query_row(
        "SELECT p.path FROM catalog_physical_files p JOIN catalog_asset_directories a USING(library_id,asset_id)
         WHERE p.directory<>a.directory LIMIT 1", [], |r| r.get(0),
    ).optional()?;
    ensure!(
        invalid.is_none(),
        "an asset has files in multiple directories, run the photo placement audit: {invalid:?}"
    );
    db.execute_batch("
        DROP VIEW catalog_asset_locations;
        DROP TABLE catalog_paths;
        DROP TABLE catalog_version_paths;
        DROP TABLE catalog_location_choices;
        CREATE VIEW catalog_paths AS
            SELECT library_id,path,asset_id,content_hash,role FROM catalog_physical_files WHERE role<>'sidecar';
        CREATE VIEW catalog_version_paths AS
            SELECT library_id,path,asset_id,content_hash,1 AS available FROM catalog_physical_files WHERE versioned=1
            UNION ALL
            SELECT h.library_id,h.path,h.asset_id,h.content_hash,0 FROM catalog_version_path_history h
            WHERE NOT EXISTS(SELECT 1 FROM catalog_physical_files p WHERE p.library_id=h.library_id AND p.path=h.path);
        CREATE VIEW catalog_asset_locations AS
            SELECT p.library_id,p.asset_id,p.path FROM catalog_physical_files p
            WHERE p.role<>'sidecar' AND p.rowid=(
                SELECT q.rowid FROM catalog_physical_files q
                LEFT JOIN catalog_deprecated_files x ON x.library_id=q.library_id AND x.path=q.path
                WHERE q.library_id=p.library_id AND q.asset_id=p.asset_id AND q.role<>'sidecar'
                ORDER BY x.path IS NOT NULL,q.rowid LIMIT 1);
    ")?;
    crate::revisions::install_triggers(db)?;
    Ok(())
}

pub(crate) fn publish(
    db: &Connection,
    lib: &str,
    id: &str,
    path: &str,
    hash: &str,
    role: &str,
    versioned: bool,
) -> Result<bool> {
    db.execute(
        "DELETE FROM catalog_version_path_history WHERE library_id=? AND path=?",
        params![lib, path],
    )?;
    let changed = db.execute(
        "
        INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role,versioned)
        VALUES (?,?,?,?,?,?)
        ON CONFLICT(library_id,path) DO UPDATE SET asset_id=excluded.asset_id,
            content_hash=excluded.content_hash,role=excluded.role,versioned=excluded.versioned
        WHERE asset_id IS NOT excluded.asset_id OR content_hash IS NOT excluded.content_hash
            OR role IS NOT excluded.role OR versioned IS NOT excluded.versioned",
        params![lib, path, id, hash, role, versioned],
    )?;
    Ok(changed > 0)
}

pub(crate) fn withdraw(db: &Connection, lib: &str, path: &str) -> Result<bool> {
    db.execute("
        INSERT INTO catalog_version_path_history(library_id,path,asset_id,content_hash)
        SELECT library_id,path,asset_id,content_hash FROM catalog_physical_files WHERE library_id=? AND path=? AND versioned=1
        ON CONFLICT(library_id,path) DO UPDATE SET asset_id=excluded.asset_id,content_hash=excluded.content_hash
    ", params![lib,path])?;
    Ok(db.execute(
        "DELETE FROM catalog_physical_files WHERE library_id=? AND path=?",
        params![lib, path],
    )? > 0)
}

pub(crate) fn parent(path: &str) -> Result<&str> {
    let (directory, name) = path
        .rsplit_once('/')
        .context("managed file path must be absolute")?;
    ensure!(
        path.starts_with('/') && !name.is_empty(),
        "managed file path must be absolute and name a file"
    );
    Ok(if directory.is_empty() { "/" } else { directory })
}

#[cfg(test)]
pub(crate) fn restore_legacy_path_tables(db: &Connection) -> Result<()> {
    db.execute_batch("
        CREATE TEMP TABLE legacy_paths AS SELECT * FROM catalog_paths;
        CREATE TEMP TABLE legacy_version_paths AS SELECT * FROM catalog_version_paths;
        DROP VIEW catalog_asset_locations;
        DROP VIEW catalog_paths;
        DROP VIEW catalog_version_paths;
        DROP TABLE catalog_physical_files;
        DROP TABLE catalog_asset_directories;
        DROP TABLE catalog_version_path_history;
        CREATE TABLE catalog_paths(library_id TEXT NOT NULL,path TEXT NOT NULL,asset_id TEXT NOT NULL,content_hash TEXT NOT NULL,role TEXT NOT NULL,PRIMARY KEY(library_id,path));
        CREATE INDEX catalog_paths_asset ON catalog_paths(library_id,asset_id);
        CREATE TABLE catalog_version_paths(library_id TEXT NOT NULL,path TEXT NOT NULL,asset_id TEXT NOT NULL,content_hash TEXT NOT NULL,available INTEGER NOT NULL,PRIMARY KEY(library_id,path));
        INSERT INTO catalog_paths SELECT * FROM legacy_paths;
        INSERT INTO catalog_version_paths SELECT * FROM legacy_version_paths;
        DROP TABLE legacy_paths;
        DROP TABLE legacy_version_paths;
        CREATE TABLE catalog_location_choices(library_id TEXT NOT NULL,asset_id TEXT NOT NULL,path TEXT NOT NULL,PRIMARY KEY(library_id,asset_id));
    ")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use serde_json::json;

    #[test]
    fn direct_writes_cannot_publish_a_second_directory_or_assign_a_path_twice() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("catalog.sqlite");
        let store = Store::open(&path, true)?;
        let file = json!({"contentHash":"raw","sizeBytes":42,"role":"raw_original"});
        let first = store.ingest_original(
            "lib", "/照片/2025/a.3FR", &file,
            &json!({"contentFingerprint":"raw","metadataFingerprint":"fixture","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","originalFilename":"a.3FR","rating":0,"flagState":"unflagged","tags":[]}),
        )?;
        let second = store.ingest_original(
            "lib", "/照片/2025/b.3FR",
            &json!({"contentHash":"other","sizeBytes":42,"role":"raw_original"}),
            &json!({"contentFingerprint":"other","metadataFingerprint":"fixture","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","originalFilename":"b.3FR","rating":0,"flagState":"unflagged","tags":[]}),
        )?;
        assert_ne!(first, second);
        let mut writer = Connection::open(path)?;
        writer.pragma_update(None, "foreign_keys", true)?;
        let tx = writer.transaction()?;
        tx.execute(
            "INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role)
            VALUES('lib','/照片/2025/a.3FR.xmp',?,'xmp','sidecar')",
            [&first],
        )?;
        tx.commit()?;
        let tx = writer.transaction()?;
        tx.execute(
            "INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role)
            VALUES('lib','/照片/2024/a.heif',?,'heif','jpeg_original')",
            [&first],
        )?;
        assert!(tx.commit().is_err());
        assert!(
            writer
                .execute(
                    "INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role)
            VALUES('lib','/照片/2025/a.3FR',?,'raw','raw_original')",
                    [&second],
                )
                .is_err()
        );
        assert!(writer.execute(
            "INSERT INTO catalog_paths VALUES('lib','/照片/2025/b.heif',?,'heif','jpeg_original')",
            [&second],
        ).is_err());
        assert_eq!(
            writer.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get::<_, i64>(0)
            })?,
            0
        );
        Ok(())
    }
}
