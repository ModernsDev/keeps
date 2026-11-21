use crate::{
    jobs::{Job, Jobs},
    media::{self, MediaProcessor},
    previews::PreviewStorage,
    store::Store,
    versions::VersionEvidence,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::UNIX_EPOCH,
};

pub fn run(
    store: Arc<Store>,
    jobs: Arc<Jobs>,
    previews: Arc<PreviewStorage>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    MediaProcessor::new()
        .probe()
        .context("NAS media runtime unavailable")?;
    while !stop.load(Ordering::Relaxed) {
        if let Some(job) = jobs.claim_photo_processing()? {
            run_claimed(&store, &jobs, &previews, &job, &stop)?;
        } else {
            jobs.wait_for_scan_work(jobs.scan_wait_duration()?);
        }
    }
    Ok(())
}

fn run_claimed(
    store: &Store,
    jobs: &Jobs,
    previews: &PreviewStorage,
    job: &Job,
    stop: &AtomicBool,
) -> Result<()> {
    let parent = Path::new(&job.path)
        .parent()
        .and_then(Path::to_str)
        .context("photo work requires a parent directory")?;
    store.note_revision_activity(&job.library_id, &job.id, parent)?;
    jobs.set_phase(&job.id, "processing")?;
    let mut result = run_one(store, jobs, previews, job, stop);
    let terminal = !matches!(result, Ok(false))
        || stop.load(Ordering::Relaxed)
        || jobs
            .job(&job.id)?
            .is_some_and(|job| !matches!(job.status.as_str(), "pending" | "running"));
    if terminal {
        let publication = (|| -> Result<()> {
            if matches!(result, Ok(true)) && !stop.load(Ordering::Relaxed) {
                jobs.set_phase(&job.id, "publishing")?;
            }
            store.finish_revision_batch(&job.id, false)
        })();
        if let Err(error) = publication {
            result = Err(match result {
                Err(original) => original.context(format!("finish photo revision: {error:#}")),
                Ok(_) => error,
            });
        }
    }
    if stop.load(Ordering::Relaxed) {
        return result.map(|_| ());
    }
    match result {
        Ok(true) => {
            jobs.finish(&job.id, None)?;
        }
        Ok(false) => {
            if jobs.job(&job.id)?.is_some_and(|j| j.status == "running") {
                jobs.yield_job(&job.id)?;
            }
        }
        Err(error) => {
            let trace = format!("{error:#}");
            tracing::error!(job_id=%job.id,error=%trace,"NAS photo task failed");
            jobs.finish(&job.id, Some(&trace))?;
        }
    }
    if jobs
        .job(&job.id)?
        .is_some_and(|j| !matches!(j.status.as_str(), "pending" | "running"))
    {
        store.finish_revision_batch(&job.id, false)?;
    }
    Ok(())
}

/// Startup-only finalization of owners whose recorded tasks are no longer active.
pub fn maintain_revisions(store: &Store, jobs: &Jobs) -> Result<()> {
    for id in store.revision_batch_ids()? {
        match jobs.job(&id)? {
            Some(job) if matches!(job.status.as_str(), "pending" | "running") => {}
            Some(job) if job.status == "completed" => store.finish_revision_batch(&id, false)?,
            _ => store.finish_revision_batch(&id, false)?,
        }
    }
    store.lock()?.execute(
        "UPDATE catalog_revision_updates SET last_changed_at=0 WHERE owner=''",
        [],
    )?;
    store.settle_revision_updates()?;
    Ok(())
}

#[derive(Default)]
struct Progress {
    processed: i64,
    skipped: i64,
    failed: i64,
}

pub fn run_one(
    store: &Store,
    jobs: &Jobs,
    previews: &PreviewStorage,
    job: &Job,
    stop: &AtomicBool,
) -> Result<bool> {
    let _directory_guard = jobs.directory_mutation.read().unwrap();
    crate::directory_move::ensure_reconciled(jobs)?;
    let current = jobs.job(&job.id)?.context("claimed job disappeared")?;
    if stopped(jobs, &current, stop)? {
        return Ok(false);
    }
    ensure!(
        current.scope_kind == "file",
        "directory scanning has been retired"
    );
    let path = jobs.validate_scan_path(Path::new(&current.path), "file")?;
    let mut progress = Progress::default();
    process_entry(store, jobs, previews, &current, &path, &mut progress)?;
    publish(jobs, &current, &progress, None)?;
    Ok(true)
}

fn process_entry(
    store: &Store,
    jobs: &Jobs,
    previews: &PreviewStorage,
    job: &Job,
    path: &Path,
    progress: &mut Progress,
) -> Result<()> {
    publish(
        jobs,
        job,
        progress,
        Some(path.to_str().context("original path must be UTF-8")?),
    )?;
    let result = process_photo(store, jobs, previews, job, path);
    match result {
        Ok(true) => progress.processed += 1,
        Ok(false) => progress.skipped += 1,
        Err(error) => {
            let trace = format!("processing {}: {error:#}", path.display());
            tracing::error!(job_id = %job.id, error = %trace, "NAS photo processing failed");
            progress.failed += 1;
            publish(
                jobs,
                job,
                progress,
                Some(path.to_str().context("photo path must be UTF-8")?),
            )?;
            return Err(error).with_context(|| format!("processing {}", path.display()));
        }
    }
    Ok(())
}

fn stopped(jobs: &Jobs, job: &Job, stop: &AtomicBool) -> Result<bool> {
    Ok(stop.load(Ordering::Relaxed) || !jobs.is_active(&job.folder_id)?)
}
fn publish(jobs: &Jobs, job: &Job, progress: &Progress, path: Option<&str>) -> Result<()> {
    jobs.update_progress(
        &job.id,
        progress.processed,
        progress.skipped,
        progress.failed,
        path,
    )
}
fn file_stamp(path: &Path) -> Result<(i64, i64)> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "original is no longer a regular file: {}",
        path.display()
    );
    let size = i64::try_from(metadata.len()).context("original size exceeds supported range")?;
    let mtime = i64::try_from(metadata.modified()?.duration_since(UNIX_EPOCH)?.as_nanos())
        .context("original modification time exceeds supported range")?;
    Ok((size, mtime))
}

fn photo_paths(store: &Store, jobs: &Jobs, library: &str, path: &Path) -> Result<Vec<PathBuf>> {
    let parent = path.parent().context("photo has no directory")?;
    let stem = path
        .file_stem()
        .context("photo has no filename")?
        .to_string_lossy();
    let mut paths = vec![path.to_path_buf()];
    let parent = parent
        .to_str()
        .context("original directory must be UTF-8")?;
    let directory_prefix = format!("{}/", parent.trim_end_matches('/'));
    let directory_ceiling = format!("{}0", parent.trim_end_matches('/'));
    {
        let db = jobs.db.lock().unwrap();
        let mut query = db.prepare(
            "SELECT path FROM file_publications WHERE library_id=? AND path>=? AND path<?",
        )?;
        for entry in query.query_map(
            rusqlite::params![library, directory_prefix, directory_ceiling],
            |row| row.get::<_, String>(0),
        )? {
            let candidate = PathBuf::from(entry?);
            if candidate.parent() == Some(Path::new(parent))
                && (media::is_photo(&candidate) || media::is_video(&candidate))
                && candidate
                    .file_stem()
                    .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(&stem))
            {
                paths.push(candidate);
            }
        }
    }
    {
        let db = store.lock()?;
        let mut q=db.prepare("SELECT path FROM catalog_paths WHERE library_id=?1 AND asset_id IN (SELECT asset_id FROM catalog_paths WHERE library_id=?1 AND path=?2)")?;
        for item in q.query_map(rusqlite::params![library, path.to_str()], |r| {
            r.get::<_, String>(0)
        })? {
            let p = PathBuf::from(item?);
            if p.parent() == Some(Path::new(parent)) {
                paths.push(p);
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn managed_source_hash(
    store: &Store,
    jobs: &Jobs,
    library: &str,
    path: &Path,
) -> Result<Option<(String, bool)>> {
    use rusqlite::OptionalExtension;
    if let Some((hash, _)) = jobs.file_publication(library, path)? {
        return Ok(Some((hash, true)));
    }
    let path = path.to_str().context("original path must be UTF-8")?;
    let hash = store.read()?.query_row(
        "SELECT p.content_hash FROM catalog_paths p JOIN catalog_assets a ON a.library_id=p.library_id AND a.id=p.asset_id WHERE p.library_id=? AND p.path=? AND a.trashed=0",
        rusqlite::params![library,path], |r| r.get(0),
    ).optional()?;
    Ok(hash.map(|hash| (hash, false)))
}

fn managed_sidecars(
    store: &Store,
    jobs: &Jobs,
    library: &str,
    asset: Option<&str>,
    path: &Path,
) -> Result<Vec<PathBuf>> {
    use rusqlite::OptionalExtension;
    let catalog = store.read()?;
    let mut managed = Vec::new();
    for sidecar in media::sidecar_candidates(path) {
        let publication = jobs.file_publication(library, &sidecar)?;
        let hash: Option<String> = if let Some((hash, _)) = publication {
            Some(hash)
        } else {
            catalog.query_row(
                "SELECT content_hash FROM catalog_physical_files WHERE library_id=? AND asset_id=? AND path=? AND role='sidecar'",
                rusqlite::params![library,asset,sidecar.to_str()], |r| r.get(0),
            ).optional()?
        };
        if let Some(hash) = hash {
            ensure!(
                crate::imports::file_digest(jobs, library, &sidecar)? == hash,
                "XMP 在图库之外发生变更：{}",
                sidecar.display()
            );
            managed.push(sidecar);
        }
    }
    Ok(managed)
}

fn process_photo(
    store: &Store,
    jobs: &Jobs,
    previews: &PreviewStorage,
    job: &Job,
    path: &Path,
) -> Result<bool> {
    ensure!(
        managed_source_hash(store, jobs, &job.library_id, path)?.is_some(),
        "file is not registered for processing: {}",
        path.display()
    );
    let paths = photo_paths(store, jobs, &job.library_id, path)?;
    // Metadata writers may replace inodes. Keep the whole photo group untouched
    // until import checkpoints make every published link independently recoverable.
    if crate::imports::group_pending_publication(jobs, &job.library_id, &paths)? {
        bail!("照片组尚未完成发布，请完成导入后重试：{}", path.display());
    }
    crate::tasks::begin_photo(jobs, &job.library_id, &job.work_class, path, &paths)?;
    let result = (|| {
        jobs.set_phase(&job.id, "indexing")?;
        let (mut changed, assets) = process_files(store, jobs, job, &paths)?;
        for id in assets {
            store.initialize_negative(&job.library_id, &id)?;
            jobs.set_phase(&job.id, "media")?;
            changed |= crate::cache_pipeline::process_asset_with_override(
                store,
                jobs,
                previews,
                &job.library_id,
                &id,
                job.media_operation_id.as_deref(),
            )?;
            jobs.set_phase(&job.id, "cleanup")?;
            store.collect_asset_cache_garbage(previews, &job.library_id, &id)?;
        }
        Ok(changed)
    })();
    let error = result
        .as_ref()
        .err()
        .map(|e: &anyhow::Error| format!("{e:#}"));
    crate::tasks::finish(jobs, error.as_deref())?;
    result
}

struct SidecarWrite {
    previous_hash: Option<String>,
    hash: String,
    packet: Vec<u8>,
}

fn apply_sidecar_write(path: &Path, write: &SidecarWrite) -> Result<()> {
    let existing = match path.symlink_metadata() {
        Ok(info) if info.is_file() => Some(media::sha256_file(path)?),
        Ok(_) => bail!("XMP is not a regular file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("check XMP {}", path.display())),
    };
    if existing.as_deref() != Some(write.hash.as_str()) {
        ensure!(
            existing == write.previous_hash,
            "XMP changed outside Keeps: {}",
            path.display()
        );
        crate::identity::install_sidecar_packet(path, &write.packet, existing.is_some())?;
    }
    Ok(())
}

fn recover_sidecar_write(
    jobs: &Jobs,
    library: &str,
    path: &Path,
    original_hash: &str,
) -> Result<()> {
    use rusqlite::{OptionalExtension, params};
    let planned: Option<(String, SidecarWrite)> = jobs.db.lock()?.query_row(
        "SELECT source_hash,previous_hash,hash,packet FROM identity_sidecar_writes WHERE library_id=? AND path=?",
        params![library,path.to_str()],
        |r| Ok((r.get(0)?,SidecarWrite { previous_hash:r.get(1)?, hash:r.get(2)?, packet:r.get(3)? })),
    ).optional()?;
    if let Some((source, write)) = planned {
        ensure!(
            source == original_hash,
            "XMP write does not belong to this source version"
        );
        apply_sidecar_write(path, &write)?;
    }
    Ok(())
}

fn plan_sidecar_write(
    jobs: &Jobs,
    library: &str,
    source: &FileWork,
    path: &Path,
    root: &str,
) -> Result<String> {
    use rusqlite::params;
    use sha2::{Digest, Sha256};

    let previous_hash = if source.sidecars.iter().any(|sidecar| sidecar == path) {
        Some(media::sha256_file(path)?)
    } else {
        ensure!(
            !path.try_exists()?,
            "既有的 XMP 尚未登记，请先核对图库：{}",
            path.display()
        );
        None
    };
    let packet = crate::identity::sidecar_packet(
        path,
        root,
        &source
            .metadata
            .as_ref()
            .context("new XMP needs metadata")?
            .source,
    )?;
    let hash = format!("{:x}", Sha256::digest(&packet));
    let write = SidecarWrite {
        previous_hash,
        hash,
        packet,
    };
    {
        let mut db = jobs.db.lock()?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO identity_sidecar_writes VALUES(?,?,?,?,?,?)",
            params![
                library,
                path.to_str(),
                source.hash,
                write.previous_hash,
                write.hash,
                write.packet
            ],
        )?;
        Jobs::permit_file(&tx, library, path, &write.hash, None)?;
        tx.commit()?;
    }
    apply_sidecar_write(path, &write)?;
    Ok(write.hash)
}

struct Extracted {
    source: media::Metadata,
    snapshot: serde_json::Value,
    file: serde_json::Value,
    evidence: VersionEvidence,
}

struct FileWork {
    path: PathBuf,
    hash: String,
    size: i64,
    mtime: i64,
    pending: bool,
    sidecars: Vec<PathBuf>,
    stamp: String,
    previous: Option<crate::jobs::FileState>,
    metadata: Option<Extracted>,
}

fn prepare_file(store: &Store, jobs: &Jobs, job: &Job, path: &Path) -> Result<FileWork> {
    jobs.validate_path(path.parent().context("original has no parent")?)?;
    let text = path.to_str().context("original path must be UTF-8")?;
    let library = &job.library_id;
    let (hash, pending) = managed_source_hash(store, jobs, library, path)?
        .with_context(|| format!("file is not registered for processing: {text}"))?;
    let (size, mtime) = file_stamp(path)?;
    let asset = store.existing_asset_for_path(library, text)?;
    let canonical = crate::identity::sidecar_path(path);
    if jobs.file_publication(library, &canonical)?.is_some() {
        ensure!(
            crate::imports::file_digest(jobs, library, path)? == hash,
            "原片内容在图库之外发生变更：{text}"
        );
        recover_sidecar_write(jobs, library, &canonical, &hash)?;
    }
    let sidecars = managed_sidecars(store, jobs, library, asset.as_deref(), path)?;
    let stamp = media::sidecar_paths_stamp(&sidecars)?;
    let previous = jobs.file_state(&job.folder_id, text)?;
    let root = asset
        .as_ref()
        .map(|id| store.root_id(library, id))
        .transpose()?;
    let reuse = if let Some(previous) = &previous {
        (!job.refresh_metadata
            || job.metadata_operation_id.is_some()
                && previous.metadata_operation_id == job.metadata_operation_id)
            && previous.size == size
            && previous.mtime_ns == mtime
            && previous.metadata_stamp == stamp
            && previous.error.is_none()
            && previous.version == hash
            && root.is_some()
            && (!pending || crate::imports::file_digest(jobs, library, path)? == hash)
            && store.original_is_online(library, text, &hash)?
            && ((!sidecars.contains(&canonical) && !pending)
                || (sidecars.contains(&canonical)
                    && crate::identity::standard_sidecar_photo_id(path)? == root))
    } else {
        false
    };
    let metadata = if reuse {
        None
    } else {
        ensure!(
            crate::imports::file_digest(jobs, library, path)? == hash,
            "原片内容在图库之外发生变更：{text}"
        );
        let source = MediaProcessor::new().extract_with_sidecars(path, &sidecars)?;
        ensure!(
            file_stamp(path)? == (size, mtime),
            "original changed during metadata extraction: {text}"
        );
        ensure!(
            media::sidecar_paths_stamp(&sidecars)? == stamp,
            "XMP changed during metadata extraction: {text}"
        );
        let now = Utc::now().to_rfc3339();
        let mut snapshot = json!({
            "cameraMake":source.camera_make,"cameraModel":source.camera_model,"lensModel":source.lens_model,
            "originalFilename":source.original_filename.as_deref().unwrap_or(path.file_name().and_then(|name| name.to_str()).context("original filename missing")?),
            "contentFingerprint":hash,"metadataFingerprint":source.fingerprint(path),"rating":source.rating,
            "flagState":"unflagged","tags":[],"createdAt":now,"updatedAt":now,
        });
        for (key, value) in [
            ("captureTime", source.capture_time.as_deref()),
            ("photoID", root.as_deref().or(source.photo_id.as_deref())),
            ("cameraImageID", source.camera_image_id.as_deref()),
            ("documentID", source.document_id.as_deref()),
            ("originalDocumentID", source.original_document_id.as_deref()),
        ] {
            if let Some(value) = value {
                snapshot[key] = json!(value);
            }
        }
        let file = json!({"contentHash":hash,"sizeBytes":size,"role":if media::is_raw(path){"raw_original"}else{"jpeg_original"}});
        let evidence = VersionEvidence {
            visual_hash: None,
            width: source.width,
            height: source.height,
            edited: source.edited && !media::is_raw(path),
            camera_serial: source.camera_serial.clone(),
            capture_original: source.capture_original.clone(),
        };
        Some(Extracted {
            source,
            snapshot,
            file,
            evidence,
        })
    };
    Ok(FileWork {
        path: path.to_path_buf(),
        hash,
        size,
        mtime,
        pending,
        sidecars,
        stamp,
        previous,
        metadata,
    })
}

struct IndexedFile {
    path: PathBuf,
    state: crate::jobs::FileState,
    completed: Vec<(PathBuf, String)>,
    identity_ready: bool,
}

fn publish_file(
    db: &rusqlite::Connection,
    jobs: &Jobs,
    job: &Job,
    work: FileWork,
) -> Result<IndexedFile> {
    use crate::catalog::{SidecarPublication, ingest_original_in_transaction, register_sidecar};
    use rusqlite::params;

    let library = &job.library_id;
    let text = work.path.to_str().context("original path must be UTF-8")?;
    ensure!(
        file_stamp(&work.path)? == (work.size, work.mtime),
        "original changed before catalog publication: {text}"
    );
    ensure!(
        media::sidecar_paths_stamp(&work.sidecars)? == work.stamp,
        "XMP changed before catalog publication: {text}"
    );
    let asset: String = if let Some(metadata) = &work.metadata {
        ingest_original_in_transaction(
            db,
            library,
            text,
            &metadata.file,
            &metadata.snapshot,
            &metadata.evidence,
        )?
    } else {
        db.query_row(
            "SELECT asset_id FROM catalog_paths WHERE library_id=? AND path=? AND content_hash=?",
            params![library, text, work.hash],
            |r| r.get(0),
        )?
    };
    let root: String = db.query_row(
        "SELECT root_id FROM catalog_identity_roots WHERE library_id=? AND asset_id=?",
        params![library, asset],
        |r| r.get(0),
    )?;
    let canonical = crate::identity::sidecar_path(&work.path);
    let mut sidecars = work.sidecars.clone();
    let canonical_valid = sidecars.contains(&canonical)
        && crate::identity::standard_sidecar_photo_id(&work.path)?.as_deref() == Some(&root);
    if !canonical_valid && (work.pending || job.refresh_metadata && sidecars.contains(&canonical)) {
        plan_sidecar_write(jobs, library, &work, &canonical, &root)?;
        if !sidecars.contains(&canonical) {
            sidecars.push(canonical);
        }
    }
    let mut identity_ready = false;
    let mut completed = Vec::new();
    for path in &sidecars {
        let identity = crate::identity::file_sidecar_photo_id(path)?;
        if identity.as_deref() == Some(root.as_str()) {
            identity_ready = true;
        }
        if let Some((hash, batch)) = jobs.file_publication(library, path)? {
            ensure!(
                crate::imports::file_digest(jobs, library, path)? == hash,
                "已导入的 XMP 在图库之外发生变更：{}",
                path.display()
            );
            register_sidecar(
                db,
                library,
                &asset,
                SidecarPublication {
                    media: text,
                    path: path.to_str().context("XMP path must be UTF-8")?,
                    hash: &hash,
                    identity: identity.as_deref(),
                    authorized_import: batch.is_some(),
                },
            )?;
            completed.push((path.clone(), hash));
        }
    }
    if work.pending {
        ensure!(identity_ready, "缺少与照片身份一致的 XMP：{text}");
        completed.push((work.path.clone(), work.hash.clone()));
    }
    let state = crate::jobs::FileState {
        size: work.size,
        mtime_ns: work.mtime,
        asset_id: asset,
        version: work.hash,
        error: None,
        metadata_stamp: media::sidecar_paths_stamp(&sidecars)?,
        metadata_operation_id: if work.metadata.is_none() {
            work.previous
                .and_then(|previous| previous.metadata_operation_id)
        } else if job.refresh_metadata {
            job.metadata_operation_id.clone()
        } else {
            None
        },
    };
    Ok(IndexedFile {
        path: work.path,
        state,
        completed,
        identity_ready,
    })
}

fn process_files(
    store: &Store,
    jobs: &Jobs,
    job: &Job,
    paths: &[PathBuf],
) -> Result<(bool, std::collections::BTreeSet<String>)> {
    use rusqlite::params;

    let mut work = Vec::with_capacity(paths.len());
    for path in paths {
        match prepare_file(store, jobs, job, path) {
            Ok(prepared) => work.push(prepared),
            Err(error) => {
                let text = path.to_str().context("original path must be UTF-8")?;
                if let Some(mut previous) = jobs.file_state(&job.folder_id, text)? {
                    previous.error = Some(format!("{error:#}"));
                    previous.metadata_operation_id = None;
                    jobs.record_metadata_result(&job.folder_id, text, &previous)?;
                }
                return Err(error).with_context(|| format!("inspect {}", path.display()));
            }
        }
    }
    let changed = work.iter().any(|work| work.metadata.is_some());
    let indexed = {
        let _source = store
            .source_mutation
            .write()
            .map_err(|_| anyhow::anyhow!("source mutation lock poisoned"))?;
        let mut db = store.lock()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let indexed = work
            .into_iter()
            .map(|work| publish_file(&tx, jobs, job, work))
            .collect::<Result<Vec<_>>>()?;
        if jobs.manages_catalog() {
            for item in &indexed {
                Jobs::record_catalog_check(
                    &tx,
                    &job.library_id,
                    item.path.to_str().context("indexed path must be UTF-8")?,
                    &item.state,
                    item.identity_ready,
                )?;
            }
        }
        crate::revisions::flush_for_batch(&tx, Some(&job.id))?;
        tx.commit()?;
        indexed
    };
    let mut db = jobs.db.lock()?;
    let tx = db.transaction()?;
    let mut assets = std::collections::BTreeSet::new();
    for indexed in indexed {
        let path = indexed
            .path
            .to_str()
            .context("original path must be UTF-8")?;
        if !jobs.manages_catalog() {
            Jobs::record_metadata_result_in(&tx, &job.folder_id, path, &indexed.state)?;
            if indexed.identity_ready {
                tx.execute("UPDATE files SET identity_status='ready',identity_error=NULL WHERE folder_id=? AND path=?",params![job.folder_id,path])?;
            }
        }
        for (path, hash) in indexed.completed {
            let path = path.to_str().context("publication path must be UTF-8")?;
            Jobs::complete_file_publication_in(&tx, &job.library_id, path, &hash)?;
        }
        assets.insert(indexed.state.asset_id);
    }
    tx.commit()?;
    Ok((changed, assets))
}

#[cfg(test)]
fn process_file(
    store: &Store,
    jobs: &Jobs,
    _previews: &PreviewStorage,
    job: &Job,
    path: &Path,
    _stop: &AtomicBool,
) -> Result<bool> {
    process_files(store, jobs, job, &[path.to_path_buf()]).map(|result| result.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_failure_rolls_back_catalog_and_reuses_durable_xmp_on_retry() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let originals = fixture.path().join("originals");
        fs::create_dir(&originals)?;
        let store = Store::open(&fixture.path().join("catalog.sqlite"), true)?;
        let jobs = Jobs::open(&fixture.path().join("jobs.sqlite"), &originals)?;
        let folder = jobs.add_folder("photos", ".")?;
        let first = jobs.root().join("new/pair.png");
        let second = jobs.root().join("new/pair.jpg");
        fs::create_dir(first.parent().unwrap())?;
        let image = [
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 3,
            8, 2, 0, 0, 0, 54, 136, 73, 214, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 192, 2,
            0, 0, 21, 0, 1, 170, 101, 151, 196, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ];
        let paths = [first.clone(), second.clone()];
        let jpeg = include_bytes!("../../macos/scripts/runtime-sample/sample.jpg");
        let identity = "a98d1952-980d-4867-a85d-1c9a8b3a6103";
        let stem_xmp = first.with_extension("xmp");
        fs::write(
            &stem_xmp,
            format!(
                r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/"><rdf:Description xmp:Identifier="https://keeps.photos/photo/{identity}"/></rdf:RDF>"#
            ),
        )?;
        fs::write(&first, image)?;
        fs::write(&second, jpeg)?;
        for path in [&first, &second, &stem_xmp] {
            let hash = media::sha256_file(path)?;
            Jobs::permit_file(
                &jobs.db.lock().unwrap(),
                "photos",
                path,
                &hash,
                Some("batch"),
            )?;
        }
        let job = jobs.enqueue_manual_file(&folder.id, &first)?;
        store.lock()?.execute_batch("CREATE TRIGGER abort_second BEFORE INSERT ON catalog_physical_files
            WHEN substr(NEW.path,-9)='/pair.jpg' BEGIN SELECT RAISE(ABORT,'fixture second version rejected'); END")?;
        let before = store.library_revision("photos")?;
        let failed = process_files(&store, &jobs, &job, &paths).unwrap_err();
        assert!(
            format!("{failed:#}").contains("fixture second version rejected"),
            "{failed:#}"
        );
        assert_eq!(store.library_revision("photos")?, before);
        assert_eq!(store.counts("photos", false)?["all"], 0);
        assert!(!crate::navigation::registered(
            &store.read()?,
            "photos",
            first.parent().unwrap()
        )?);
        let first_xmp = crate::identity::sidecar_path(&first);
        let packet = fs::read(&first_xmp)?;
        let root = crate::identity::file_sidecar_photo_id(&first_xmp)?
            .context("generated XMP identity")?;
        assert!(jobs.file_publication("photos", &first_xmp)?.is_some());
        store.lock()?.execute_batch("DROP TRIGGER abort_second")?;
        let (_, assets) = process_files(&store, &jobs, &job, &paths)?;
        assert_eq!(assets.len(), 1);
        let asset = assets.first().unwrap();
        assert_eq!(store.root_id("photos", asset)?, root);
        assert_eq!(fs::read(&first_xmp)?, packet);
        for (path, expected) in paths.iter().zip([image.as_slice(), jpeg.as_slice()]) {
            assert_eq!(fs::read(path)?, expected);
            let xmp = crate::identity::sidecar_path(path);
            assert_eq!(
                crate::identity::file_sidecar_photo_id(&xmp)?.as_deref(),
                Some(root.as_str())
            );
            assert!(jobs.file_publication("photos", path)?.is_none());
            assert!(jobs.file_publication("photos", &xmp)?.is_none());
        }
        assert_eq!(
            store.asset("photos", asset)?["paths"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn ordinary_file_reuses_index_but_metadata_override_reextracts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        fs::create_dir(&originals)?;
        let jobs = Jobs::open(&temp.path().join("jobs.sqlite"), &originals)?;
        let store = Store::open(&temp.path().join("catalog.sqlite"), true)?;
        let previews = PreviewStorage::new(
            &temp.path().join("keeps"),
            Some(&originals),
            "http://localhost",
            "test",
        )?;
        let path = jobs.root().join("photo.jpg");
        fs::write(
            &path,
            b"fixture must not be decoded when its index is current",
        )?;
        let hash = media::sha256_file(&path)?;
        let (size, mtime) = file_stamp(&path)?;
        let id = store.ingest_original(
            "library",
            path.to_str().unwrap(),
            &json!({"contentHash":hash,"sizeBytes":size,"role":"jpeg_original"}),
            &json!({"originalFilename":"photo.jpg","contentFingerprint":hash,"metadataFingerprint":"fixture","rating":0,"flagState":"unflagged","tags":[],"createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z"}),
        )?;
        let sidecar = crate::identity::ensure_known(&path, &store.root_id("library", &id)?)?.path;
        store.publish_identity_sidecar("library", &id, &path, &sidecar, None)?;
        let folder = jobs.add_folder("library", ".")?;
        jobs.record_file(
            &folder.id,
            path.to_str().unwrap(),
            size,
            mtime,
            &id,
            &hash,
            None,
        )?;
        jobs.record_metadata_stamp(
            &folder.id,
            path.to_str().unwrap(),
            &media::sidecar_paths_stamp(std::slice::from_ref(&sidecar))?,
        )?;
        let mut job = jobs.enqueue_manual_file(&folder.id, &path)?;
        let stop = AtomicBool::new(false);
        assert!(!process_file(&store, &jobs, &previews, &job, &path, &stop)?);
        let mut checkpoint = jobs
            .file_state(&folder.id, path.to_str().unwrap())?
            .unwrap();
        checkpoint.asset_id = "merged-away-asset".to_owned();
        checkpoint.metadata_operation_id = Some("completed-operation".to_owned());
        jobs.record_metadata_result(&folder.id, path.to_str().unwrap(), &checkpoint)?;
        assert!(!process_file(&store, &jobs, &previews, &job, &path, &stop)?);
        let refreshed = jobs
            .file_state(&folder.id, path.to_str().unwrap())?
            .unwrap();
        assert_eq!(refreshed.asset_id, id);
        assert_eq!(
            refreshed.metadata_operation_id,
            checkpoint.metadata_operation_id
        );
        assert_eq!(refreshed.version, checkpoint.version);
        assert_eq!(refreshed.metadata_stamp, checkpoint.metadata_stamp);
        job.rebuild_media = true;
        assert!(!process_file(&store, &jobs, &previews, &job, &path, &stop)?);
        job.refresh_metadata = true;
        assert!(process_file(&store, &jobs, &previews, &job, &path, &stop).is_err());
        assert_eq!(media::sha256_file(&path)?, hash);
        Ok(())
    }

    #[test]
    fn metadata_override_resumes_same_generation_and_reprocesses_changed_input() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let originals = temp.path().join("originals");
        fs::create_dir(&originals)?;
        let jobs_path = temp.path().join("jobs.sqlite");
        let jobs = Jobs::open(&jobs_path, &originals)?;
        let store = Store::open(&temp.path().join("catalog.sqlite"), true)?;
        let previews = PreviewStorage::new(
            &temp.path().join("keeps"),
            Some(&originals),
            "http://localhost",
            "test",
        )?;
        let path = jobs.root().join("photo.png");
        fs::write(
            &path,
            [
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0,
                3, 8, 2, 0, 0, 0, 54, 136, 73, 214, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96,
                192, 2, 0, 0, 21, 0, 1, 170, 101, 151, 196, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
                96, 130,
            ],
        )?;
        let hash = media::sha256_file(&path)?;
        Jobs::permit_file(&jobs.db.lock().unwrap(), "library", &path, &hash, None)?;
        let folder = jobs.add_folder("library", ".")?;
        let initial = jobs.enqueue_import_repair(&folder.id, &path)?;
        let job = jobs.claim_next()?.unwrap();
        let stop = AtomicBool::new(false);
        assert!(process_file(&store, &jobs, &previews, &job, &path, &stop)?);
        assert_eq!(
            jobs.file_state(&folder.id, path.to_str().unwrap())?
                .unwrap()
                .metadata_operation_id,
            initial.metadata_operation_id
        );
        drop(jobs);
        let jobs = Jobs::open(&jobs_path, &originals)?;
        let resumed = jobs.claim_next()?.unwrap();
        assert!(resumed.refresh_metadata);
        assert_eq!(resumed.metadata_operation_id, initial.metadata_operation_id);
        assert!(!process_file(
            &store, &jobs, &previews, &resumed, &path, &stop
        )?);
        let next = jobs.enqueue_import_repair(&folder.id, &path)?;
        assert_ne!(next.metadata_operation_id, resumed.metadata_operation_id);
        assert!(process_file(&store, &jobs, &previews, &next, &path, &stop)?);
        assert!(!process_file(
            &store, &jobs, &previews, &next, &path, &stop
        )?);
        assert_eq!(media::sha256_file(&path)?, hash);
        let unauthorized = path.with_extension("xmp");
        fs::write(
            &unauthorized,
            b"unregistered sidecar must not affect metadata",
        )?;
        assert!(!process_file(
            &store, &jobs, &previews, &next, &path, &stop
        )?);
        let unknown: bool = store.read()?.query_row("SELECT EXISTS(SELECT 1 FROM catalog_physical_files WHERE library_id='library' AND path=?)", [unauthorized.to_str().unwrap()], |r|r.get(0))?;
        assert!(!unknown);
        fs::write(&path, b"changed invalid source must be inspected")?;
        assert!(process_file(&store, &jobs, &previews, &next, &path, &stop).is_err());
        assert!(
            jobs.file_state(&folder.id, path.to_str().unwrap())?
                .unwrap()
                .metadata_operation_id
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn import_uncheckpointed_pair_fails_without_touching_metadata() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let originals = directory.path().join("originals");
        fs::create_dir(&originals)?;
        let store = Store::open(&directory.path().join("catalog.sqlite"), true)?;
        let jobs = Jobs::open(&directory.path().join("jobs.sqlite"), &originals)?;
        let path = jobs.root().join("pair.CR3");
        fs::write(&path, b"unchanged original")?;
        Jobs::permit_file(
            &jobs.db.lock().unwrap(),
            "library",
            &path,
            &media::sha256_file(&path)?,
            None,
        )?;
        let previews = PreviewStorage::new(
            &directory.path().join("keeps"),
            Some(&originals),
            "http://localhost",
            "test-key",
        )?;
        let folder = jobs.add_folder("library", ".")?;
        let job = jobs.enqueue_manual_file(&folder.id, &path)?;
        let batch_id = uuid::Uuid::new_v4().to_string();
        let manifest = json!({"id":batch_id, "targetPath":jobs.root(), "finished":false,"jobId":null,
            "files":[{"id":uuid::Uuid::new_v4().to_string(),"relativePath":"pair.JPG", "fileName":"pair.JPG", "size":8,"uploaded":true,"published":false}]});
        jobs.db.lock().unwrap().execute(
            "INSERT INTO imports(id,library_id,manifest) VALUES(?,?,?)",
            rusqlite::params![batch_id, "library", manifest.to_string()],
        )?;
        assert!(
            process_photo(&store, &jobs, &previews, &job, &path)
                .unwrap_err()
                .to_string()
                .contains("尚未完成发布")
        );
        assert_eq!(fs::read(&path)?, b"unchanged original");
        assert!(
            jobs.file_state(&folder.id, path.to_str().unwrap())?
                .is_none()
        );
        let queued = jobs.jobs(100)?;
        assert!(queued.iter().any(
            |pending| pending.work_class == "manual" && pending.path == path.to_str().unwrap()
        ));
        assert!(!crate::imports::group_pending_publication(
            &jobs,
            "library",
            &[jobs.root().join("other.CR3")]
        )?);
        assert!(!crate::imports::group_pending_publication(
            &jobs,
            "library",
            &[jobs.root().join("other/pair.CR3")]
        )?);
        jobs.db.lock().unwrap().execute("UPDATE imports SET manifest=json_set(manifest,'$.files[0].published',json('true')) WHERE id=?", [&batch_id])?;
        assert!(!crate::imports::group_pending_publication(
            &jobs,
            "library",
            &[path]
        )?);
        Ok(())
    }

    #[test]
    fn missing_file_task_does_not_scan_siblings_or_descendants() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let originals = directory.path().join("originals");
        fs::create_dir_all(originals.join("child"))?;
        fs::write(originals.join("sibling.jpg"), b"invalid media")?;
        fs::write(originals.join("child/nested.jpg"), b"invalid media")?;
        let store = Store::open(&directory.path().join("catalog.sqlite"), true)?;
        let jobs = Jobs::open(&directory.path().join("jobs.sqlite"), &originals)?;
        let previews = PreviewStorage::new(
            &directory.path().join("keeps"),
            Some(&originals),
            "http://localhost:2283",
            "test-key",
        )?;
        let folder = jobs.add_folder("library", ".")?;
        let missing = jobs.root().join("missing.jpg");
        Jobs::permit_file(
            &*jobs.db.lock()?,
            "library",
            &missing,
            &"a".repeat(64),
            None,
        )?;
        jobs.enqueue_manual_file(&folder.id, &missing)?;
        let job = jobs.claim_next()?.unwrap();
        let error = run_one(&store, &jobs, &previews, &job, &AtomicBool::new(false)).unwrap_err();
        assert!(format!("{error:#}").contains("missing.jpg"));
        let finished = jobs.job(&job.id)?.unwrap();
        assert_eq!(
            (finished.processed, finished.skipped, finished.failed),
            (0, 0, 1)
        );
        Ok(())
    }

    #[test]
    fn stopped_job_does_not_inspect_media() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let originals = directory.path().join("originals");
        fs::create_dir(&originals)?;
        fs::write(originals.join("unprocessed.jpg"), b"original")?;
        let store = Store::open(&directory.path().join("catalog.sqlite"), true)?;
        let jobs = Jobs::open(&directory.path().join("jobs.sqlite"), &originals)?;
        let previews = PreviewStorage::new(
            &directory.path().join("keeps"),
            Some(&originals),
            "http://localhost:2283",
            "test-key",
        )?;
        let folder = jobs.add_folder("library", ".")?;
        jobs.enqueue_manual_file(&folder.id, &jobs.root().join("unprocessed.jpg"))?;
        let job = jobs.claim_next()?.unwrap();
        jobs.remove_folder(&folder.id)?;
        assert!(!run_one(
            &store,
            &jobs,
            &previews,
            &job,
            &AtomicBool::new(false)
        )?);
        assert_eq!(jobs.jobs(1)?[0].status, "cancelled");
        assert_eq!(store.counts("library", false)?["all"], 0);
        assert_eq!(fs::read(originals.join("unprocessed.jpg"))?, b"original");
        Ok(())
    }
}
