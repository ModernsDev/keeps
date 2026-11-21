use crate::store::{Store, StoreError};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

fn missing_paths_query(scope: &str) -> Result<String> {
    let predicate = match scope {
        "file" => "p.path=?2",
        "directory" => "p.path>=?2 AND p.path<?3 AND instr(substr(p.path,length(?2)+1),'/')=0",
        "recursive" => "p.path>=?2 AND p.path<?3",
        _ => anyhow::bail!("invalid missing reconciliation scope"),
    };
    Ok(format!(
        "SELECT p.asset_id,p.path,p.content_hash,f.size,p.role FROM catalog_physical_files p LEFT JOIN catalog_files f ON f.library_id=p.library_id AND f.asset_id=p.asset_id AND f.content_hash=p.content_hash AND f.role=p.role AND f.holder='keeps-nas' WHERE p.library_id=?1 AND {predicate}"
    ))
}

const VERSION: i64 = 27;
const DIRECTORY_EXACT: &str = "SELECT library_id,asset_id,directory FROM catalog_asset_directories WHERE library_id=? AND directory=?";
const DIRECTORY_RANGE: &str = "SELECT library_id,asset_id,directory FROM catalog_asset_directories WHERE library_id=? AND directory>=? AND directory<?";
const LIVE_ASSET: &str = "EXISTS(SELECT 1 FROM catalog_physical_files live
    WHERE live.library_id=a.library_id AND live.asset_id=a.id AND live.role<>'sidecar')";
// Ordering by path must not make SQLite scan the whole library for every asset.
const ASSET_PATHS: &str = "SELECT path FROM catalog_physical_files INDEXED BY catalog_physical_files_asset WHERE library_id=? AND asset_id=? AND role<>'sidecar' ORDER BY path";
// Path updates preserve rowid, so an explicit move carries the photo location with it.
const ASSET_LOCATIONS: &str = "CREATE VIEW IF NOT EXISTS catalog_asset_locations AS
    SELECT p.library_id,p.asset_id,p.path FROM catalog_paths p WHERE p.rowid=(
        SELECT q.rowid FROM catalog_paths q
        LEFT JOIN catalog_deprecated_files x ON x.library_id=q.library_id AND x.path=q.path
        WHERE q.library_id=p.library_id AND q.asset_id=p.asset_id
        ORDER BY NOT EXISTS(SELECT 1 FROM catalog_location_choices c WHERE c.library_id=q.library_id AND c.asset_id=q.asset_id AND c.path=q.path),
                 x.path IS NOT NULL,q.rowid LIMIT 1
    );";

pub(crate) fn photo_location(db: &Connection, lib: &str, id: &str) -> Result<Option<String>> {
    Ok(db
        .prepare_cached(
            "SELECT path FROM catalog_asset_locations WHERE library_id=? AND asset_id=?",
        )?
        .query_row(params![lib, id], |row| row.get(0))
        .optional()?)
}

pub(crate) fn photo_location_paths(db: &Connection, lib: &str, id: &str) -> Result<Vec<String>> {
    let mut paths = db.prepare_cached(ASSET_PATHS)?;
    Ok(paths
        .query_map(params![lib, id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn directory_scope(
    lib: &str,
    path: &str,
    recursive: bool,
) -> (String, Vec<rusqlite::types::Value>) {
    let stem = path.trim_end_matches('/');
    let directory = if stem.is_empty() { "/" } else { stem };
    let range = || {
        vec![
            lib.to_owned().into(),
            format!("{stem}/").into(),
            format!("{stem}0").into(),
        ]
    };
    if recursive && stem.is_empty() {
        return (DIRECTORY_RANGE.into(), range());
    }
    let mut values = vec![lib.to_owned().into(), directory.to_owned().into()];
    if recursive {
        values.extend(range());
        (
            format!("{DIRECTORY_EXACT} UNION ALL {DIRECTORY_RANGE}"),
            values,
        )
    } else {
        (DIRECTORY_EXACT.into(), values)
    }
}

pub(crate) fn directory_photo_counts(db: &Connection, lib: &str, path: &str) -> Result<(i64, i64)> {
    let direct = path.trim_end_matches('/');
    let direct = if direct.is_empty() { "/" } else { direct };
    let (scope, mut values) = directory_scope(lib, path, true);
    values.insert(0, direct.to_owned().into());
    Ok(db
        .prepare_cached(&format!(
            "SELECT count(*) FILTER(WHERE d.directory=?), count(*)
        FROM ({scope}) d JOIN catalog_assets a ON a.library_id=d.library_id AND a.id=d.asset_id
        WHERE a.trashed=0 AND {LIVE_ASSET}"
        ))?
        .query_row(rusqlite::params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?)
}

const SCHEMA: &str = r#"
CREATE TABLE catalog_assets(library_id TEXT NOT NULL,id TEXT NOT NULL,snapshot TEXT NOT NULL,content_hash TEXT NOT NULL,fingerprint TEXT NOT NULL,sort_time TEXT NOT NULL,filename TEXT NOT NULL,rating INTEGER NOT NULL,flag TEXT NOT NULL,color TEXT,trashed INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(library_id,id));
CREATE INDEX catalog_hash ON catalog_assets(library_id,content_hash);
CREATE INDEX catalog_fingerprint ON catalog_assets(library_id,fingerprint);
CREATE INDEX catalog_sort ON catalog_assets(library_id,trashed,sort_time,id);
CREATE TABLE catalog_files(library_id TEXT NOT NULL,asset_id TEXT NOT NULL,content_hash TEXT NOT NULL,size INTEGER NOT NULL,role TEXT NOT NULL,holder TEXT NOT NULL,availability TEXT NOT NULL,PRIMARY KEY(library_id,asset_id,content_hash,role,holder));
CREATE INDEX catalog_file_hash ON catalog_files(library_id,content_hash);
CREATE TABLE catalog_paths(library_id TEXT NOT NULL,path TEXT NOT NULL,asset_id TEXT NOT NULL,content_hash TEXT NOT NULL,role TEXT NOT NULL,PRIMARY KEY(library_id,path));
CREATE INDEX catalog_paths_asset ON catalog_paths(library_id,asset_id);
"#;

pub fn ensure_schema(db: &mut Connection, allow_migrate: bool) -> Result<()> {
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        version <= VERSION,
        "database schema {version} is newer than this server supports ({VERSION})"
    );
    if version == VERSION {
        db.prepare("SELECT library_id,kind,value,asset_id FROM catalog_photo_sources LIMIT 0")?;
        db.prepare(
            "SELECT library_id,asset_id,created_at FROM catalog_missing_capture_dates LIMIT 0",
        )?;
        db.prepare("SELECT library_id,generation,floor FROM catalog_sync_state LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,revision,deleted,changed_at FROM catalog_sync_assets LIMIT 0")?;
        db.prepare("SELECT library_id,replica_id,generation,ack_revision,delivered_revision,expires_at FROM catalog_replicas LIMIT 0")?;
        db.prepare("SELECT library_id,name FROM catalog_tags LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,trashed_at FROM catalog_trash_entries LIMIT 0")?;
        db.prepare("SELECT library_id,enabled,retention_days FROM trash_settings LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,metadata FROM photo_edit_decisions LIMIT 0")?;
        db.prepare("SELECT library_id,kind,revision,content FROM ai_editing_state LIMIT 0")?;
        db.prepare("SELECT negative_hash,edit_revision,negative_selected,last_edit_request FROM catalog_assets LIMIT 0")?;
        db.prepare(
            "SELECT negative_hash,exposure_ev,standard,thumbnail,browse,recipe FROM photo_edits LIMIT 0",
        )?;
        db.prepare("SELECT request_id,payload,response FROM photo_edit_requests LIMIT 0")?;
        db.prepare("SELECT library_id,path,retained_path,basis,reason FROM catalog_deprecated_files LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,root_id FROM catalog_identity_roots LIMIT 0")?;
        db.prepare("SELECT library_id,root_id,asset_id FROM catalog_identity_aliases LIMIT 0")?;
        db.prepare("SELECT id,expires_at,completed FROM remote_cache_tasks LIMIT 0")?;
        db.prepare("SELECT library_id,batch_id,path,worker_id,expires_at FROM import_compute_leases LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,role,file_object,object_bucket,object_key,object_etag,pixel_width,pixel_height,updated_at FROM derivative_objects LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,bucket,object_key,not_before,attempts,last_error FROM media_cache_gc LIMIT 0")?;
        db.prepare("SELECT library_id,id,snapshot FROM photos LIMIT 0")?;
        db.prepare("SELECT library_id,id,snapshot FROM videos LIMIT 0")?;
        db.prepare("SELECT library_id,path,parent_path FROM directories LIMIT 0")?;
        db.prepare("SELECT library_id,path,active FROM folders LIMIT 0")?;
        db.prepare("SELECT id FROM directory_inventory_pending LIMIT 0")?;
        db.prepare("SELECT library_id,path FROM directory_creations LIMIT 0")?;
        db.prepare("SELECT library_id,path,verified_hash FROM catalog_file_checks LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,source_hash,spec,status,attempts,available_at,last_error,thumbnail,standard,updated_at,audited_at,published_source_hash,published_spec,thumbnail_operation_id,browse_operation_id FROM media_cache LIMIT 0")?;
        db.prepare("SELECT next_batch_at,last_batch_count,last_batch_at,last_error,free_bytes FROM cache_runtime LIMIT 0")?;
        db.prepare("SELECT snapshot,content_hash,fingerprint,sort_time,filename,rating,flag,color,trashed FROM catalog_assets LIMIT 0")?;
        db.prepare(
            "SELECT asset_id,content_hash,size,role,holder,availability FROM catalog_files LIMIT 0",
        )?;
        db.prepare("SELECT library_id,path,asset_id,content_hash,role FROM catalog_paths LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,path FROM catalog_asset_locations LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,directory FROM catalog_asset_directories INDEXED BY catalog_asset_directories_directory LIMIT 0")?;
        db.prepare("SELECT library_id,path FROM catalog_hidden_directories LIMIT 0")?;
        db.prepare("SELECT library_id,asset_id,content_hash,visual_hash,capture_key,width,height,priority,evidence FROM catalog_versions LIMIT 0")?;
        db.prepare("SELECT library_id,path,asset_id,content_hash,available FROM catalog_version_paths LIMIT 0")?;
        db.prepare(
            "SELECT library_id,asset_id,content_hash,user_selected FROM catalog_defaults LIMIT 0",
        )?;
        db.prepare("SELECT library_id,revision FROM catalog_version_revision LIMIT 0")?;
        db.prepare(
            "SELECT library_id,path,parent_path,revision FROM catalog_directory_revisions LIMIT 0",
        )?;
        db.prepare("SELECT library_id,kind,key FROM catalog_revision_dirty LIMIT 0")?;
        db.prepare("SELECT library_id,value FROM catalog_revision_sequence LIMIT 0")?;
        db.prepare(
            "SELECT library_id,path,owner,last_changed_at FROM catalog_revision_updates LIMIT 0",
        )?;
        db.prepare("SELECT browse_thumbnail,browse_spec,browse_source_version,browse_attempts,browse_available_at,browse_last_error FROM media_cache LIMIT 0")?;
        return Ok(());
    }
    ensure!(
        allow_migrate,
        "database migration required: stop the service, back up SQLite, then run keeps-server migrate"
    );
    let foreign_keys: bool = db.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    db.pragma_update(None, "foreign_keys", false)?;
    let migration = migrate_schema(db, version);
    db.pragma_update(None, "foreign_keys", foreign_keys)?;
    migration
}

fn migrate_schema(db: &mut Connection, version: i64) -> Result<()> {
    tracing::info!(from = version, to = VERSION, "migrating NAS catalog");
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if version == 0 {
        let legacy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ledger_events')", [], |r| r.get(0))?;
        if legacy {
            let populated: bool =
                tx.query_row("SELECT EXISTS(SELECT 1 FROM ledger_events)", [], |r| {
                    r.get(0)
                })?;
            ensure!(
                !populated,
                "legacy ledger-only database requires explicit conversion with the previous server; historical events are never replayed by this server"
            );
        }
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS derivative_objects(library_id TEXT NOT NULL,asset_id TEXT NOT NULL,role TEXT NOT NULL,file_object TEXT NOT NULL,object_bucket TEXT NOT NULL,object_key TEXT NOT NULL,object_etag TEXT,pixel_width INTEGER NOT NULL,pixel_height INTEGER NOT NULL,updated_at TEXT NOT NULL,PRIMARY KEY(library_id,asset_id,role));")?;
    }

    if version < 2 {
        tx.execute_batch("CREATE TABLE catalog_hidden_directories(library_id TEXT NOT NULL,path TEXT NOT NULL,PRIMARY KEY(library_id,path));")?;
    }
    if version < 3 {
        crate::versions::migrate(&tx)?;
    }
    if version < 4 {
        crate::revisions::migrate(&tx)?;
    }
    if version < 5 {
        crate::revisions::migrate_updates(&tx)?;
    }
    if version < 6 {
        crate::cache_pipeline::migrate(&tx)?;
    }
    if version < 7 {
        let has_event_sequence = tx
            .prepare("PRAGMA table_info(derivative_objects)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == "declared_event_seq");
        if has_event_sequence {
            tx.execute_batch("ALTER TABLE derivative_objects DROP COLUMN declared_event_seq;")?;
        }
        tx.execute_batch("DROP TABLE IF EXISTS archive_receipts; DROP TABLE IF EXISTS sync_conflicts; DROP TABLE IF EXISTS device_states; DROP TABLE IF EXISTS ledger_sequence_counters; DROP TABLE IF EXISTS ledger_events;")?;
    }
    if version < 8 {
        crate::remote_worker::migrate(&tx)?;
    }
    if version < 9 {
        tx.execute_batch("CREATE TABLE catalog_identity_roots(library_id TEXT NOT NULL,asset_id TEXT NOT NULL,root_id TEXT NOT NULL,PRIMARY KEY(library_id,asset_id));
            CREATE TABLE catalog_identity_aliases(library_id TEXT NOT NULL,root_id TEXT NOT NULL,asset_id TEXT NOT NULL,PRIMARY KEY(library_id,root_id));
            INSERT INTO catalog_identity_roots SELECT library_id,id,id FROM catalog_assets;
            INSERT INTO catalog_identity_aliases SELECT library_id,id,id FROM catalog_assets;")?;
    }
    if version < 10 {
        crate::photo_relations::migrate(&tx)?;
    }
    crate::browse_cache::migrate(&tx)?;
    if version < 12 {
        crate::edits::migrate(&tx)?;
    }
    if version < 14 {
        crate::remote_worker::migrate(&tx)?;
        crate::edits::migrate_recipe(&tx)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS ai_editing_state(library_id TEXT NOT NULL,kind TEXT NOT NULL,revision INTEGER NOT NULL,content TEXT,PRIMARY KEY(library_id,kind)); CREATE TABLE IF NOT EXISTS photo_edit_decisions(library_id TEXT NOT NULL,asset_id TEXT NOT NULL,metadata TEXT NOT NULL,PRIMARY KEY(library_id,asset_id));")?;
    }
    if version < 15 {
        tx.execute_batch("CREATE INDEX IF NOT EXISTS version_capture_original ON catalog_versions(library_id,json_extract(evidence,'$.captureOriginal'));")?;
    }
    if version < 16 {
        tx.execute_batch("CREATE TABLE IF NOT EXISTS catalog_trash_entries(library_id TEXT NOT NULL,asset_id TEXT NOT NULL,trashed_at INTEGER NOT NULL,PRIMARY KEY(library_id,asset_id));
CREATE INDEX IF NOT EXISTS catalog_trash_due ON catalog_trash_entries(library_id,trashed_at);
CREATE TABLE IF NOT EXISTS trash_settings(library_id TEXT PRIMARY KEY,enabled INTEGER NOT NULL DEFAULT 1,retention_days INTEGER NOT NULL DEFAULT 30 CHECK(retention_days BETWEEN 1 AND 3650));
INSERT OR IGNORE INTO catalog_trash_entries SELECT library_id,id,unixepoch() FROM catalog_assets WHERE trashed=1;
CREATE TRIGGER IF NOT EXISTS catalog_trash_enter AFTER UPDATE OF trashed ON catalog_assets WHEN old.trashed=0 AND new.trashed=1 BEGIN INSERT OR REPLACE INTO catalog_trash_entries VALUES(new.library_id,new.id,unixepoch()); END;
CREATE TRIGGER IF NOT EXISTS catalog_trash_restore AFTER UPDATE OF trashed ON catalog_assets WHEN old.trashed=1 AND new.trashed=0 BEGIN DELETE FROM catalog_trash_entries WHERE library_id=new.library_id AND asset_id=new.id; END;
CREATE TRIGGER IF NOT EXISTS catalog_trash_insert AFTER INSERT ON catalog_assets WHEN new.trashed=1 BEGIN INSERT INTO catalog_trash_entries VALUES(new.library_id,new.id,unixepoch()); END;
CREATE TRIGGER IF NOT EXISTS catalog_trash_remove AFTER DELETE ON catalog_assets BEGIN DELETE FROM catalog_trash_entries WHERE library_id=old.library_id AND asset_id=old.id; END;")?;
    }
    if version < 17 {
        tx.execute_batch("CREATE TABLE IF NOT EXISTS catalog_tags(library_id TEXT NOT NULL,name TEXT NOT NULL,PRIMARY KEY(library_id,name));
INSERT OR IGNORE INTO catalog_tags SELECT a.library_id,trim(t.value) FROM catalog_assets a,json_each(a.snapshot,'$.tags') t WHERE t.type='text' AND length(trim(t.value))>0;
CREATE TRIGGER IF NOT EXISTS catalog_tags_insert AFTER INSERT ON catalog_assets BEGIN INSERT OR IGNORE INTO catalog_tags SELECT new.library_id,trim(value) FROM json_each(new.snapshot,'$.tags') WHERE type='text' AND length(trim(value))>0; END;
CREATE TRIGGER IF NOT EXISTS catalog_tags_update AFTER UPDATE OF snapshot ON catalog_assets WHEN json_extract(old.snapshot,'$.tags') IS NOT json_extract(new.snapshot,'$.tags') BEGIN INSERT OR IGNORE INTO catalog_tags SELECT new.library_id,trim(value) FROM json_each(new.snapshot,'$.tags') WHERE type='text' AND length(trim(value))>0; END;")?;
    }
    if version < 18 {
        crate::cache_gc::migrate_asset_ownership(&tx)?;
    }
    if version < 19 {
        crate::replicas::migrate(&tx)?;
    }
    if version < 20 {
        tx.execute_batch("CREATE TABLE IF NOT EXISTS catalog_photo_sources(library_id TEXT NOT NULL,kind TEXT NOT NULL,value TEXT NOT NULL,asset_id TEXT NOT NULL,PRIMARY KEY(library_id,kind,value,asset_id));
CREATE TABLE IF NOT EXISTS catalog_missing_capture_dates(library_id TEXT NOT NULL,asset_id TEXT NOT NULL,created_at TEXT NOT NULL,PRIMARY KEY(library_id,asset_id));")?;
    }
    if version < 21 {
        crate::cache_pipeline::migrate_published_state(&tx)?;
    }
    if version < 22 {
        crate::cache_pipeline::migrate_operation_state(&tx)?;
    }
    if version < 23 {
        tx.execute_batch("INSERT OR IGNORE INTO catalog_revision_dirty SELECT library_id,'asset',asset_id FROM catalog_trash_entries;")?;
        crate::revisions::flush(&tx)?;
    }
    if version < 24 {
        tx.execute_batch("CREATE TABLE IF NOT EXISTS catalog_location_choices(library_id TEXT NOT NULL,asset_id TEXT NOT NULL,path TEXT NOT NULL,PRIMARY KEY(library_id,asset_id));")?;
        tx.execute_batch(ASSET_LOCATIONS)?;
        tx.execute_batch(
            "INSERT OR IGNORE INTO catalog_revision_dirty
            SELECT library_id,'asset',lower(asset_id) FROM catalog_paths
            GROUP BY library_id,asset_id HAVING count(*)>1;",
        )?;
        crate::revisions::flush(&tx)?;
    }
    if version < 25 {
        crate::physical_files::migrate(&tx)?;
    }
    if version < 26 {
        tx.execute_batch("CREATE INDEX IF NOT EXISTS catalog_asset_directories_directory ON catalog_asset_directories(library_id,directory,asset_id)")?;
        let mut check = tx.prepare("PRAGMA foreign_key_check")?;
        ensure!(
            check.query([])?.next()?.is_none(),
            "catalog paths violate SQLite foreign keys"
        );
    }
    if version < 27 {
        crate::navigation::migrate(&tx)?;
        crate::physical_files::enforce_directory_references(&tx)?;
        let mut check = tx.prepare("PRAGMA foreign_key_check")?;
        ensure!(
            check.query([])?.next()?.is_none(),
            "directory migration violates SQLite foreign keys"
        );
        drop(check);
        if version != 0 {
            tx.execute(
                "INSERT OR IGNORE INTO directory_inventory_pending VALUES(1)",
                [],
            )?;
        }
        tx.execute(
            "DELETE FROM catalog_revision_dirty WHERE kind='navigation'",
            [],
        )?;
    }
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    Ok(())
}

pub(crate) struct SidecarPublication<'a> {
    pub media: &'a str,
    pub path: &'a str,
    pub hash: &'a str,
    pub identity: Option<&'a str>,
    pub authorized_import: bool,
}

pub(crate) fn register_sidecar(
    db: &Connection,
    lib: &str,
    asset: &str,
    input: SidecarPublication<'_>,
) -> Result<()> {
    let SidecarPublication {
        media: media_path,
        path,
        hash,
        identity,
        authorized_import,
    } = input;
    let root: String = db.query_row(
        "SELECT i.root_id FROM catalog_physical_files p JOIN catalog_identity_roots i USING(library_id,asset_id)
         WHERE p.library_id=? AND p.asset_id=? AND p.path=? AND p.role<>'sidecar'",
        params![lib,asset,media_path], |r| r.get(0),
    )?;
    ensure!(
        identity == Some(root.as_str()) || identity.is_none() && authorized_import,
        invalid("XMP 照片身份与原片不一致")
    );
    let current: Option<(String, String)> = db
        .query_row(
            "SELECT asset_id,role FROM catalog_physical_files WHERE library_id=? AND path=?",
            params![lib, path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    ensure!(
        current.is_none_or(|(id, role)| id == asset && role == "sidecar"),
        invalid("XMP 已关联其他照片")
    );
    crate::physical_files::publish(db, lib, asset, path, hash, "sidecar", false)?;
    Ok(())
}

pub(crate) fn ingest_original_in_transaction(
    db: &Connection,
    lib: &str,
    path: &str,
    file: &Value,
    snapshot: &Value,
    evidence: &crate::versions::VersionEvidence,
) -> Result<String> {
    let hash = text(file, "contentHash")?;

    let root = snapshot["photoID"]
        .as_str()
        .map(|value| Uuid::parse_str(value).map(|id| id.to_string()))
        .transpose()
        .context("invalid photoID")?;
    let previous_path_asset: Option<String> = db
        .query_row(
            "SELECT asset_id FROM catalog_paths WHERE library_id=? AND path=? AND content_hash=?",
            params![lib, path, hash],
            |r| r.get(0),
        )
        .optional()?;
    let id = match identified_photo(db, lib, snapshot, root.as_deref(), previous_path_asset)? {
        Some(id) => id,
        None => crate::photo_relations::filename_match(db, lib, path, snapshot)?
            .unwrap_or_else(|| capture_uuid(snapshot)),
    };
    if existing(db, lib, &id)?.is_none() {
        let mut snapshot = snapshot.clone();
        snapshot
            .as_object_mut()
            .context("snapshot is not an object")?
            .remove("assetID");
        snapshot["id"] = json!(id);
        snapshot["trashed"] = json!(false);
        save(db, lib, &snapshot)?;
    }
    let canonical = root.as_deref().unwrap_or(&id);
    db.execute(
        "INSERT OR IGNORE INTO catalog_identity_aliases VALUES(?,?,?)",
        params![lib, canonical, id],
    )?;
    db.execute(
        "INSERT OR IGNORE INTO catalog_identity_roots VALUES(?,?,?)",
        params![lib, id, canonical],
    )?;
    register_file(db, lib, &id, file, "online")?;
    crate::versions::register(db, lib, &id, path, file, snapshot, evidence)?;
    for (kind, value) in photo_source_keys(snapshot) {
        db.execute(
            "INSERT OR IGNORE INTO catalog_photo_sources VALUES(?,?,?,?)",
            params![lib, kind, value, id],
        )?;
    }
    if let Some(time) = snapshot["captureTime"].as_str() {
        db.execute("UPDATE catalog_assets SET snapshot=json_set(snapshot,'$.captureTime',?),sort_time=? WHERE library_id=? AND id=? AND json_extract(snapshot,'$.captureTime') IS NULL",params![time,time,lib,id])?;
    }
    let known_date: bool = db.query_row("SELECT json_extract(snapshot,'$.captureTime') IS NOT NULL FROM catalog_assets WHERE library_id=? AND id=?",params![lib,id],|r|r.get(0))?;
    if known_date {
        db.execute(
            "DELETE FROM catalog_missing_capture_dates WHERE library_id=? AND asset_id=?",
            params![lib, id],
        )?;
    } else {
        db.execute(
            "INSERT OR IGNORE INTO catalog_missing_capture_dates VALUES(?,?,?)",
            params![lib, id, Utc::now().to_rfc3339()],
        )?;
    }
    let id = db.query_row(
        "SELECT asset_id FROM catalog_paths WHERE library_id=? AND path=?",
        params![lib, path],
        |r| r.get::<_, String>(0),
    )?;
    Ok(id)
}

pub(crate) fn photo_source_keys(
    snapshot: &Value,
) -> impl Iterator<Item = (&'static str, String)> + '_ {
    [
        ("camera", "cameraImageID"),
        ("software", "originalDocumentID"),
        ("software", "documentID"),
    ]
    .into_iter()
    .filter_map(|(kind, field)| {
        let raw = snapshot[field].as_str().filter(|value| !value.is_empty())?;
        let value = if kind == "software" {
            crate::identity::normalize(raw).unwrap_or_else(|| raw.to_owned())
        } else {
            raw.to_owned()
        };
        Some((kind, value))
    })
}

pub(crate) fn identified_photo(
    db: &Connection,
    lib: &str,
    snapshot: &Value,
    root: Option<&str>,
    previous_path_asset: Option<String>,
) -> Result<Option<String>> {
    let alias = |value: &str| -> Result<Option<String>> {
        Ok(db
            .query_row(
                "SELECT asset_id FROM catalog_identity_aliases WHERE library_id=? AND root_id=?",
                params![lib, value],
                |r| r.get(0),
            )
            .optional()?)
    };
    if let Some(root) = root {
        return Ok(Some(alias(root)?.unwrap_or_else(|| root.to_owned())));
    }
    if previous_path_asset.is_some() {
        return Ok(previous_path_asset);
    }
    for (kind, value) in photo_source_keys(snapshot) {
        if kind == "software"
            && let Some(asset) = alias(&value)?
        {
            return Ok(Some(asset));
        }
        let mut q = db.prepare("SELECT DISTINCT asset_id FROM catalog_photo_sources WHERE library_id=? AND kind=? AND value=? LIMIT 2")?;
        let ids = q
            .query_map(params![lib, kind, value], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if ids.len() == 1 {
            return Ok(ids.into_iter().next());
        }
    }
    Ok(snapshot["originalDocumentID"]
        .as_str()
        .and_then(crate::identity::normalize))
}

fn capture_uuid(snapshot: &Value) -> String {
    let millis = snapshot["captureTime"]
        .as_str()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .map(|v| v.timestamp_millis())
        .unwrap_or_else(|| Utc::now().timestamp_millis());
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[..6].copy_from_slice(&(millis.max(0) as u64).to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    Uuid::from_bytes(bytes).to_string()
}

fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key].as_str().with_context(|| format!("missing {key}"))
}
fn existing(db: &Connection, lib: &str, id: &str) -> Result<Option<Value>> {
    let s: Option<String> = db
        .query_row(
            "SELECT snapshot FROM catalog_assets WHERE library_id=? AND id=?",
            params![lib, id],
            |r| r.get(0),
        )
        .optional()?;
    s.map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}
fn save(db: &Connection, lib: &str, a: &Value) -> Result<()> {
    db.execute("INSERT INTO catalog_assets(library_id,id,snapshot,content_hash,fingerprint,sort_time,filename,rating,flag,color,trashed) VALUES (?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(library_id,id) DO UPDATE SET snapshot=excluded.snapshot,content_hash=excluded.content_hash,fingerprint=excluded.fingerprint,sort_time=excluded.sort_time,filename=excluded.filename,rating=excluded.rating,flag=excluded.flag,color=excluded.color,trashed=excluded.trashed",params![lib,text(a,"id")?,a.to_string(),text(a,"contentFingerprint")?,text(a,"metadataFingerprint")?,a["captureTime"].as_str().unwrap_or(text(a,"createdAt")?),text(a,"originalFilename")?,a["rating"].as_i64().unwrap_or(0),a["flagState"].as_str().unwrap_or("unflagged"),a["colorLabel"].as_str(),a["trashed"].as_bool().unwrap_or(false)])?;
    Ok(())
}

pub(crate) fn register_derivative(db: &Connection, lib: &str, id: &str, d: &Value) -> Result<()> {
    ensure!(
        existing(db, lib, id)?.is_some(),
        "derivative asset does not exist"
    );
    ensure!(
        d["assetID"].as_str().is_none_or(|asset| asset == id),
        "derivative asset mismatch"
    );
    db.execute("INSERT INTO derivative_objects(library_id,asset_id,role,file_object,object_bucket,object_key,object_etag,pixel_width,pixel_height,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(library_id,asset_id,role) DO UPDATE SET file_object=excluded.file_object,object_bucket=excluded.object_bucket,object_key=excluded.object_key,object_etag=excluded.object_etag,pixel_width=excluded.pixel_width,pixel_height=excluded.pixel_height,updated_at=excluded.updated_at",params![lib,id,text(d,"role")?,d["fileObject"].to_string(),text(&d["objectRef"],"bucket")?,text(&d["objectRef"],"key")?,d["objectRef"]["eTag"].as_str(),d["pixelSize"]["width"].as_i64().context("missing derivative width")?,d["pixelSize"]["height"].as_i64().context("missing derivative height")?,Utc::now().to_rfc3339()])?;
    Ok(())
}

pub(crate) fn register_file(
    db: &Connection,
    lib: &str,
    id: &str,
    file: &Value,
    availability: &str,
) -> Result<()> {
    db.execute("INSERT INTO catalog_files(library_id,asset_id,content_hash,size,role,holder,availability) VALUES (?,?,?,?,?,'keeps-nas',?) ON CONFLICT(library_id,asset_id,content_hash,role,holder) DO UPDATE SET availability=excluded.availability",params![lib,id,text(file,"contentHash")?,file["sizeBytes"].as_i64().context("file size missing")?,text(file,"role")?,availability])?;
    Ok(())
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetQuery {
    pub q: Option<String>,
    pub min_rating: Option<i64>,
    pub flag_state: Option<String>,
    pub color_label: Option<String>,
    pub tag: Option<String>,
    pub trashed: Option<bool>,
    pub sort: Option<String>,
    pub directory: Option<String>,
    pub recursive: Option<bool>,
    pub show_hidden: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    #[serde(rename = "folderID")]
    pub folder_id: Option<String>,
}
fn invalid(message: &str) -> anyhow::Error {
    StoreError {
        status: 422,
        code: "invalid_request".into(),
        message: message.into(),
    }
    .into()
}
fn not_found() -> anyhow::Error {
    StoreError {
        status: 404,
        code: "asset_not_found".into(),
        message: "asset not found".into(),
    }
    .into()
}
fn decorate(db: &Connection, lib: &str, mut a: Value) -> Result<Value> {
    let id = text(&a, "id")?.to_owned();
    a["negativeContentHash"] = json!(db.query_row(
        "SELECT negative_hash FROM catalog_assets WHERE library_id=? AND id=?",
        params![lib, id],
        |r| r.get::<_, Option<String>>(0)
    )?);
    if a["trashed"] == true {
        a["trashedAt"] = json!(
            db.prepare_cached(
                "SELECT trashed_at FROM catalog_trash_entries WHERE library_id=? AND asset_id=?"
            )?
            .query_row(params![lib, id], |r| r.get::<_, i64>(0))
            .optional()?
        );
    }
    a["paths"] = json!(photo_location_paths(db, lib, &id)?);
    let preview:Option<String>=db.query_row("SELECT json_object('objectRef',json_object('bucket',object_bucket,'key',object_key),'width',pixel_width,'height',pixel_height,'version',json_extract(file_object,'$.contentHash')) FROM derivative_objects WHERE library_id=? AND asset_id=? AND role='preview'",params![lib,id],|r|r.get(0)).optional()?;
    a["_preview"] = preview
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or(Value::Null);
    Ok(a)
}

fn hidden_filter(
    filter: &mut String,
    values: &mut Vec<rusqlite::types::Value>,
    lib: &str,
    directory: Option<&str>,
) {
    filter.push_str(" AND a.id NOT IN (
        SELECT d.asset_id FROM catalog_hidden_directories h CROSS JOIN catalog_asset_directories d
        ON d.library_id=h.library_id AND d.directory>=h.path AND d.directory<rtrim(h.path,'/') || '0'
           AND (d.directory=h.path OR d.directory>=rtrim(h.path,'/') || '/')
        WHERE h.library_id=? AND NOT (?=h.path OR substr(?,1,length(rtrim(h.path,'/'))+1)=rtrim(h.path,'/') || '/'))");
    values.push(lib.to_string().into());
    let directory = directory
        .map(|p| if p == "/" { p } else { p.trim_end_matches('/') })
        .unwrap_or("");
    values.push(directory.to_string().into());
    values.push(directory.to_string().into());
}

impl Store {
    pub(crate) fn duplicate_locations(&self, _lib: &str) -> Result<Value> {
        Ok(json!({"items":[]}))
    }

    pub(crate) fn choose_photo_location(&self, lib: &str, id: &str, path: &str) -> Result<()> {
        let db = self.read()?;
        let selectable: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM catalog_physical_files p JOIN catalog_assets a ON a.library_id=p.library_id AND a.id=p.asset_id WHERE p.library_id=? AND p.asset_id=? AND p.path=? AND p.role<>'sidecar' AND a.trashed=0)",
            params![lib,id,path], |r| r.get(0),
        )?;
        ensure!(
            selectable && std::fs::symlink_metadata(path)?.is_file(),
            invalid("照片路径未登记或已不可用")
        );
        Ok(())
    }

    pub fn hidden_directories(&self, lib: &str) -> Result<Value> {
        let db = self.read()?;
        let mut statement = db.prepare(
            "SELECT path FROM catalog_hidden_directories WHERE library_id=? ORDER BY path",
        )?;
        let paths = statement
            .query_map([lib], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"paths": paths}))
    }
    pub fn set_hidden_directory(&self, lib: &str, path: &str, hidden: bool) -> Result<Value> {
        if !path.starts_with('/')
            || path.contains('\0')
            || path.split('/').any(|part| part == "." || part == "..")
            || path.contains("//")
        {
            return Err(invalid(
                "path must be an absolute normalized directory path",
            ));
        }
        let path = if path == "/" {
            path
        } else {
            path.trim_end_matches('/')
        };
        {
            let mut connection = self.lock()?;
            let db = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if hidden {
                db.execute("INSERT INTO catalog_hidden_directories(library_id,path) VALUES (?,?) ON CONFLICT DO NOTHING", params![lib,path])?;
            } else {
                db.execute(
                    "DELETE FROM catalog_hidden_directories WHERE library_id=? AND path=?",
                    params![lib, path],
                )?;
            }
            crate::revisions::flush(&db)?;
            db.commit()?;
        }
        self.hidden_directories(lib)
    }
    pub fn query_assets(&self, lib: &str, q: &AssetQuery) -> Result<Value> {
        self.query_assets_impl(lib, q, false)
    }

    pub(crate) fn query_assets_with_media(&self, lib: &str, q: &AssetQuery) -> Result<Value> {
        self.query_assets_impl(lib, q, true)
    }

    fn query_assets_impl(&self, lib: &str, q: &AssetQuery, include_media: bool) -> Result<Value> {
        let sort = q.sort.as_deref().unwrap_or("capture_desc");
        let limit = q.limit.unwrap_or(100).clamp(1, 200);
        let from = if sort == "trash_desc" {
            "catalog_assets a LEFT JOIN catalog_trash_entries t ON t.library_id=a.library_id AND t.asset_id=a.id"
        } else {
            "catalog_assets a"
        };
        let sort_time = if sort == "trash_desc" {
            "printf('%020lld',coalesce(t.trashed_at,0))"
        } else {
            "a.sort_time"
        };
        let cursor_prefix = match sort {
            "trash_desc" => Some("td1."),
            "capture_desc" => Some("cd1."),
            _ => None,
        };
        let order = match sort {
            "trash_desc" => "sort_key DESC,a.id",
            "capture_desc" => "a.sort_time DESC,a.id",
            "capture_asc" => "a.sort_time ASC,a.id",
            "filename" => "a.filename COLLATE NOCASE,a.id",
            "rating_desc" => "a.rating DESC,a.sort_time DESC,a.id",
            _ => return Err(invalid("invalid sort")),
        };
        let mut boundary: Option<(String, String)> = None;
        let mut offset = 0_i64;
        if let Some(cursor) = &q.cursor {
            if let Some(encoded) = cursor_prefix.and_then(|prefix| cursor.strip_prefix(prefix)) {
                let bytes = URL_SAFE_NO_PAD
                    .decode(encoded)
                    .map_err(|_| invalid("invalid cursor"))?;
                boundary =
                    Some(serde_json::from_slice(&bytes).map_err(|_| invalid("invalid cursor"))?);
            } else {
                // Installed clients may resume a persisted offset; the next page upgrades it.
                offset = cursor.parse().map_err(|_| invalid("invalid cursor"))?;
                if offset < 0 {
                    return Err(invalid("invalid cursor"));
                }
            }
        }
        let mut filter = format!("a.library_id=? AND a.trashed=? AND {LIVE_ASSET}");
        let mut values: Vec<rusqlite::types::Value> = vec![
            lib.to_string().into(),
            (q.trashed.unwrap_or(false) as i64).into(),
        ];
        if let Some(s) = q.q.as_ref().filter(|v| !v.is_empty()) {
            filter.push_str(" AND (a.filename LIKE ? ESCAPE '\\' OR json_extract(a.snapshot,'$.cameraModel') LIKE ? ESCAPE '\\')");
            let pattern = format!("%{}%", escape_like(s));
            values.push(pattern.clone().into());
            values.push(pattern.into());
        }
        if let Some(rating) = q.min_rating {
            if !(0..=5).contains(&rating) {
                return Err(invalid("minRating must be 0...5"));
            }
            filter.push_str(" AND a.rating>=?");
            values.push(rating.into());
        }
        for (value, column) in [(&q.flag_state, "a.flag"), (&q.color_label, "a.color")] {
            if let Some(value) = value {
                filter.push_str(&format!(" AND {column}=?"));
                values.push(value.clone().into());
            }
        }
        if let Some(tag) = &q.tag {
            filter.push_str(
                " AND EXISTS(SELECT 1 FROM json_each(a.snapshot,'$.tags') WHERE value=?)",
            );
            values.push(tag.clone().into());
        }
        if let Some(path) = &q.directory {
            let (scope, bindings) = directory_scope(lib, path, q.recursive.unwrap_or(true));
            filter.push_str(&format!(" AND a.id IN (SELECT asset_id FROM ({scope}))"));
            values.extend(bindings);
        }
        if !q.show_hidden.unwrap_or(false) {
            hidden_filter(&mut filter, &mut values, lib, q.directory.as_deref());
        }
        let mut connection = self.read()?;
        let db = connection.transaction()?;
        let revision = crate::revisions::read(&db, lib, q.directory.as_deref())?;
        let is_updating = crate::revisions::is_updating(&db, lib, q.directory.as_deref())?;
        let total: i64 = db.query_row(
            &format!("SELECT count(*) FROM catalog_assets a WHERE {filter}"),
            rusqlite::params_from_iter(&values),
            |r| r.get(0),
        )?;
        if let Some((time, id)) = boundary {
            filter.push_str(&format!(
                " AND ({sort_time}<? OR ({sort_time}=? AND a.id>?))"
            ));
            values.extend([time.clone().into(), time.into(), id.into()]);
        }
        let mut statement=db.prepare(&format!("SELECT a.snapshot,{sort_time} AS sort_key,a.id FROM {from} WHERE {filter} ORDER BY {order} LIMIT ? OFFSET ?"))?;
        values.push(((limit + 1) as i64).into());
        values.push(offset.into());
        let rows = statement
            .query_map(rusqlite::params_from_iter(&values), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = rows.len() > limit;
        let mut items = Vec::new();
        let mut last_key = None;
        for (snapshot, time, id) in rows.into_iter().take(limit) {
            let mut asset = decorate(&db, lib, serde_json::from_str(&snapshot)?)?;
            if include_media {
                asset["_media"] = Self::media_snapshot_on(&db, lib, &id)?;
            }
            items.push(asset);
            last_key = Some((time, id));
        }
        let next_cursor = if has_more {
            Some(if let Some(prefix) = cursor_prefix {
                format!(
                    "{prefix}{}",
                    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&last_key)?)
                )
            } else {
                (offset + items.len() as i64).to_string()
            })
        } else {
            None
        };
        Ok(
            json!({"items":items,"total":total,"nextCursor":next_cursor,"revision":revision,"isUpdating":is_updating}),
        )
    }

    pub fn asset(&self, lib: &str, id: &str) -> Result<Value> {
        let mut connection = self.read()?;
        let db = connection.transaction()?;
        decorate(&db, lib, existing(&db, lib, id)?.ok_or_else(not_found)?)
    }
    pub fn counts(&self, lib: &str, show_hidden: bool) -> Result<Value> {
        let db = self.read()?;
        let mut filter = format!("a.library_id=? AND {LIVE_ASSET}");
        let mut values: Vec<rusqlite::types::Value> = vec![lib.to_string().into()];
        if !show_hidden {
            hidden_filter(&mut filter, &mut values, lib, None);
        }
        let result=db.query_row(&format!("SELECT count(*) FILTER(WHERE trashed=0),count(*) FILTER(WHERE trashed=1),count(*) FILTER(WHERE trashed=0 AND flag='picked') FROM catalog_assets a WHERE {filter}"),rusqlite::params_from_iter(&values),|r|Ok(json!({"all":r.get::<_,i64>(0)?,"trashed":r.get::<_,i64>(1)?,"picked":r.get::<_,i64>(2)?})))?;
        Ok(result)
    }
    pub fn directory_photo_counts(&self, lib: &str, paths: &[String]) -> Result<Vec<(i64, i64)>> {
        let mut connection = self.read()?;
        let db = connection.transaction()?;
        paths
            .iter()
            .map(|path| directory_photo_counts(&db, lib, path))
            .collect()
    }
    pub fn directories(&self, lib: &str) -> Result<Value> {
        let db = self.read()?;
        let mut statement = db.prepare(&format!("SELECT d.directory,count(*)
            FROM catalog_asset_directories d JOIN catalog_assets a ON a.library_id=d.library_id AND a.id=d.asset_id
            WHERE d.library_id=? AND {LIVE_ASSET} GROUP BY d.directory ORDER BY d.directory"))?;
        let directories = statement
            .query_map([lib], |r| {
                Ok(json!({"path":r.get::<_, String>(0)?,"count":r.get::<_, i64>(1)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"directories":directories}))
    }
    pub fn tag_library(&self, lib: &str) -> Result<Value> {
        let db = self.read()?;
        let tags = db
            .prepare("SELECT name FROM catalog_tags WHERE library_id=? ORDER BY name")?
            .query_map([lib], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"tags": tags}))
    }
    pub fn add_tags_to_library(&self, lib: &str, body: &Value) -> Result<Value> {
        let tags = body
            .get("tags")
            .and_then(Value::as_array)
            .filter(|tags| !tags.is_empty() && tags.len() <= 100)
            .ok_or_else(|| invalid("tags must contain 1...100 nonempty strings"))?;
        let tags = tags
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty() && tag.len() <= 128)
                    .ok_or_else(|| {
                        invalid("tags must contain nonempty strings of at most 128 bytes")
                    })
            })
            .collect::<Result<BTreeSet<_>>>()?;
        {
            let mut db = self.lock()?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for tag in tags {
                tx.execute(
                    "INSERT OR IGNORE INTO catalog_tags(library_id,name) VALUES(?,?)",
                    params![lib, tag],
                )?;
            }
            tx.commit()?;
        }
        self.tag_library(lib)
    }
    pub fn patch_asset(&self, lib: &str, id: &str, patch: &Value) -> Result<Value> {
        let fields = patch
            .as_object()
            .ok_or_else(|| invalid("metadata patch must be an object"))?;
        if fields.is_empty()
            || fields
                .keys()
                .any(|k| !["rating", "flagState", "colorLabel", "tags"].contains(&k.as_str()))
        {
            return Err(invalid("unsupported metadata patch"));
        }
        if fields
            .get("rating")
            .is_some_and(|v| !v.as_i64().is_some_and(|n| (0..=5).contains(&n)))
        {
            return Err(invalid("rating must be 0...5"));
        }
        if fields.get("flagState").is_some_and(|v| {
            !v.as_str()
                .is_some_and(|s| ["unflagged", "picked", "rejected"].contains(&s))
        }) {
            return Err(invalid("invalid flagState"));
        }
        if fields.get("colorLabel").is_some_and(|v| {
            !v.is_null()
                && !v
                    .as_str()
                    .is_some_and(|s| ["red", "yellow", "green", "blue", "purple"].contains(&s))
        }) {
            return Err(invalid("invalid colorLabel"));
        }
        if fields.get("tags").is_some_and(|v| {
            !v.as_array().is_some_and(|a| {
                a.len() <= 100
                    && a.iter()
                        .all(|v| v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 128))
            })
        }) {
            return Err(invalid(
                "tags must be an array of at most 100 nonempty strings",
            ));
        }
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut asset = existing(&tx, lib, id)?.ok_or_else(not_found)?;
        let old = asset.clone();
        for (key, value) in fields {
            if key == "tags" {
                let tags: BTreeSet<&str> = value
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                asset[key] = json!(tags);
            } else {
                asset[key] = value.clone();
            }
        }
        if asset != old {
            asset["updatedAt"] = json!(Utc::now().to_rfc3339());
            save(&tx, lib, &asset)?;
        }
        let a = decorate(&tx, lib, existing(&tx, lib, id)?.ok_or_else(not_found)?)?;
        crate::revisions::flush(&tx)?;
        tx.commit()?;
        Ok(a)
    }
    pub(crate) fn enqueue_recycled_asset_caches(&self, lib: &str, ids: &[String]) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in ids {
            let descriptors=tx.prepare("SELECT thumbnail FROM media_cache WHERE library_id=?1 AND asset_id=?2 UNION ALL SELECT browse_thumbnail FROM media_cache WHERE library_id=?1 AND asset_id=?2 UNION ALL SELECT standard FROM photo_edits WHERE library_id=?1 AND asset_id=?2 UNION ALL SELECT thumbnail FROM photo_edits WHERE library_id=?1 AND asset_id=?2 UNION ALL SELECT browse FROM photo_edits WHERE library_id=?1 AND asset_id=?2 UNION ALL SELECT json_object('objectRef',json_object('bucket',object_bucket,'key',object_key)) FROM derivative_objects WHERE library_id=?1 AND asset_id=?2 UNION ALL SELECT o.value FROM photo_edit_requests r,json_each(r.payload,'$.body.outputs') o WHERE r.library_id=?1 AND r.asset_id=?2 AND json_extract(r.payload,'$.action')='commit'")?.query_map(params![lib,id],|r|r.get::<_,Option<String>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for descriptor in descriptors.into_iter().flatten() {
                let value: Value = serde_json::from_str(&descriptor)?;
                if !value["objectRef"].is_null() {
                    crate::cache_gc::enqueue(
                        &tx,
                        lib,
                        id,
                        &value["objectRef"],
                        &Value::Null,
                        Utc::now().timestamp(),
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn purge_recycled_assets(&self, lib: &str, ids: &[String]) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in ids {
            let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM catalog_physical_files WHERE library_id=?1 AND asset_id=?2)",params![lib,id],|r|r.get(0))?;
            ensure!(!live, invalid("仍有可用原片路径，不能清理照片资料"));
            for table in [
                "catalog_files",
                "catalog_versions",
                "catalog_version_path_history",
                "catalog_defaults",
                "catalog_identity_roots",
                "catalog_identity_aliases",
                "media_cache",
                "derivative_objects",
                "photo_edits",
                "photo_edit_requests",
                "photo_edit_decisions",
                "remote_cache_tasks",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE library_id=?1 AND asset_id=?2"),
                    params![lib, id],
                )?;
            }
            for table in ["photos", "videos", "catalog_assets"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE library_id=?1 AND id=?2"),
                    params![lib, id],
                )?;
            }
        }
        crate::revisions::flush(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn trash_rejected_ids(&self, lib: &str, ids: &[String]) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in ids {
            let mut asset = existing(&tx, lib, id)?.ok_or_else(not_found)?;
            ensure!(
                asset["flagState"] == "rejected",
                invalid("照片已不再是弃用状态，已停止删除")
            );
            if asset["trashed"] != true {
                asset["trashed"] = json!(true);
                asset["updatedAt"] = json!(Utc::now().to_rfc3339());
                save(&tx, lib, &asset)?;
            }
        }
        crate::revisions::flush(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn set_trashed(&self, lib: &str, id: &str, trashed: bool) -> Result<Value> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut asset = existing(&tx, lib, id)?.ok_or_else(not_found)?;
        if asset["trashed"] != trashed {
            asset["trashed"] = json!(trashed);
            asset["updatedAt"] = json!(Utc::now().to_rfc3339());
            save(&tx, lib, &asset)?;
        }
        let a = decorate(&tx, lib, existing(&tx, lib, id)?.ok_or_else(not_found)?)?;
        crate::revisions::flush(&tx)?;
        tx.commit()?;
        Ok(a)
    }
}
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl Store {
    pub fn root_id(&self, lib: &str, asset: &str) -> Result<String> {
        Ok(self.lock()?.query_row(
            "SELECT root_id FROM catalog_identity_roots WHERE library_id=? AND asset_id=?",
            params![lib, asset],
            |r| r.get(0),
        )?)
    }
    pub fn existing_asset_for_path(&self, lib: &str, path: &str) -> Result<Option<String>> {
        Ok(self
            .lock()?
            .query_row(
                "SELECT asset_id FROM catalog_paths WHERE library_id=? AND path=?",
                params![lib, path],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn with_identity_update<F>(
        &self,
        lib: &str,
        path: &str,
        expected_hash: &str,
        action: F,
    ) -> Result<bool>
    where
        F: FnOnce() -> Result<()>,
    {
        // Source mutation precedes the database lock; media claims use the same
        // order, while ordinary catalog reads and edits never wait for file I/O.
        let _source = self
            .source_mutation
            .write()
            .map_err(|_| anyhow::anyhow!("source mutation lock poisoned"))?;
        let indexed: Option<(String, String)> = {
            let db = self.lock()?;
            let indexed: Option<(String, String)> = db
                .query_row(
                    "SELECT asset_id,content_hash FROM catalog_paths WHERE library_id=? AND path=?",
                    params![lib, path],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((_, hash)) = &indexed {
                ensure!(
                    hash == expected_hash,
                    "identity update source changed in catalog"
                );
            }
            indexed
        };
        action()?;
        let db = self.lock()?;
        let current: Option<(String, String)> = db
            .query_row(
                "SELECT asset_id,content_hash FROM catalog_paths WHERE library_id=? AND path=?",
                params![lib, path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        ensure!(
            current == indexed,
            "identity update source changed in catalog during sidecar update"
        );
        Ok(true)
    }
    #[cfg(test)]
    pub(crate) fn publish_identity_sidecar(
        &self,
        lib: &str,
        asset: &str,
        media: &std::path::Path,
        sidecar: &std::path::Path,
        batch_id: Option<&str>,
    ) -> Result<()> {
        let path = sidecar.to_str().context("XMP path must be UTF-8")?;
        let media_path = media.to_str().context("media path must be UTF-8")?;
        ensure!(
            media.parent() == sidecar.parent()
                && crate::media::sidecars(media)?.iter().any(|p| p == sidecar),
            invalid("XMP 必须与对应的原片同目录")
        );
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let identity = crate::identity::file_sidecar_photo_id(sidecar)?;
        let hash = crate::media::sha256_file(sidecar)?;
        register_sidecar(
            &tx,
            lib,
            asset,
            SidecarPublication {
                media: media_path,
                path,
                hash: &hash,
                identity: identity.as_deref(),
                authorized_import: batch_id.is_some(),
            },
        )?;
        crate::revisions::flush_for_batch(&tx, batch_id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn original_is_online(&self, lib: &str, path: &str, hash: &str) -> Result<bool> {
        Ok(self.lock()?.query_row("SELECT EXISTS(SELECT 1 FROM catalog_paths p JOIN catalog_files f ON f.library_id=p.library_id AND f.asset_id=p.asset_id AND f.content_hash=p.content_hash AND f.role=p.role WHERE p.library_id=? AND p.path=? AND p.content_hash=? AND f.holder='keeps-nas' AND f.availability='online')",params![lib,path,hash],|r|r.get(0))?)
    }
    pub fn ingest_original(
        &self,
        lib: &str,
        path: &str,
        file: &Value,
        snapshot: &Value,
    ) -> Result<String> {
        self.ingest_original_with_evidence(lib, path, file, snapshot, &Default::default())
    }
    pub fn ingest_original_with_evidence(
        &self,
        lib: &str,
        path: &str,
        file: &Value,
        snapshot: &Value,
        evidence: &crate::versions::VersionEvidence,
    ) -> Result<String> {
        self.ingest_original_with_revision(lib, path, file, snapshot, evidence, None)
    }
    pub(crate) fn ingest_original_with_revision(
        &self,
        lib: &str,
        path: &str,
        file: &Value,
        snapshot: &Value,
        evidence: &crate::versions::VersionEvidence,
        batch_id: Option<&str>,
    ) -> Result<String> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = ingest_original_in_transaction(&tx, lib, path, file, snapshot, evidence)?;
        crate::revisions::flush_for_batch(&tx, batch_id)?;
        tx.commit()?;
        Ok(id)
    }
    pub fn declare_generated_preview(&self, lib: &str, id: &str, derivative: &Value) -> Result<()> {
        self.declare_preview_with_revision(lib, id, None, derivative, None)
            .map(|_| ())
    }
    pub fn declare_generated_preview_for_version(
        &self,
        lib: &str,
        id: &str,
        hash: &str,
        derivative: &Value,
    ) -> Result<bool> {
        self.declare_preview_with_revision(lib, id, Some(hash), derivative, None)
    }
    pub(crate) fn declare_preview_with_revision(
        &self,
        lib: &str,
        id: &str,
        hash: Option<&str>,
        derivative: &Value,
        batch_id: Option<&str>,
    ) -> Result<bool> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(hash) = hash {
            let selected: Option<String> = tx
                .query_row(
                    "SELECT content_hash FROM catalog_defaults WHERE library_id=? AND asset_id=?",
                    params![lib, id],
                    |r| r.get(0),
                )
                .optional()?;
            if selected.as_deref() != Some(hash) {
                return Ok(false);
            }
        }
        let current:Option<String>=tx.query_row("SELECT json_object('fileObject',json(file_object),'bucket',object_bucket,'key',object_key,'eTag',object_etag,'width',pixel_width,'height',pixel_height) FROM derivative_objects WHERE library_id=? AND asset_id=? AND role='preview'",params![lib,id],|r|r.get(0)).optional()?;
        let current = current
            .map(|value| serde_json::from_str::<Value>(&value))
            .transpose()?;
        let next = json!({"fileObject":derivative["fileObject"],"bucket":derivative["objectRef"]["bucket"],"key":derivative["objectRef"]["key"],"eTag":derivative["objectRef"]["eTag"],"width":derivative["pixelSize"]["width"],"height":derivative["pixelSize"]["height"]});
        if current.as_ref() != Some(&next) {
            register_derivative(&tx, lib, id, derivative)?;
        }

        crate::revisions::flush_for_batch(&tx, batch_id)?;
        tx.commit()?;
        Ok(true)
    }
    pub fn mark_missing_under(&self, lib: &str, directory: &str) -> Result<usize> {
        self.mark_missing_under_with_revision(lib, directory, None)
    }
    pub(crate) fn mark_missing_under_with_revision(
        &self,
        lib: &str,
        directory: &str,
        batch_id: Option<&str>,
    ) -> Result<usize> {
        self.mark_missing_scope_with_revision(lib, directory, "recursive", batch_id)
    }
    pub(crate) fn mark_missing_scope_with_revision(
        &self,
        lib: &str,
        directory: &str,
        scope: &str,
        batch_id: Option<&str>,
    ) -> Result<usize> {
        let mut db = self.lock()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let files = {
            let query = missing_paths_query(scope)?;
            let mut statement = tx.prepare(&query)?;
            let directory = directory.trim_end_matches('/');
            let bindings = if scope == "file" {
                vec![lib.to_owned(), directory.to_owned()]
            } else {
                vec![
                    lib.to_owned(),
                    format!("{directory}/"),
                    format!("{directory}0"),
                ]
            };
            statement
                .query_map(rusqlite::params_from_iter(bindings), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut missing = 0;
        let mut changed_directories = BTreeSet::new();
        for (id, path, hash, size, role) in files {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("check original {path}")),
            }
            if role == "sidecar" {
                crate::physical_files::withdraw(&tx, lib, &path)?;
                continue;
            }
            if let Some(parent) = std::path::Path::new(&path).parent() {
                changed_directories.insert(parent.to_string_lossy().into_owned());
            }
            crate::versions::missing_path(&tx, lib, &id, &path)?;
            let other:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM catalog_paths WHERE library_id=? AND asset_id=? AND content_hash=? AND role=?)",params![lib,id,hash,role],|r|r.get(0))?;
            if other {
                continue;
            }
            if let Some(size) = size {
                let file = json!({"contentHash":hash,"sizeBytes":size,"role":role});
                register_file(&tx, lib, &id, &file, "missing")?;
            }
            missing += 1;
        }
        if scope == "recursive" {
            crate::navigation::withdraw_missing(&tx, lib, std::path::Path::new(directory))?;
        }
        for directory in changed_directories {
            crate::photo_relations::reconcile_directory(&tx, lib, &directory)?;
        }
        crate::revisions::flush_for_batch(&tx, batch_id)?;
        tx.commit()?;
        Ok(missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_twenty_one_adds_operation_checkpoints_without_fabricating_success() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("db");
        let store = Store::open(&path, true)?;
        store.lock()?.execute_batch("ALTER TABLE media_cache DROP COLUMN thumbnail_operation_id; ALTER TABLE media_cache DROP COLUMN browse_operation_id; INSERT INTO media_cache(library_id,asset_id,source_hash,spec,status,thumbnail,standard) VALUES('lib','ready','hash','spec','ready','{}','{}'); PRAGMA user_version=21;")?;
        drop(store);
        assert!(Store::open(&path, false).is_err());
        let store = Store::open(&path, true)?;
        let row: (String, Option<String>, Option<String>) = store.lock()?.query_row(
            "SELECT status,thumbnail_operation_id,browse_operation_id FROM media_cache",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(row, ("ready".into(), None, None));
        drop(store);
        Store::open(&path, false)?;
        Ok(())
    }

    #[test]
    fn schema_twenty_media_publication_migrates_existing_results() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("catalog.sqlite");
        let store = Store::open(&path, true)?;
        {
            let db = store.lock()?;
            db.execute_batch("DROP TRIGGER media_cache_published_update; DROP TRIGGER media_cache_published_insert; ALTER TABLE media_cache DROP COLUMN published_source_hash; ALTER TABLE media_cache DROP COLUMN published_spec; PRAGMA user_version=20;")?;
            db.execute("INSERT INTO media_cache(library_id,asset_id,source_hash,spec,status,thumbnail,standard) VALUES('lib','ready','source','spec','ready','{}','{}'),('lib','failed','source','spec','failed','{}','{}')", [])?;
        }
        drop(store);
        assert!(Store::open(&path, false).is_err());
        let store = Store::open(&path, true)?;
        {
            let db = store.lock()?;
            assert_eq!(
                db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
                VERSION
            );
            let published: (String,String) = db.query_row("SELECT published_source_hash,published_spec FROM media_cache WHERE asset_id='ready'", [], |r| Ok((r.get(0)?,r.get(1)?)))?;
            assert_eq!(published, ("source".into(), "spec".into()));
            assert!(
                db.query_row(
                    "SELECT published_source_hash FROM media_cache WHERE asset_id='failed'",
                    [],
                    |r| r.get::<_, Option<String>>(0)
                )?
                .is_none()
            );
            db.execute("UPDATE media_cache SET source_hash='new',spec='new-spec',status='ready' WHERE asset_id='failed'", [])?;
            assert_eq!(
                db.query_row(
                    "SELECT published_source_hash FROM media_cache WHERE asset_id='failed'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "new"
            );
        }
        drop(store);
        let store = Store::open(&path, false)?;
        store.lock()?.execute_batch("DROP TRIGGER media_cache_published_update; DROP TRIGGER media_cache_published_insert; ALTER TABLE media_cache DROP COLUMN published_source_hash;")?;
        drop(store);
        assert!(Store::open(&path, false).is_err());
        Ok(())
    }

    #[test]
    fn legacy_ai_state_survives_reopen_and_schema_twelve_migration() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("catalog.sqlite");
        let store = Store::open(&path, true)?;
        store.lock()?.execute(
            "INSERT INTO ai_editing_state VALUES ('lib','workspace',7,'saved draft')",
            [],
        )?;
        drop(store);
        let store = Store::open(&path, false)?;
        let content: String = store.lock()?.query_row(
            "SELECT content FROM ai_editing_state WHERE library_id='lib' AND kind='workspace'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(content, "saved draft");
        store
            .lock()?
            .execute_batch("DROP TABLE ai_editing_state; PRAGMA user_version=12;")?;
        drop(store);
        assert!(Store::open(&path, false).is_err());
        let store = Store::open(&path, true)?;
        let count: i64 =
            store
                .lock()?
                .query_row("SELECT COUNT(*) FROM ai_editing_state", [], |row| {
                    row.get(0)
                })?;
        assert_eq!(count, 0);
        Ok(())
    }
    #[test]
    fn tag_library_migrates_existing_tags_and_retains_removed_tags() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("catalog.sqlite");
        let store = Store::open(&path, true)?;
        let id = store.ingest_original(
            "lib",
            "/photo.jpg",
            &json!({"contentHash":"photo-hash","sizeBytes":1,"role":"jpeg_original"}),
            &snapshot(),
        )?;
        store.patch_asset("lib", &id, &json!({"tags":["旅行"]}))?;
        store.lock()?.execute_batch("DROP TRIGGER catalog_tags_insert; DROP TRIGGER catalog_tags_update; DROP TABLE catalog_tags; PRAGMA user_version=16;")?;
        drop(store);
        assert!(Store::open(&path, false).is_err());
        let store = Store::open(&path, true)?;
        assert_eq!(store.tag_library("lib")?, json!({"tags":["旅行"]}));
        store.patch_asset("lib", &id, &json!({"tags":["风景"]}))?;
        store.add_tags_to_library("lib", &json!({"tags":[" 人像 ","人像"]}))?;
        assert!(
            store
                .add_tags_to_library("lib", &json!({"tags":["valid", " "]}))
                .is_err()
        );
        assert_eq!(
            store.tag_library("lib")?,
            json!({"tags":["人像","旅行","风景"]})
        );
        assert_eq!(store.tag_library("other")?, json!({"tags":[]}));
        drop(store);
        assert_eq!(
            Store::open(&path, false)?.tag_library("lib")?,
            json!({"tags":["人像","旅行","风景"]})
        );
        Ok(())
    }
    fn snapshot() -> Value {
        json!({"assetID":Uuid::new_v4(),"captureTime":"2024-01-01T00:00:00Z","cameraMake":"Canon","cameraModel":"R3","lensModel":"50mm","originalFilename":"photo.jpg","contentFingerprint":"photo-hash","metadataFingerprint":"2024|Canon|R3|photo","rating":1,"flagState":"unflagged","tags":[],"createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z"})
    }
    #[test]
    fn document_and_original_document_ids_link_in_both_import_orders() -> Result<()> {
        for export_first in [false, true] {
            let tmp = tempfile::tempdir()?;
            let store = Store::open(&tmp.path().join("catalog.sqlite"), true)?;
            let document = Uuid::new_v4().to_string();
            let mut raw = snapshot();
            raw["documentID"] = json!(format!(
                "xmp.did:{}",
                document.replace('-', "").to_uppercase()
            ));
            raw["originalFilename"] = json!("capture.3FR");
            let mut export = snapshot();
            export["originalDocumentID"] = json!(format!("urn:uuid:{document}"));
            export["documentID"] = json!(Uuid::new_v4().to_string());
            export["originalFilename"] = json!("renamed.jpg");
            let raw_file = json!({"contentHash":"raw-hash","sizeBytes":42,"role":"raw_original"});
            let export_file =
                json!({"contentHash":"export-hash","sizeBytes":12,"role":"jpeg_original"});
            let inputs = [
                ("/album/capture.3FR", &raw_file, &raw),
                ("/album/renamed.jpg", &export_file, &export),
            ];
            let order = if export_first { [1, 0] } else { [0, 1] };
            let first = inputs[order[0]];
            let second = inputs[order[1]];
            let id = store.ingest_original("lib", first.0, first.1, first.2)?;
            assert_eq!(
                store.ingest_original("lib", second.0, second.1, second.2)?,
                id
            );
            assert_eq!(store.lock()?.query_row(
                "SELECT count(DISTINCT asset_id) FROM catalog_photo_sources WHERE library_id='lib' AND kind='software' AND value=?",
                [&document], |row| row.get::<_, i64>(0))?, 1);
        }
        Ok(())
    }

    #[test]
    fn distinct_document_ids_preserve_camera_and_filename_pairing() -> Result<()> {
        for camera in [false, true] {
            let tmp = tempfile::tempdir()?;
            let store = Store::open(&tmp.path().join("catalog.sqlite"), true)?;
            let mut raw = snapshot();
            raw["originalFilename"] = json!("pair.3FR");
            raw["documentID"] = json!(Uuid::new_v4().to_string());
            if camera {
                raw["cameraImageID"] = json!("same-capture");
            }
            let mut export = raw.clone();
            export["originalFilename"] = json!(if camera { "renamed.heic" } else { "pair.heic" });
            export["documentID"] = json!(Uuid::new_v4().to_string());
            let file = json!({"contentHash":"raw","sizeBytes":42,"role":"raw_original"});
            let id = store.ingest_original("lib", "/photos/pair.3FR", &file, &raw)?;
            let path = if camera {
                "/photos/renamed.heic"
            } else {
                "/photos/pair.heic"
            };
            assert_eq!(
                store.ingest_original(
                    "lib",
                    path,
                    &json!({"contentHash":"heic","sizeBytes":12,"role":"jpeg_original"}),
                    &export
                )?,
                id
            );
        }
        Ok(())
    }

    #[test]
    fn photo_identity_uses_ids_and_camera_pairing_without_hash_dedup() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("catalog.sqlite"), true)?;
        let file = json!({"contentHash":"same-hash","sizeBytes":42,"role":"raw_original"});
        let mut a = snapshot();
        a["photoID"] = json!(Uuid::new_v4().to_string());
        let first = store.ingest_original("lib", "/photos/a.3FR", &file, &a)?;
        let mut b = a.clone();
        b["photoID"] = json!(Uuid::new_v4().to_string());
        let second = store.ingest_original("lib", "/photos/b.3FR", &file, &b)?;
        assert_ne!(first, second);
        let mut raw = snapshot();
        raw["cameraImageID"] = json!("camera-pair");
        raw["originalFilename"] = json!("pair.3FR");
        let r = store.ingest_original("lib", "/photos/pair.3FR", &file, &raw)?;
        let mut heif = raw.clone();
        heif["originalFilename"] = json!("renamed.heif");
        heif["originalDocumentID"] = json!(Uuid::new_v4().to_string());
        let h = store.ingest_original(
            "lib",
            "/photos/renamed.heif",
            &json!({"contentHash":"other-hash","sizeBytes":12,"role":"jpeg_original"}),
            &heif,
        )?;
        assert_eq!(r, h);
        let software = Uuid::new_v4().to_string();
        let decorated = format!("xmp.did:{}", software.replace('-', "").to_uppercase());
        let mut export = heif.clone();
        export["originalDocumentID"] = json!(decorated);
        store.ingest_original("lib", "/photos/decorated.heif", &file, &export)?;
        export.as_object_mut().unwrap().remove("cameraImageID");
        export["originalFilename"] = json!("third-party-export.jpg");
        export["originalDocumentID"] = json!(format!("urn:uuid:{software}"));
        assert_eq!(
            store.ingest_original("lib", "/photos/third-party-export.jpg", &file, &export)?,
            r
        );
        assert_eq!(store.lock()?.query_row("SELECT count(*) FROM catalog_photo_sources WHERE library_id='lib' AND kind='software' AND value=? AND asset_id=?",params![software,r],|row|row.get::<_,i64>(0))?,1);

        let generated = Uuid::parse_str(&r)?;
        assert_eq!(generated.get_version_num(), 7);
        assert_eq!(
            &generated.as_bytes()[..6],
            &1704067200000_u64.to_be_bytes()[2..]
        );
        let mut unknown = snapshot();
        unknown.as_object_mut().unwrap().remove("captureTime");
        unknown["originalFilename"] = json!("unknown.jpg");
        let id = store.ingest_original("lib", "/photos/unknown.jpg", &file, &unknown)?;
        assert!(store.lock()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM catalog_missing_capture_dates WHERE asset_id=?)",
            [&id],
            |r| r.get::<_, bool>(0)
        )?);
        let mut dated = unknown.clone();
        dated["photoID"] = json!(id);
        dated["captureTime"] = json!("2024-03-01T00:00:00Z");
        store.ingest_original("lib", "/photos/dated.heif", &file, &dated)?;
        store.ingest_original("lib", "/photos/unknown.jpg", &file, &unknown)?;
        assert!(!store.lock()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM catalog_missing_capture_dates WHERE asset_id=?)",
            [&id],
            |r| r.get::<_, bool>(0)
        )?);
        let mut pair = snapshot();
        pair["originalFilename"] = json!("newpair.3FR");
        let paired = store.ingest_original("lib", "/photos/newpair.3FR", &file, &pair)?;
        pair["originalFilename"] = json!("newpair.HEIC");
        assert_eq!(
            store.ingest_original("lib", "/photos/newpair.HEIC", &file, &pair)?,
            paired
        );
        Ok(())
    }
    #[test]
    fn capture_cursor_survives_inserts_removed_boundary_and_time_ties() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(&directory.path().join("catalog.sqlite"), true)?;
        let insert = |id: &str, time: &str| -> Result<()> {
            let mut asset = snapshot();
            asset["id"] = json!(id);
            asset["captureTime"] = json!(time);
            store.lock()?.execute(
                "INSERT INTO catalog_assets(library_id,id,snapshot,content_hash,fingerprint,sort_time,filename,rating,flag,color,trashed) VALUES ('lib',?,?, 'hash','fingerprint',?,'photo.jpg',1,'unflagged',NULL,0)",
                params![id, asset.to_string(), time],
            )?;
            store.lock()?.execute(
                "INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role) VALUES('lib',?,?,'hash','jpeg_original')",
                params![format!("/photos/{id}.jpg"), id],
            )?;
            Ok(())
        };
        for (id, time) in [
            ("a", "2024-03"),
            ("b", "2024-02"),
            ("c", "2024-02"),
            ("d", "2024-01"),
        ] {
            insert(id, time)?;
        }
        let mut query = AssetQuery {
            limit: Some(2),
            ..Default::default()
        };
        let first = store.query_assets("lib", &query)?;
        assert_eq!(first["items"][0]["id"], "a");
        assert_eq!(first["items"][1]["id"], "b");
        query.cursor = first["nextCursor"].as_str().map(str::to_owned);
        insert("new", "2024-04")?;
        let after_insert = store.query_assets("lib", &query)?;
        assert_eq!(after_insert["items"][0]["id"], "c");
        assert_eq!(after_insert["items"][1]["id"], "d");
        // Removing both the boundary and an earlier result must not skip older photos.
        store
            .lock()?
            .execute_batch("DELETE FROM catalog_physical_files WHERE asset_id IN ('a','b'); DELETE FROM catalog_assets WHERE id IN ('a','b');")?;
        let second = store.query_assets("lib", &query)?;
        assert_eq!(second["total"], 3);
        assert_eq!(second["items"].as_array().unwrap().len(), 2);
        assert_eq!(second["items"][0]["id"], "c");
        assert_eq!(second["items"][1]["id"], "d");
        assert!(second["nextCursor"].is_null());
        for cursor in ["-1", "bad", "cd1.invalid", "cd1.bnVsbA"] {
            query.cursor = Some(cursor.into());
            assert!(store.query_assets("lib", &query).is_err());
        }
        query.cursor = Some("1".into());
        query.limit = Some(1);
        let migrated = store.query_assets("lib", &query)?;
        assert_eq!(migrated["items"][0]["id"], "c");
        assert!(migrated["nextCursor"].as_str().unwrap().starts_with("cd1."));
        query.cursor = migrated["nextCursor"].as_str().map(str::to_owned);
        assert_eq!(store.query_assets("lib", &query)?["items"][0]["id"], "d");
        query.limit = Some(2);
        query.cursor = None;
        query.sort = Some("capture_asc".into());
        let ascending = store.query_assets("lib", &query)?;
        assert_eq!(ascending["nextCursor"], "2");
        query.cursor = Some("2".into());
        assert_eq!(store.query_assets("lib", &query)?["items"][0]["id"], "new");
        Ok(())
    }
    #[test]
    fn missing_reconciliation_respects_file_and_shallow_directory_scope() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let paths = [
            dir.path().join("first.jpg"),
            dir.path().join("second.jpg"),
            dir.path().join("child/nested.jpg"),
        ];
        for (index, path) in paths.iter().enumerate() {
            let hash = format!("hash-{index}");
            let file = json!({"contentHash":hash,"sizeBytes":5,"role":"jpeg_original"});
            let mut snap = snapshot();
            snap["contentFingerprint"] = json!(hash);
            store.ingest_original("lib", path.to_str().unwrap(), &file, &snap)?;
        }
        assert_eq!(
            store.mark_missing_scope_with_revision(
                "lib",
                paths[0].to_str().unwrap(),
                "file",
                None
            )?,
            1
        );
        assert_eq!(
            store.mark_missing_scope_with_revision(
                "lib",
                dir.path().to_str().unwrap(),
                "directory",
                None
            )?,
            1
        );
        assert_eq!(
            store.mark_missing_scope_with_revision(
                "lib",
                dir.path().to_str().unwrap(),
                "recursive",
                None
            )?,
            1
        );
        Ok(())
    }

    #[test]
    fn trusted_root_keeps_previously_issued_photo_and_annotations() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        std::fs::write(&a, b"first")?;
        std::fs::write(&b, b"second")?;
        let file = json!({"contentHash":"photo-hash","sizeBytes":5,"role":"jpeg_original"});
        let first = store.ingest_original("lib", a.to_str().unwrap(), &file, &snapshot())?;
        let other = json!({"contentHash":"second-hash","sizeBytes":6,"role":"jpeg_original"});
        let mut snap = snapshot();
        snap["contentFingerprint"] = json!("second-hash");
        snap["originalFilename"] = json!("b.jpg");
        snap["rating"] = json!(5);
        let second = store.ingest_original("lib", b.to_str().unwrap(), &other, &snap)?;
        snap["photoID"] = json!(first);
        assert_eq!(
            store.ingest_original("lib", b.to_str().unwrap(), &other, &snap)?,
            first
        );
        let db = store.lock()?;
        assert_eq!(existing(&db, "lib", &second)?.unwrap()["rating"], 5);
        assert_ne!(existing(&db, "lib", &first)?.unwrap()["rating"], 5);
        assert_eq!(
            db.query_row(
                "SELECT asset_id FROM catalog_identity_aliases WHERE root_id=?",
                [&second],
                |r| r.get::<_, String>(0)
            )?,
            second
        );
        Ok(())
    }
    #[test]
    fn root_identity_joins_versions_and_preserves_existing_asset() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        std::fs::write(&a, b"first")?;
        std::fs::write(&b, b"edited")?;
        let file = json!({"contentHash":"photo-hash","sizeBytes":5,"role":"jpeg_original"});
        let id = store.ingest_original("lib", a.to_str().unwrap(), &file, &snapshot())?;
        assert_eq!(store.root_id("lib", &id)?, id);
        let root = Uuid::new_v4().to_string();
        let mut snap = snapshot();
        snap["photoID"] = json!(id);
        snap["originalDocumentID"] = json!(root);
        assert_eq!(
            store.ingest_original("lib", a.to_str().unwrap(), &file, &snap)?,
            id
        );
        assert_eq!(store.root_id("lib", &id)?, id);
        let edited = json!({"contentHash":"edited-hash","sizeBytes":6,"role":"jpeg_original"});
        assert_eq!(
            store.ingest_original("lib", b.to_str().unwrap(), &edited, &snap)?,
            id
        );
        assert_eq!(
            store
                .lock()?
                .query_row("SELECT count(*) FROM catalog_paths", [], |r| r
                    .get::<_, i64>(0))?,
            2
        );
        std::fs::remove_file(&a)?;
        store.mark_missing_under("lib", dir.path().to_str().unwrap())?;
        store.ingest_original("lib", b.to_str().unwrap(), &edited, &snap)?;
        assert_eq!(
            store
                .lock()?
                .query_row("SELECT count(*) FROM catalog_paths", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        Ok(())
    }
    #[test]
    fn unknown_identity_on_replaced_path_does_not_steal_existing_photo() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"first")?;
        let file = json!({"contentHash":"first-hash","sizeBytes":5,"role":"jpeg_original"});
        let mut first = snapshot();
        first["rating"] = json!(5);
        let original = store.ingest_original("lib", path.to_str().unwrap(), &file, &first)?;
        let root = Uuid::new_v4().to_string();
        let mut replacement = snapshot();
        replacement["originalDocumentID"] = json!(root);
        replacement["contentFingerprint"] = json!("other-hash");
        let other = json!({"contentHash":"other-hash","sizeBytes":5,"role":"jpeg_original"});
        std::fs::write(&path, b"other")?;
        let replaced =
            store.ingest_original("lib", path.to_str().unwrap(), &other, &replacement)?;
        assert_ne!(original, replaced);
        assert_eq!(store.root_id("lib", &original)?, original);
        assert_eq!(store.root_id("lib", &replaced)?, root);
        let db = store.lock()?;
        assert_eq!(existing(&db, "lib", &original)?.unwrap()["rating"], 5);
        assert_ne!(existing(&db, "lib", &replaced)?.unwrap()["rating"], 5);
        Ok(())
    }

    #[test]
    fn exact_copy_with_distinct_ids_stays_separate() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let original_root = Uuid::new_v4().to_string();
        let alias = Uuid::new_v4().to_string();
        let file = json!({"contentHash":"same-hash","sizeBytes":5,"role":"jpeg_original"});
        let mut snap = snapshot();
        snap["originalDocumentID"] = json!(original_root);
        let id = store.ingest_original("lib", "/album/first.jpg", &file, &snap)?;
        snap["originalDocumentID"] = json!(alias);
        let second = store.ingest_original("lib", "/album/copy.jpg", &file, &snap)?;
        assert_ne!(second, id);
        assert_eq!(store.root_id("lib", &id)?, original_root);
        let edited = json!({"contentHash":"edited-hash","sizeBytes":6,"role":"jpeg_original"});
        assert_eq!(
            store.ingest_original("lib", "/album/edited.jpg", &edited, &snap)?,
            second
        );
        assert_eq!(store.root_id("lib", &id)?, original_root);
        Ok(())
    }

    #[test]
    fn sidecar_update_checks_catalog_path_before_and_after_io() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let file = json!({"contentHash":"photo-hash","sizeBytes":5,"role":"jpeg_original"});
        store.ingest_original("lib", "/a.jpg", &file, &snapshot())?;
        assert!(
            store
                .with_identity_update("lib", "/a.jpg", "stale-hash", || panic!(
                    "stale source must not write"
                ))
                .is_err()
        );
        let result = store.with_identity_update("lib", "/a.jpg", "photo-hash", || {
            store.lock()?.execute("UPDATE catalog_physical_files SET content_hash='changed' WHERE library_id='lib' AND path='/a.jpg'", [])?;
            Ok(())
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("during sidecar update")
        );
        Ok(())
    }

    #[test]
    fn sidecar_update_preserves_all_original_hashes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut shared = snapshot();
        shared["photoID"] = json!(Uuid::new_v4().to_string());
        let store = Store::open(&dir.path().join("db"), true)?;
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        std::fs::write(&a, b"first")?;
        std::fs::write(&b, b"first")?;
        let file = json!({"contentHash":"photo-hash","sizeBytes":5,"role":"jpeg_original"});
        let id = store.ingest_original("lib", a.to_str().unwrap(), &file, &shared)?;
        assert_eq!(
            store.ingest_original("lib", b.to_str().unwrap(), &file, &shared)?,
            id
        );
        store.with_identity_update("lib", a.to_str().unwrap(), "photo-hash", || Ok(()))?;
        let db = store.lock()?;
        let old: String = db.query_row(
            "SELECT content_hash FROM catalog_paths WHERE path=?",
            [b.to_str().unwrap()],
            |r| r.get(0),
        )?;
        assert_eq!(old, "photo-hash");
        assert_eq!(
            db.query_row("SELECT count(*) FROM catalog_versions", [], |r| r
                .get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM catalog_files WHERE content_hash='photo-hash'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        Ok(())
    }
    #[test]
    fn sidecar_update_preserves_cache_hashes_and_allows_processing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let file = json!({"contentHash":"photo-hash","sizeBytes":5,"role":"jpeg_original"});
        let id = store.ingest_original("lib", "/a.jpg", &file, &snapshot())?;
        store.lock()?.execute("INSERT INTO media_cache(library_id,asset_id,source_hash,spec,status,thumbnail,standard) VALUES('lib',?,'photo-hash','test','ready','{}',?)",params![id,json!({"path":"/a.jpg","version":"photo-hash","sizeBytes":5,"mtimeNs":"1"}).to_string()])?;
        for (task, path, completed) in [
            ("done", "/a.jpg", 1),
            ("other", "/copy.jpg", 1),
            ("cancelled", "/a.jpg", -1),
        ] {
            store.lock()?.execute("INSERT INTO remote_cache_tasks VALUES(?,'lib',?,'photo-hash','photo-hash',?,'worker',0,?)",params![task,id,path,completed])?;
        }
        assert!(store.with_identity_update("lib", "/a.jpg", "photo-hash", || Ok(()))?);
        let result: (String, String, String) = store.lock()?.query_row(
            "SELECT status,source_hash,json_extract(standard,'$.version') FROM media_cache",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(
            result,
            ("ready".into(), "photo-hash".into(), "photo-hash".into())
        );
        let tasks = {
            let db = store.lock()?;
            let mut q =
                db.prepare("SELECT id,source_hash,input_hash FROM remote_cache_tasks ORDER BY id")?;
            q.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        assert_eq!(
            tasks,
            vec![
                ("cancelled".into(), "photo-hash".into(), "photo-hash".into()),
                ("done".into(), "photo-hash".into(), "photo-hash".into()),
                ("other".into(), "photo-hash".into(), "photo-hash".into())
            ]
        );

        store
            .lock()?
            .execute("UPDATE media_cache SET status='processing'", [])?;
        assert!(store.with_identity_update("lib", "/a.jpg", "photo-hash", || Ok(()))?);
        Ok(())
    }
    #[test]
    fn single_location_respects_hidden_directory_and_v1_migration_preserves_assets() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("db");
        let store = Store::open(&path, true)?;
        let file = json!({"contentHash":"photo-hash","sizeBytes":100,"role":"jpeg_original"});
        let asset = snapshot();
        let id = store.ingest_original("lib", "/private/photo.jpg", &file, &asset)?;
        assert!(store.lock()?.execute(
            "INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role) VALUES('lib','/public/photo.jpg',?,'photo-hash','jpeg_original')",
            [&id],
        ).is_err());
        store.set_hidden_directory("lib", "/private", true)?;
        let all = store.query_assets(
            "lib",
            &AssetQuery {
                show_hidden: Some(true),
                ..Default::default()
            },
        )?;
        assert_eq!(all["items"][0]["paths"], json!(["/private/photo.jpg"]));
        assert_eq!(store.asset("lib", &id)?["paths"], all["items"][0]["paths"]);
        assert_eq!(
            store.query_assets(
                "lib",
                &AssetQuery {
                    directory: Some("/public".into()),
                    ..Default::default()
                }
            )?["total"],
            0
        );
        assert_eq!(
            store.query_assets(
                "lib",
                &AssetQuery {
                    directory: Some("/private".into()),
                    ..Default::default()
                }
            )?["total"],
            1
        );
        store.set_hidden_directory("lib", "/", true)?;
        assert_eq!(
            store.query_assets("lib", &AssetQuery::default())?["total"],
            0
        );
        assert_eq!(
            store.query_assets(
                "lib",
                &AssetQuery {
                    directory: Some("/private".into()),
                    ..Default::default()
                }
            )?["total"],
            1
        );
        drop(store);
        let db = Connection::open(&path)?;
        crate::revisions::remove_schema(&db)?;
        crate::edits::remove_schema(&db)?;
        crate::physical_files::restore_legacy_path_tables(&db)?;
        db.execute_batch("DROP TABLE catalog_identity_roots; DROP TABLE catalog_identity_aliases; DROP TABLE catalog_hidden_directories; DROP TABLE catalog_versions; DROP TABLE catalog_version_paths; DROP TABLE catalog_defaults; PRAGMA user_version=1;")?;
        drop(db);
        assert!(Store::open(&path, false).is_err());
        let store = Store::open(&path, true)?;
        assert_eq!(store.counts("lib", false)?["all"], 1);
        assert_eq!(store.hidden_directories("lib")?, json!({"paths":[]}));
        drop(store);
        Store::open(&path, false)?;
        Ok(())
    }
    #[test]
    fn nas_commands_projection_and_idempotency() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db"), true).unwrap();
        let file = json!({"contentHash":"photo-hash","sizeBytes":100,"role":"jpeg_original"});
        let id = store
            .ingest_original("lib", "/photos/photo.jpg", &file, &snapshot())
            .unwrap();
        assert_eq!(
            store
                .ingest_original("lib", "/photos/photo.jpg", &file, &snapshot())
                .unwrap(),
            id
        );
        assert_eq!(store.counts("lib", false).unwrap()["all"], 1);
        let result = store
            .patch_asset(
                "lib",
                &id,
                &json!({"rating":5,"flagState":"picked","tags":["旅行","北京"],"colorLabel":"red"}),
            )
            .unwrap();
        assert_eq!(result["rating"], 5);
        let before = store.asset("lib", &id).unwrap();
        store.patch_asset("lib", &id, &json!({"rating":5})).unwrap();
        assert_eq!(store.asset("lib", &id).unwrap(), before);
        let q = AssetQuery {
            tag: Some("北京".into()),
            min_rating: Some(4),
            ..Default::default()
        };
        assert_eq!(store.query_assets("lib", &q).unwrap()["total"], 1);
        store.set_trashed("lib", &id, true).unwrap();
        assert_eq!(store.counts("lib", false).unwrap()["trashed"], 1);
        assert_eq!(
            store.query_assets("lib", &AssetQuery::default()).unwrap()["total"],
            0
        );
        store.set_trashed("lib", &id, false).unwrap();
        assert!(store.patch_asset("lib", &id, &json!({"rating":6})).is_err());
        assert_eq!(store.asset("lib", &id).unwrap()["rating"], 5);
    }
    #[test]
    fn known_legacy_original_indexes_without_overwriting_imported_holder() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut shared = snapshot();
        shared["photoID"] = json!(Uuid::new_v4().to_string());
        let old = dir.path().join("old/photo.jpg");
        let new = old.clone();
        std::fs::create_dir_all(old.parent().unwrap())?;
        std::fs::create_dir_all(new.parent().unwrap())?;
        std::fs::write(&old, b"same")?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let file = json!({"contentHash":"same","sizeBytes":4,"role":"jpeg_original"});
        let id = store.ingest_original("lib", old.to_str().unwrap(), &file, &shared)?;
        store
            .lock()?
            .execute("UPDATE catalog_files SET holder='legacy-mac'", [])?;
        store
            .lock()?
            .execute("UPDATE catalog_physical_files SET versioned=0", [])?;
        assert_eq!(
            store.ingest_original("lib", new.to_str().unwrap(), &file, &shared)?,
            id
        );
        assert!(store.original_is_online("lib", new.to_str().unwrap(), "same")?);
        assert_eq!(
            store.lock()?.query_row(
                "SELECT count(*) FROM catalog_files WHERE holder='legacy-mac'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(store.counts("lib", false)?["all"], 1);
        Ok(())
    }
    #[test]
    fn cross_directory_move_preserves_identity_and_default() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut shared = snapshot();
        shared["photoID"] = json!(Uuid::new_v4().to_string());
        let old = dir.path().join("old/photo.jpg");
        let new = dir.path().join("new/photo.jpg");
        std::fs::create_dir_all(old.parent().unwrap())?;
        std::fs::create_dir_all(new.parent().unwrap())?;
        std::fs::write(&old, b"same")?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let file = json!({"contentHash":"same","sizeBytes":4,"role":"jpeg_original"});
        let id = store.ingest_original("lib", old.to_str().unwrap(), &file, &shared)?;
        store.patch_asset("lib", &id, &json!({"rating":5,"tags":["keep"]}))?;
        store.set_default_version("lib", &id, "same")?;
        store.declare_generated_preview("lib", &id, &json!({"role":"preview","fileObject":{},"objectRef":{"bucket":"local","key":"preview"},"pixelSize":{"width":10,"height":10}}))?;
        std::fs::rename(&old, &new)?;
        assert!(
            store
                .ingest_original("lib", new.to_str().unwrap(), &file, &shared)
                .is_err()
        );
        {
            let mut db = store.lock()?;
            let tx = db.transaction()?;
            crate::navigation::record(&tx, "lib", new.parent().unwrap())?;
            tx.execute("UPDATE catalog_asset_directories SET directory=? WHERE library_id='lib' AND asset_id=?", params![new.parent().unwrap().to_str(), id])?;
            tx.execute(
                "UPDATE catalog_physical_files SET path=? WHERE library_id='lib' AND path=?",
                params![new.to_str(), old.to_str()],
            )?;
            tx.commit()?;
        }
        assert_eq!(
            store.default_version("lib", &id)?.unwrap().path,
            new.to_str().unwrap()
        );
        assert_eq!(store.asset("lib", &id)?["rating"], 5);
        assert_eq!(store.asset("lib", &id)?["tags"], json!(["keep"]));
        assert!(!store.original_is_online("lib", old.to_str().unwrap(), "same")?);
        assert!(store.original_is_online("lib", new.to_str().unwrap(), "same")?);
        let db = store.lock()?;
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM derivative_objects WHERE asset_id=? AND role='preview'",
                params![id],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT user_selected FROM catalog_defaults WHERE asset_id=?",
                params![id],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        Ok(())
    }

    #[test]
    fn cross_directory_live_copies_and_ambiguous_moves_stay_separate() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let file = json!({"contentHash":"same","sizeBytes":4,"role":"jpeg_original"});
        let mut ids = Vec::new();
        let mut paths = Vec::new();
        for name in ["a", "b"] {
            let path = dir.path().join(name).join("photo.jpg");
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, b"same")?;
            ids.push(store.ingest_original("lib", path.to_str().unwrap(), &file, &snapshot())?);
            paths.push(path);
        }
        assert_ne!(ids[0], ids[1]);
        for path in paths {
            std::fs::remove_file(path)?;
        }
        let new = dir.path().join("c/photo.jpg");
        std::fs::create_dir_all(new.parent().unwrap())?;
        std::fs::write(&new, b"same")?;
        let id = store.ingest_original("lib", new.to_str().unwrap(), &file, &snapshot())?;
        assert!(!ids.contains(&id));
        assert_eq!(store.counts("lib", false)?["all"], 3);
        Ok(())
    }
    #[test]
    fn duplicate_paths_and_missing_restore_keep_one_asset() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut shared = snapshot();
        shared["photoID"] = json!(Uuid::new_v4().to_string());
        let photos = dir.path().join("photos");
        std::fs::create_dir(&photos)?;
        let a = photos.join("one.jpg");
        let b = photos.join("two.jpg");
        std::fs::write(&a, b"same")?;
        std::fs::write(&b, b"same")?;
        let store = Store::open(&dir.path().join("db"), true)?;
        let file = json!({"contentHash":"same","sizeBytes":4,"role":"jpeg_original"});
        let id = store.ingest_original("lib", a.to_str().unwrap(), &file, &shared)?;
        assert_eq!(
            store.ingest_original("lib", b.to_str().unwrap(), &file, &shared)?,
            id
        );
        assert!(store.original_is_online("lib", a.to_str().unwrap(), "same")?);
        std::fs::remove_file(&a)?;
        assert_eq!(
            store.mark_missing_under("lib", photos.to_str().unwrap())?,
            0
        );
        assert!(store.original_is_online("lib", b.to_str().unwrap(), "same")?);
        std::fs::remove_file(&b)?;
        assert_eq!(
            store.mark_missing_under("lib", photos.to_str().unwrap())?,
            1
        );
        assert_eq!(store.asset("lib", &id)?["paths"], json!([]));
        assert_eq!(
            store.query_assets("lib", &AssetQuery::default())?["items"],
            json!([])
        );
        assert_eq!(store.counts("lib", false)?["all"], 0);
        std::fs::write(&a, b"same")?;
        assert_eq!(
            store.ingest_original("lib", a.to_str().unwrap(), &file, &shared)?,
            id
        );
        assert!(store.original_is_online("lib", a.to_str().unwrap(), "same")?);
        assert_eq!(store.counts("lib", false)?["all"], 1);
        Ok(())
    }
}

#[cfg(test)]
mod directory_plan_tests {
    use super::*;

    #[test]
    fn missing_scope_queries_use_exact_or_bounded_path_index() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(&directory.path().join("catalog.sqlite"), true)?;
        let db = store.lock()?;
        for scope in ["file", "directory", "recursive"] {
            let bindings = if scope == "file" {
                vec!["lib", "/photo/one.jpg"]
            } else {
                vec!["lib", "/photo/", "/photo0"]
            };
            let mut query = db.prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                missing_paths_query(scope)?
            ))?;
            let plan = query
                .query_map(rusqlite::params_from_iter(bindings), |row| {
                    row.get::<_, String>(3)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let paths = plan
                .iter()
                .find(|step| step.contains("SEARCH p "))
                .expect("catalog path index search");
            assert!(paths.contains("library_id=?"), "{scope}: {plan:?}");
            if scope == "file" {
                assert!(paths.contains("path=?"), "{scope}: {plan:?}");
            } else {
                assert!(
                    paths.contains("path>?") && paths.contains("path<?"),
                    "{scope}: {plan:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn ordered_asset_paths_use_asset_lookup() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        ensure_schema(&mut db, true)?;
        let plan = db
            .prepare(&format!("EXPLAIN QUERY PLAN {ASSET_PATHS}"))?
            .query_map(params!["photos", "asset"], |row| row.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            plan.iter()
                .any(|step| step.contains("catalog_physical_files_asset")
                    && step.contains("library_id=? AND asset_id=?")),
            "{plan:?}"
        );
        Ok(())
    }

    #[test]
    fn directory_asset_set_is_not_correlated_per_catalog_asset() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        ensure_schema(&mut db, true)?;
        for recursive in [false, true] {
            let (scope, bindings) = directory_scope("photos", "/photo", recursive);
            let mut query = db.prepare(&format!("EXPLAIN QUERY PLAN SELECT count(*) FROM catalog_assets a WHERE a.library_id='photos' AND a.trashed=0 AND a.id IN (SELECT asset_id FROM ({scope}))"))?;
            let plan = query
                .query_map(rusqlite::params_from_iter(bindings), |row| {
                    row.get::<_, String>(3)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert!(
                plan.iter()
                    .any(|step| step.contains("catalog_asset_directories_directory")
                        && step.contains("directory=?")),
                "{plan:?}"
            );
            assert!(
                plan.iter().any(|step| step.contains("LIST SUBQUERY")),
                "{plan:?}"
            );
            if recursive {
                assert!(
                    plan.iter()
                        .any(|step| step.contains("directory>?") && step.contains("directory<?")),
                    "{plan:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn directory_reads_match_schema_25_across_path_boundaries_and_withdrawals() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("catalog.sqlite");
        let store = Store::open(&path, true)?;
        let (before, revision) = {
            let mut connection = store.lock()?;
            let db = connection.transaction()?;
            db.execute_batch(
                "DROP INDEX catalog_asset_directories_directory; PRAGMA user_version=25;",
            )?;
            for (id, file, role, trash) in [
                ("root", "/root.jpg", "jpeg_original", 0),
                ("direct", "/照片/a.jpg", "jpeg_original", 0),
                ("child", "/照片/子目录/b.jpg", "jpeg_original", 0),
                ("sibling", "/照片-/c.jpg", "jpeg_original", 0),
                ("trash", "/照片/trash.jpg", "jpeg_original", 1),
                ("sidecar", "/照片/only.xmp", "sidecar", 0),
                ("missing", "/照片/missing.jpg", "jpeg_original", 0),
            ] {
                db.execute("INSERT INTO catalog_assets(library_id,id,snapshot,content_hash,fingerprint,sort_time,filename,rating,flag,trashed) VALUES('lib',?1,json_object('id',?1),'hash','','2026','photo.jpg',0,'unflagged',?2)", params![id,trash])?;
                crate::physical_files::publish(
                    &db,
                    "lib",
                    id,
                    file,
                    "hash",
                    role,
                    id == "missing",
                )?;
            }
            crate::physical_files::publish(
                &db,
                "lib",
                "direct",
                "/照片/a.raw",
                "raw",
                "raw_original",
                true,
            )?;
            crate::physical_files::withdraw(&db, "lib", "/照片/missing.jpg")?;
            let before = db.prepare("SELECT asset_id FROM catalog_asset_locations WHERE library_id='lib' ORDER BY asset_id")?
                .query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            crate::revisions::flush(&db)?;
            let revision = crate::revisions::read(&db, "lib", None)?;
            db.commit()?;
            (before, revision)
        };
        drop(store);
        let store = Store::open(&path, true)?;
        let db = store.read()?;
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
            VERSION
        );
        assert!(
            db.prepare("PRAGMA foreign_key_check")?
                .query([])?
                .next()?
                .is_none()
        );
        let visible = db.prepare(&format!("SELECT a.id FROM catalog_assets a WHERE library_id='lib' AND {LIVE_ASSET} ORDER BY id"))?
            .query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(visible, before);
        assert_eq!(directory_photo_counts(&db, "lib", "/")?, (1, 4));
        assert_eq!(directory_photo_counts(&db, "lib", "/照片/")?, (1, 2));
        assert_eq!(directory_photo_counts(&db, "lib", "/not-found")?, (0, 0));
        assert_eq!(crate::revisions::read(&db, "lib", None)?, revision);
        drop(db);
        store.set_hidden_directory("lib", "/照片", true)?;
        store.set_hidden_directory("lib", "/照片/子目录", true)?;
        let query = |directory: Option<&str>, recursive| AssetQuery {
            directory: directory.map(str::to_owned),
            recursive: Some(recursive),
            ..Default::default()
        };
        let page = store.query_assets("lib", &query(Some("/照片/"), true))?;
        assert_eq!(page["total"], 1);
        assert_eq!(page["items"][0]["id"], "direct");
        assert_eq!(
            page["items"][0]["paths"],
            json!(["/照片/a.jpg", "/照片/a.raw"])
        );
        assert_eq!(store.query_assets("lib", &query(None, true))?["total"], 2);
        assert_eq!(
            store.query_assets("lib", &query(Some("/"), false))?["total"],
            1
        );
        assert_eq!(
            store.query_assets("lib", &query(Some("/照片/子目录"), false))?["total"],
            1
        );
        let with_media = store.query_assets_with_media("lib", &query(Some("/照片"), false))?;
        assert_eq!(
            with_media["items"][0]["_media"],
            store.media_snapshot("lib", "direct")?
        );
        Ok(())
    }
}
