use crate::{jobs::Jobs, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use std::{collections::BTreeSet, os::unix::fs::MetadataExt, path::PathBuf};

pub fn register(jobs: &Jobs, store: &Store, library: &str, paths: &[PathBuf]) -> Result<()> {
    ensure!(
        !paths.is_empty(),
        "provide the exact restored original paths"
    );
    crate::directory_move::ensure_reconciled(jobs)?;
    crate::directory_trash::ensure_reconciled(jobs)?;
    ensure!(
        !jobs.pending_file_publication(library, None)?,
        "finish pending publications before recovery"
    );
    let roots = jobs.folders()?;
    let mut db = store.lock()?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut affected = BTreeSet::new();
    for path in paths {
        ensure!(
            path.is_absolute() && jobs.validate_scan_path(path, "file")? == *path,
            "restored source must have a canonical regular file path: {}",
            path.display()
        );
        ensure!(
            roots
                .iter()
                .any(|root| root.library_id == library && path.starts_with(&root.path)),
            "restored source is outside this library's saved roots: {}",
            path.display()
        );
        let text = path.to_str().context("restored path must be UTF-8")?;
        let identity: Option<(String,String,String)> = tx.query_row(
            "SELECT p.asset_id,p.content_hash,i.root_id FROM (
                SELECT asset_id,content_hash FROM catalog_version_path_history WHERE library_id=?1 AND path=?2
                UNION ALL
                SELECT asset_id,content_hash FROM catalog_physical_files WHERE library_id=?1 AND path=?2 AND role<>'sidecar'
            ) p JOIN catalog_identity_roots i ON i.library_id=?1 AND i.asset_id=p.asset_id",
            params![library,text], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).optional()?;
        let (asset, hash, root) =
            identity.with_context(|| format!("no matching historical version: {text}"))?;
        ensure!(
            crate::media::sha256_file(path)? == hash,
            "restored bytes do not match the historic version: {text}"
        );
        let info = path.symlink_metadata()?;
        let size = info.len();
        let mtime = i64::try_from(
            info.modified()?
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
        )?;

        let role: String = tx.query_row(
            "SELECT role FROM catalog_files WHERE library_id=?1 AND asset_id=?2 AND content_hash=?3
               AND holder='keeps-nas' AND role<>'sidecar' ORDER BY role LIMIT 1",
            params![library,asset,hash], |r|r.get(0),
        ).with_context(|| format!("historical version has no NAS file record: {text}"))?;
        let occupied: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM catalog_physical_files WHERE path=?2 AND library_id<>?1)",
            params![library, text],
            |r| r.get(0),
        )?;
        ensure!(
            !occupied,
            "restored path is registered in another library: {text}"
        );

        let mut valid_sidecar = false;
        let mut associated = Vec::new();
        let mut seen = BTreeSet::new();
        for sidecar in crate::media::sidecars(path)? {
            let metadata = sidecar.symlink_metadata()?;
            if !seen.insert((metadata.dev(), metadata.ino())) {
                continue;
            }
            let id = crate::identity::file_sidecar_photo_id(&sidecar)?;
            if let Some(id) = &id {
                ensure!(
                    id == &root,
                    "restored XMP has a different photo identity: {}",
                    sidecar.display()
                );
                valid_sidecar = true;
            }
            associated.push((sidecar, id));
        }
        ensure!(valid_sidecar, "restored photo has no matching XMP: {text}");
        crate::physical_files::publish(&tx, library, &asset, text, &hash, &role, true)?;
        crate::catalog::register_file(
            &tx,
            library,
            &asset,
            &json!({"contentHash":hash,"sizeBytes":size,"role":role}),
            "online",
        )?;
        let stamp = crate::media::sidecar_paths_stamp(
            &associated
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
        )?;
        for (sidecar, id) in associated {
            let path = sidecar
                .to_str()
                .context("restored XMP path must be UTF-8")?;
            if id.is_some() {
                let hash = crate::media::sha256_file(&sidecar)?;
                crate::catalog::register_sidecar(
                    &tx,
                    library,
                    &asset,
                    crate::catalog::SidecarPublication {
                        media: text,
                        path,
                        hash: &hash,
                        identity: id.as_deref(),
                        authorized_import: false,
                    },
                )?;
            }
        }
        Jobs::record_catalog_check(
            &tx,
            library,
            text,
            &crate::jobs::FileState {
                size: i64::try_from(size)?,
                mtime_ns: mtime,
                asset_id: asset.clone(),
                version: hash,
                error: None,
                metadata_stamp: stamp,
                metadata_operation_id: None,
            },
            true,
        )?;
        affected.insert(asset);
    }
    for asset in affected {
        crate::versions::choose(&tx, library, &asset)?;
    }
    crate::revisions::flush(&tx)?;
    tx.commit()?;
    drop(db);
    store.reconcile_cache()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::Arc};

    #[test]
    fn offline_restore_reuses_historical_identity_and_rejects_wrong_bytes_or_xmp() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let originals = fixture.path().canonicalize()?.join("originals");
        let directory = originals.join("holiday");
        fs::create_dir_all(&directory)?;
        let path = directory.join("photo.jpg");
        let bytes = b"fixture original";
        fs::write(&path, bytes)?;
        let hash = crate::media::sha256_file(&path)?;
        let store = Arc::new(Store::open(&fixture.path().join("catalog.sqlite"), true)?);
        let jobs = Jobs::open_managed(
            &fixture.path().join("jobs.sqlite"),
            &originals,
            store.clone(),
        )?;
        jobs.add_folder("library", ".")?;
        let id = store.ingest_original("library", path.to_str().unwrap(),
            &json!({"contentHash":hash,"sizeBytes":bytes.len(),"role":"jpeg_original"}),
            &json!({"originalFilename":"photo.jpg","contentFingerprint":hash,"metadataFingerprint":"fixture","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","flagState":"picked","rating":4,"tags":["keep"]}),
        )?;
        let root = store.root_id("library", &id)?;
        let xmp = crate::identity::ensure_known(&path, &root)?.path;
        let xmp_bytes = fs::read(&xmp)?;
        store.publish_identity_sidecar("library", &id, &path, &xmp, None)?;
        let removed = fixture.path().join("recycled");
        fs::rename(&directory, &removed)?;
        store.mark_missing_under("library", directory.to_str().unwrap())?;
        assert_eq!(store.counts("library", false)?["all"], 0);
        assert!(!crate::navigation::registered(
            &store.read()?,
            "library",
            &directory
        )?);
        fs::rename(&removed, &directory)?;
        fs::write(&path, b"wrong bytes")?;
        assert!(register(&jobs, &store, "library", std::slice::from_ref(&path)).is_err());
        fs::write(&path, bytes)?;
        fs::write(
            &xmp,
            format!(
                r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/"><rdf:Description xmp:Identifier="https://keeps.photos/photo/{}"/></rdf:RDF>"#,
                uuid::Uuid::new_v4()
            ),
        )?;
        assert!(register(&jobs, &store, "library", std::slice::from_ref(&path)).is_err());
        assert_eq!(store.counts("library", false)?["all"], 0);
        fs::write(&xmp, &xmp_bytes)?;
        register(&jobs, &store, "library", std::slice::from_ref(&path))?;
        let revision = store.library_revision("library")?;
        register(&jobs, &store, "library", std::slice::from_ref(&path))?;
        assert_eq!(store.library_revision("library")?, revision);
        assert_eq!(store.root_id("library", &id)?, root);
        assert_eq!(store.asset("library", &id)?["rating"], 4);
        assert_eq!(store.asset("library", &id)?["tags"], json!(["keep"]));
        assert!(crate::navigation::registered(
            &store.read()?,
            "library",
            &directory
        )?);
        assert_eq!(fs::read(path)?, bytes);
        assert_eq!(fs::read(xmp)?, xmp_bytes);
        Ok(())
    }
}
