use anyhow::Result;
use rusqlite::{Connection, params};

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS catalog_counted_photos(
            library_id TEXT NOT NULL,asset_id TEXT NOT NULL,directory TEXT NOT NULL,
            PRIMARY KEY(library_id,asset_id)) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS catalog_directory_photo_counts(
            library_id TEXT NOT NULL,directory TEXT NOT NULL,direct_count INTEGER NOT NULL CHECK(direct_count>=0),
            PRIMARY KEY(library_id,directory)) WITHOUT ROWID;
         DELETE FROM catalog_counted_photos;
         INSERT INTO catalog_counted_photos
            SELECT d.library_id,d.asset_id,d.directory FROM catalog_asset_directories d
            JOIN catalog_assets a ON a.library_id=d.library_id AND a.id=d.asset_id
            WHERE a.trashed=0 AND EXISTS(SELECT 1 FROM catalog_physical_files p
              WHERE p.library_id=d.library_id AND p.asset_id=d.asset_id AND p.role<>'sidecar');
         DELETE FROM catalog_directory_photo_counts;
         INSERT INTO catalog_directory_photo_counts SELECT library_id,directory,count(*)
            FROM catalog_counted_photos GROUP BY library_id,directory;",
    )?;
    let add = "INSERT INTO catalog_directory_photo_counts VALUES(NEW.library_id,NEW.directory,1)
        ON CONFLICT(library_id,directory) DO UPDATE SET direct_count=direct_count+1;";
    let subtract = "UPDATE catalog_directory_photo_counts SET direct_count=direct_count-1
        WHERE library_id=OLD.library_id AND directory=OLD.directory;
        DELETE FROM catalog_directory_photo_counts WHERE library_id=OLD.library_id
        AND directory=OLD.directory AND direct_count=0;";
    let credit_file = "INSERT INTO catalog_counted_photos
        SELECT NEW.library_id,NEW.asset_id,NEW.directory
        WHERE NEW.role<>'sidecar' AND (SELECT trashed=0 FROM catalog_assets WHERE library_id=NEW.library_id AND id=NEW.asset_id)
        ON CONFLICT DO NOTHING;";
    let withdraw_file = "DELETE FROM catalog_counted_photos WHERE library_id=OLD.library_id AND asset_id=OLD.asset_id
        AND OLD.role<>'sidecar' AND NOT EXISTS(SELECT 1 FROM catalog_physical_files p
          WHERE p.library_id=OLD.library_id AND p.asset_id=OLD.asset_id AND p.role<>'sidecar');
        UPDATE catalog_counted_photos SET directory=p.directory FROM catalog_physical_files p
          WHERE catalog_counted_photos.library_id=OLD.library_id AND catalog_counted_photos.asset_id=OLD.asset_id
          AND p.library_id=OLD.library_id AND p.asset_id=OLD.asset_id AND p.role<>'sidecar'
          AND catalog_counted_photos.directory<>p.directory;";
    let restore = "INSERT INTO catalog_counted_photos
        SELECT library_id,asset_id,directory FROM catalog_physical_files
        WHERE library_id=NEW.library_id AND asset_id=NEW.id AND role<>'sidecar' LIMIT 1
        ON CONFLICT DO NOTHING;";
    db.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS counted_photo_added AFTER INSERT ON catalog_counted_photos BEGIN {add} END;
         CREATE TRIGGER IF NOT EXISTS counted_photo_removed AFTER DELETE ON catalog_counted_photos BEGIN {subtract} END;
         CREATE TRIGGER IF NOT EXISTS counted_photo_moved AFTER UPDATE OF directory ON catalog_counted_photos
         WHEN OLD.directory<>NEW.directory BEGIN {subtract} {add} END;

         CREATE TRIGGER IF NOT EXISTS count_file_added AFTER INSERT ON catalog_physical_files
         WHEN NEW.role<>'sidecar' BEGIN {credit_file} END;
         CREATE TRIGGER IF NOT EXISTS count_file_removed AFTER DELETE ON catalog_physical_files
         WHEN OLD.role<>'sidecar' BEGIN {withdraw_file} END;
         CREATE TRIGGER IF NOT EXISTS count_file_reassigned AFTER UPDATE OF library_id,asset_id,role ON catalog_physical_files
         WHEN OLD.library_id<>NEW.library_id OR OLD.asset_id<>NEW.asset_id OR (OLD.role='sidecar')<>(NEW.role='sidecar')
         BEGIN {withdraw_file} {credit_file} END;

         CREATE TRIGGER IF NOT EXISTS count_file_moved AFTER UPDATE OF path ON catalog_physical_files
         WHEN OLD.directory<>NEW.directory AND NEW.role<>'sidecar' BEGIN
           UPDATE catalog_counted_photos SET directory=NEW.directory WHERE library_id=NEW.library_id
             AND asset_id=NEW.asset_id AND directory<>NEW.directory;
         END;
         CREATE TRIGGER IF NOT EXISTS count_photo_restored AFTER UPDATE OF trashed ON catalog_assets
         WHEN OLD.trashed<>0 AND NEW.trashed=0 BEGIN {restore} END;
         CREATE TRIGGER IF NOT EXISTS count_photo_trashed AFTER UPDATE OF trashed ON catalog_assets
         WHEN OLD.trashed=0 AND NEW.trashed<>0 BEGIN
           DELETE FROM catalog_counted_photos WHERE library_id=OLD.library_id AND asset_id=OLD.id;
         END;
         CREATE TRIGGER IF NOT EXISTS count_photo_removed AFTER DELETE ON catalog_assets BEGIN
           DELETE FROM catalog_counted_photos WHERE library_id=OLD.library_id AND asset_id=OLD.id;
         END;"
    ))?;
    Ok(())
}

pub(crate) fn read(db: &Connection, lib: &str, path: &str) -> Result<(i64, i64)> {
    let stem = path.trim_end_matches('/');
    let directory = if stem.is_empty() { "/" } else { stem };
    Ok(db
        .prepare_cached(
            "SELECT coalesce((SELECT direct_count FROM catalog_directory_photo_counts
           WHERE library_id=?1 AND directory=?2),0),
         coalesce((SELECT sum(direct_count) FROM catalog_directory_photo_counts
           WHERE library_id=?1 AND directory>=?3 AND directory<?4 AND directory<>?2),0)",
        )?
        .query_row(
            params![lib, directory, format!("{stem}/"), format!("{stem}0")],
            |r| {
                let direct = r.get::<_, i64>(0)?;
                Ok((direct, direct + r.get::<_, i64>(1)?))
            },
        )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{directory_move, physical_files, store::Store};

    fn check(db: &Connection) -> Result<()> {
        let live =
            "SELECT DISTINCT p.library_id,p.asset_id,p.directory FROM catalog_physical_files p
            JOIN catalog_assets a ON a.library_id=p.library_id AND a.id=p.asset_id
            WHERE a.trashed=0 AND p.role<>'sidecar'";
        for (projected, expected) in [
            ("SELECT * FROM catalog_counted_photos", live.to_string()),
            (
                "SELECT * FROM catalog_directory_photo_counts",
                format!(
                    "SELECT library_id,directory,count(*) FROM ({live}) GROUP BY library_id,directory"
                ),
            ),
        ] {
            for (left, right) in [
                (projected, expected.as_str()),
                (expected.as_str(), projected),
            ] {
                let differs =
                    db.query_row(&format!("SELECT EXISTS({left} EXCEPT {right})"), [], |r| {
                        r.get::<_, bool>(0)
                    })?;
                assert!(!differs, "{left} EXCEPT {right}");
            }
        }
        Ok(())
    }

    #[test]
    fn directory_counts_follow_media_membership_moves_and_transaction_rollback() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("catalog.sqlite");
        let store = Store::open(&path, true)?;
        let mut db = store.lock()?;
        let tx = db.transaction()?;
        for id in ["a", "b"] {
            tx.execute("INSERT INTO catalog_assets(library_id,id,snapshot,content_hash,fingerprint,sort_time,filename,rating,flag) VALUES('lib',?1,json_object('id',?1),'hash','','2026','photo.jpg',0,'unflagged')", [id])?;
        }
        for (id, file, role) in [
            ("a", "/亲/子/a.xmp", "sidecar"),
            ("a", "/亲/子/a.jpg", "jpeg_original"),
            ("a", "/亲/子/a.raw", "raw_original"),
            ("b", "/亲-/b.jpg", "jpeg_original"),
        ] {
            physical_files::publish(&tx, "lib", id, file, "hash", role, false)?;
            check(&tx)?;
        }
        assert_eq!(read(&tx, "lib", "/亲")?, (0, 1));
        assert_eq!(read(&tx, "lib", "/")?, (0, 2));
        tx.commit()?;
        let tx = db.transaction()?;
        directory_move::catalog(&tx, "lib", "/亲", "/新")?;
        check(&tx)?;
        assert_eq!(read(&tx, "lib", "/新/")?, (0, 1));
        tx.rollback()?;
        check(&db)?;
        assert_eq!(read(&db, "lib", "/亲/子")?, (1, 1));
        let tx = db.transaction()?;
        for trashed in [1, 1, 0, 0] {
            tx.execute(
                "UPDATE catalog_assets SET trashed=? WHERE id='a'",
                [trashed],
            )?;
            check(&tx)?;
        }
        physical_files::withdraw(&tx, "lib", "/亲/子/a.jpg")?;
        check(&tx)?;
        physical_files::withdraw(&tx, "lib", "/亲/子/a.raw")?;
        check(&tx)?;
        assert_eq!(read(&tx, "lib", "/亲")?, (0, 0));
        tx.execute(
            "UPDATE catalog_physical_files SET role='jpeg_original' WHERE asset_id='a'",
            [],
        )?;
        check(&tx)?;
        tx.execute(
            "UPDATE catalog_physical_files SET role='sidecar' WHERE asset_id='b'",
            [],
        )?;
        check(&tx)?;
        physical_files::withdraw(&tx, "lib", "/亲-/b.jpg")?;
        tx.execute(
            "UPDATE catalog_physical_files SET asset_id='b' WHERE asset_id='a'",
            [],
        )?;
        check(&tx)?;
        tx.commit()?;
        drop(db);
        drop(store);
        check(&Store::open(&path, false)?.read()?)
    }
}
