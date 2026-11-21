use anyhow::{Context, Result};
use keeps_server::{jobs::Jobs, store::Store};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command, sync::Arc};

fn copy_database(source: &Path, target: &Path) -> Result<()> {
    let source = Connection::open(source)?;
    let mut target = Connection::open(target)?;
    rusqlite::backup::Backup::new(&source, &mut target)?.run_to_completion(
        128,
        std::time::Duration::ZERO,
        None,
    )?;
    Ok(())
}

#[test]
fn verified_inventory_migrates_paired_databases_and_can_be_retried_or_rolled_back() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let originals = root.join("photos");
    let empty = originals.join("空目录");
    fs::create_dir_all(&empty)?;
    let media = originals.join("photo.jpg");
    let xmp = originals.join("photo.xmp");
    let original_bytes = b"sample original";
    fs::write(&media, original_bytes)?;
    let digest = format!("{:x}", Sha256::digest(original_bytes));
    let identity = "a98d1952-980d-4867-a85d-1c9a8b3a6103";
    let xmp_bytes = format!(
        r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/"><rdf:Description xmp:Identifier="https://keeps.photos/photo/{identity}"/></rdf:RDF>"#
    );
    fs::write(&xmp, &xmp_bytes)?;

    let legacy = root.join("legacy");
    fs::create_dir(&legacy)?;
    let source = legacy.join("control_plane.sqlite");
    let task_source = legacy.join("jobs.sqlite");
    let store = Store::open(&source, true)?;
    let asset = store.ingest_original(
        "photos", media.to_str().unwrap(),
        &json!({"contentHash":digest,"sizeBytes":original_bytes.len(),"role":"jpeg_original"}),
        &json!({"photoID":identity,"originalFilename":"photo.jpg","contentFingerprint":digest,"metadataFingerprint":"fixture","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","rating":4,"flagState":"picked","tags":["fixture"]}),
    )?;
    let folder_id;
    let exact_id;
    {
        let jobs = Jobs::open(&task_source, &originals)?;
        let folder = jobs.add_folder("photos", ".")?;
        folder_id = folder.id;
        let inactive = jobs.add_folder("photos", empty.to_str().unwrap())?;
        jobs.remove_folder(&inactive.id)?;
        let time = fs::metadata(&media)?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        jobs.record_file(
            &folder_id,
            media.to_str().unwrap(),
            original_bytes.len().try_into()?,
            time.try_into()?,
            &asset,
            &digest,
            None,
        )?;
        exact_id = jobs.enqueue_manual_file(&folder_id, &media)?.id;
    }
    drop(store);
    {
        let db = Connection::open(&source)?;
        db.execute(
            "UPDATE catalog_identity_roots SET root_id=? WHERE library_id='photos'",
            [identity],
        )?;
        db.execute("INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role) VALUES('photos',?,?,?,'sidecar')",
            params![xmp.to_str().unwrap(),asset,format!("{:x}",Sha256::digest(xmp_bytes.as_bytes()))])?;
    }
    drop(Store::open(&source, false)?);
    {
        let db = Connection::open(&source)?;
        db.execute_batch("DROP TRIGGER directories_added; DROP TRIGGER directories_removed; DROP TRIGGER directories_moved;
            DROP TABLE folders; DROP TABLE directory_inventory_pending; DROP TABLE directory_creations;
            PRAGMA user_version=26;")?;
    }
    {
        let db = Connection::open(&task_source)?;
        db.execute("INSERT INTO jobs(id,folder_id,status,scope_path,scope_kind,work_class,checkpoint) VALUES('legacy-scan',?,'running',?,'recursive','manual','old cursor')",
            params![folder_id, originals.to_str().unwrap()])?;
    }
    let state = root.join("state");
    fs::create_dir_all(state.join("db"))?;
    let catalog = state.join("db/control_plane.sqlite");
    let tasks = state.join("db/jobs.sqlite");
    copy_database(&source, &catalog)?;
    copy_database(&task_source, &tasks)?;

    let manifest = root.join("inventory.json");
    let bad_manifest = root.join("stale.json");
    let audit_script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/audit_photo_placement.py");
    let audit = Command::new("python3")
        .arg(audit_script)
        .arg("--catalog")
        .arg(&catalog)
        .args(["--library", "photos", "--root"])
        .arg(&originals)
        .arg("--directory-manifest")
        .output()?;
    assert!(
        audit.status.success(),
        "{}",
        String::from_utf8_lossy(&audit.stdout)
    );
    let report: Value = serde_json::from_slice(&audit.stdout)?;
    assert_eq!(
        report["directoryManifest"]["directories"],
        json!([originals, empty])
    );
    fs::write(&manifest, &audit.stdout)?;
    let mut stale = report;
    stale["directoryManifest"]["catalogRevision"] = json!(-1);
    fs::write(&bad_manifest, serde_json::to_vec(&stale)?)?;

    let migrate = |path: &Path| {
        Command::new(env!("CARGO_BIN_EXE_keeps-server"))
            .arg("migrate")
            .arg(path)
            .env("KEEPS_ROOT", &state)
            .env("ORIGINAL_ROOT", &originals)
            .output()
    };
    let failed = migrate(&bad_manifest)?;
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr)
            .contains("catalog revision changed since inventory")
    );
    let blocked = Store::open(&catalog, false)?;
    assert!(blocked.require_directory_inventory().is_err());
    drop(blocked);

    let migrated = migrate(&manifest)?;
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let store = Arc::new(Store::open(&catalog, false)?);
    store.require_directory_inventory()?;
    let jobs = Jobs::open_managed(&tasks, &originals, store.clone())?;
    let roots = jobs.folders()?;
    assert_eq!(
        roots.iter().find(|f| f.active).context("active root")?.id,
        folder_id
    );
    assert_eq!(roots.iter().filter(|f| !f.active).count(), 1);
    assert_eq!(
        jobs.job("legacy-scan")?.context("legacy job")?.status,
        "cancelled"
    );
    assert_eq!(
        jobs.claim_photo_processing()?.context("exact job")?.id,
        exact_id
    );
    assert_eq!(
        jobs.file_state(&folder_id, media.to_str().unwrap())?
            .context("migrated file check")?
            .version,
        digest
    );
    assert_eq!(store.asset("photos", &asset)?["rating"], 4);
    assert_eq!(fs::read(&media)?, original_bytes);
    assert_eq!(fs::read_to_string(&xmp)?, xmp_bytes);
    drop(jobs);
    drop(store);
    let db = Connection::open(&catalog)?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM directories WHERE library_id='photos' AND path=?",
            [empty.to_str().unwrap()],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    assert!(
        db.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none()
    );
    drop(db);
    let db = Connection::open(&tasks)?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name IN ('folders','files')",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    drop(db);

    let rollback = root.join("rollback");
    fs::create_dir(&rollback)?;
    copy_database(&source, &rollback.join("control_plane.sqlite"))?;
    copy_database(&task_source, &rollback.join("jobs.sqlite"))?;
    let restored = Connection::open(rollback.join("control_plane.sqlite"))?;
    assert_eq!(
        restored.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        26
    );
    let restored_tasks = Jobs::open(&rollback.join("jobs.sqlite"), &originals)?;
    assert_eq!(
        restored_tasks
            .job("legacy-scan")?
            .context("restored scan")?
            .status,
        "pending"
    );
    assert_eq!(
        restored_tasks
            .job(&exact_id)?
            .context("restored exact job")?
            .folder_id,
        folder_id
    );
    Ok(())
}
