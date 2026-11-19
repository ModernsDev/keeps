use super::*;
use crate::store::Store;
use axum::http::HeaderMap;
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Repair {
    old_hash: String,
    source_hash: String,
    old_stamp: DigestStamp,
    phase: String,
}

pub(super) fn optional_hash(headers: &HeaderMap, name: &str) -> Result<Option<String>> {
    headers
        .get(name)
        .map(|value| {
            let value = value
                .to_str()
                .map_err(|_| invalid("invalid SHA256 header"))?;
            ensure!(
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                invalid("invalid SHA256 header")
            );
            Ok(value.to_owned())
        })
        .transpose()
}

fn required_hash(headers: &HeaderMap, name: &str) -> Result<String> {
    optional_hash(headers, name)?.ok_or_else(|| invalid(format!("{name} required")))
}

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/libraries/{library}/imports/{batch}/repair-source/{task}",
            axum::routing::get(source),
        )
        .route(
            "/libraries/{library}/imports/{batch}/files/{file}/repair",
            put(upload).layer(DefaultBodyLimit::disable()),
        )
}

fn repairable(file: &File) -> Result<()> {
    ensure!(
        file.uploaded
            && file.published
            && !file.skipped
            && crate::media::is_photo(FsPath::new(&file.file_name)),
        conflict("only originals published by this import can be repaired")
    );
    Ok(())
}

async fn source(
    State(state): State<Arc<AppState>>,
    Path((library, id, task)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(tokio::task::spawn_blocking(move || -> Result<Value> {
        let _guard = state.jobs.directory_mutation.read().unwrap();
        let batch = load(&state.jobs.db.lock().unwrap(), &library, &id)?;
        let path: String = state.store.read()?.query_row("SELECT input_path FROM remote_cache_tasks WHERE library_id=? AND id=?", params![library,task], |r| r.get(0)).optional()?.ok_or_else(|| conflict("media task no longer exists"))?;
        let destination = target(&state.jobs, &library, &batch.target_path)?;
        let file = batch.files.iter().find(|file| destination.join(&file.file_name) == FsPath::new(&path)).ok_or_else(|| conflict("media input does not belong to this import"))?;
        repairable(file)?;
        let before = DigestStamp::read(FsPath::new(&path))?;
        let hash = crate::media::sha256_file(FsPath::new(&path))?;
        ensure!(DigestStamp::read(FsPath::new(&path))? == before, conflict("NAS source changed during verification"));
        Ok(json!({"fileID":file.input.id,"relativePath":file.input.relative_path,"sha256":hash,"size":file.input.size}))
    }).await.map_err(anyhow::Error::from)??))
}

fn staged(destination: &FsPath, batch: &str, file: &str, hash: &str) -> PathBuf {
    destination.join(format!(".keeps-repair-{batch}-{file}-{hash}"))
}

async fn upload(
    State(state): State<Arc<AppState>>,
    Path((library, id, file_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    mut body: Body,
) -> Result<Json<Value>, ApiError> {
    let id = uuid(&id)?;
    let file_id = uuid(&file_id)?;
    let source_hash = required_hash(&headers, "x-keeps-source-sha256")?;
    let old_hash = required_hash(&headers, "x-keeps-replaced-sha256")?;
    if source_hash == old_hash {
        return Err(invalid("identical content must not be recycled").into());
    }
    let (destination, file) = {
        let batch = load(&state.jobs.db.lock().unwrap(), &library, &id)?;
        let file = batch
            .files
            .iter()
            .find(|file| file.input.id == file_id)
            .cloned()
            .ok_or_else(|| invalid("import file not found"))?;
        repairable(&file)?;
        (target(&state.jobs, &library, &batch.target_path)?, file)
    };
    let temporary = tempfile::Builder::new()
        .prefix(".keeps-repair-upload-")
        .tempfile_in(&destination)
        .map_err(anyhow::Error::from)?;
    let mut writer = tokio::fs::File::from_std(temporary.reopen().map_err(anyhow::Error::from)?);
    let mut hash = Sha256::new();
    let mut size = 0;
    while let Some(frame) = body.frame().await {
        if let Ok(bytes) = frame.map_err(anyhow::Error::from)?.into_data() {
            size += bytes.len() as u64;
            ensure_upload_size(size, file.input.size)?;
            writer
                .write_all(&bytes)
                .await
                .map_err(anyhow::Error::from)?;
            hash.update(&bytes);
        }
    }
    if size != file.input.size || format!("{:x}", hash.finalize()) != source_hash {
        return Err(invalid("repair upload does not match camera SHA256 and size").into());
    }
    writer.sync_all().await.map_err(anyhow::Error::from)?;
    drop(writer);
    Ok(Json(
        tokio::task::spawn_blocking(move || -> Result<Value> {
            let _import = state.jobs.import_mutation.lock().unwrap();
            let _directory = state.jobs.directory_mutation.write().unwrap();
            crate::directory_move::ensure_reconciled(&state.jobs)?;
            let mut batch = load(&state.jobs.db.lock().unwrap(), &library, &id)?;
            let index = batch
                .files
                .iter()
                .position(|file| file.input.id == file_id)
                .context("import file disappeared")?;
            let file = &batch.files[index];
            repairable(file)?;
            let destination = target(&state.jobs, &library, &batch.target_path)?;
            import_directory(
                &destination,
                FsPath::new(&file.file_name)
                    .parent()
                    .context("import parent")?,
                false,
            )?;
            if let Some(repair) = &file.repair {
                ensure!(
                    repair.source_hash == source_hash && repair.old_hash == old_hash,
                    conflict("different repair already pending")
                );
                return Ok(json!({"status":"repairing"}));
            }
            let output = destination.join(&file.file_name);
            let old_stamp = DigestStamp::read(&output)?;
            let actual = crate::media::sha256_file(&output)?;
            ensure!(
                DigestStamp::read(&output)? == old_stamp,
                conflict("NAS file changed during verification")
            );
            if actual == source_hash {
                return Ok(json!({"status":"repaired"}));
            }
            ensure!(
                actual == old_hash && file.received_sha256.as_ref() == Some(&old_hash),
                conflict("NAS file changed since import; refusing to recycle unrelated content")
            );
            crate::directory_trash::recycle_share(
                &fs::read_to_string("/etc/samba/smb.share.conf")?,
                &output,
            )?;
            ensure!(
                crate::media::sha256_file(temporary.path())? == source_hash,
                conflict("repair staging verification failed")
            );
            let receipt = staged(&destination, &id, &file_id, &source_hash);
            match fs::hard_link(temporary.path(), &receipt) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure!(
                        DigestStamp::read(&receipt)?.size == file.input.size
                            && crate::media::sha256_file(&receipt)? == source_hash,
                        conflict("repair staging collision")
                    );
                }
                Err(error) => return Err(error.into()),
            }
            fs::File::open(&destination)?.sync_all()?;
            batch.files[index].repair = Some(Repair {
                old_hash,
                source_hash,
                old_stamp,
                phase: "staged".into(),
            });
            batch.finish_error = None;
            batch.finished = false;
            batch.job_id = None;
            save(&state.jobs.db.lock().unwrap(), &library, &batch)?;
            Ok(json!({"status":"repairing"}))
        })
        .await
        .map_err(anyhow::Error::from)??,
    ))
}

pub(super) fn execute(jobs: &Jobs, store: &Store, library: &str, id: &str) -> Result<()> {
    execute_with(jobs, store, library, id, |path| {
        let share = crate::directory_trash::recycle_share(
            &fs::read_to_string("/etc/samba/smb.share.conf")?,
            path,
        )?;
        crate::directory_trash::recycle_file(&share, path)
    })
}

fn execute_with(
    jobs: &Jobs,
    store: &Store,
    library: &str,
    id: &str,
    recycle: impl Fn(&FsPath) -> Result<()>,
) -> Result<()> {
    let mut batch = load(&jobs.db.lock().unwrap(), library, id)?;
    if !batch.files.iter().any(|file| file.repair.is_some()) {
        return Ok(());
    }
    let _directory = jobs.directory_mutation.write().unwrap();
    crate::directory_move::ensure_reconciled(jobs)?;
    let _source = store
        .source_mutation
        .write()
        .map_err(|_| anyhow::anyhow!("source mutation lock poisoned"))?;
    let destination = target(jobs, library, &batch.target_path)?;
    for index in 0..batch.files.len() {
        let Some(mut repair) = batch.files[index].repair.clone() else {
            continue;
        };
        let file = batch.files[index].clone();
        repairable(&file)?;
        let parent = import_directory(
            &destination,
            FsPath::new(&file.file_name)
                .parent()
                .context("import parent")?,
            false,
        )?;
        let output = destination.join(&file.file_name);
        let receipt = staged(&destination, &batch.id, &file.input.id, &repair.source_hash);
        ensure!(
            DigestStamp::read(&receipt)?.size == file.input.size
                && crate::media::sha256_file(&receipt)? == repair.source_hash,
            conflict("verified camera staging is unavailable; original preserved")
        );
        if repair.phase == "staged" {
            ensure!(
                DigestStamp::read(&output)? == repair.old_stamp
                    && crate::media::sha256_file(&output)? == repair.old_hash,
                conflict("NAS original changed before recycle")
            );
            repair.phase = "recycling".into();
            batch.files[index].repair = Some(repair.clone());
            save(&jobs.db.lock().unwrap(), library, &batch)?;
            recycle(&output)?;
        }
        if repair.phase == "recycling" {
            ensure!(
                fs::symlink_metadata(&output)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                conflict("recycle outcome uncertain; inspect NAS recycle bin before retrying")
            );
            fs::File::open(&parent)?.sync_all()?;
            repair.phase = "recycled".into();
            batch.files[index].repair = Some(repair.clone());
            save(&jobs.db.lock().unwrap(), library, &batch)?;
        }
        ensure!(repair.phase == "recycled", conflict("unknown repair phase"));
        match fs::hard_link(&receipt, &output) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => ensure!(
                same_file(&receipt, &output)?,
                conflict("repair destination occupied; preserved existing file")
            ),
            Err(error) => return Err(error.into()),
        }
        fs::File::open(&parent)?.sync_all()?;
        ensure!(
            crate::media::sha256_file(&output)? == repair.source_hash,
            conflict("published repair checksum mismatch")
        );
        let path = output.to_str().context("UTF-8 import path")?;
        let db = store.lock()?;
        db.execute("UPDATE remote_cache_tasks SET completed=-1 WHERE library_id=?1 AND (input_path=?2 OR asset_id IN (SELECT asset_id FROM catalog_paths WHERE library_id=?1 AND path=?2)) AND completed IN (0,1)", params![library,path])?;
        db.execute("UPDATE media_cache SET status='pending',attempts=0,available_at=0,last_error=NULL,browse_attempts=0,browse_available_at=0,browse_last_error=NULL WHERE library_id=?1 AND asset_id IN (SELECT asset_id FROM catalog_paths WHERE library_id=?1 AND path=?2)", params![library,path])?;
        drop(db);
        let folder = jobs
            .folders()?
            .into_iter()
            .filter(|f| f.active && f.library_id == library && output.starts_with(&f.path))
            .max_by_key(|f| f.path.len())
            .context("tracked folder disappeared")?;
        let job = jobs.enqueue_import_repair(&folder.id, &output)?;
        let key = format!(
            "{}/{}",
            FsPath::new(&file.file_name)
                .parent()
                .context("import parent")?
                .display(),
            destination_stem(&file.file_name)
        );
        batch.group_jobs.insert(key, job.id);
        batch.files[index].received_sha256 = Some(repair.source_hash.clone());
        batch.files[index].digest_stamp = None;
        batch.files[index].repair = None;
        save(&jobs.db.lock().unwrap(), library, &batch)?;
        fs::remove_file(&receipt)?;
        fs::File::open(&destination)?.sync_all()?;
        let stamp = DigestStamp::read(&output)?;
        save_digest(
            &jobs.db.lock().unwrap(),
            library,
            &output,
            &repair.source_hash,
            &stamp,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, Arc<AppState>, Batch)> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("photos");
        fs::create_dir(&root)?;
        let keeps = dir.path().join("keeps");
        let previews = crate::previews::PreviewStorage::new(
            &keeps,
            Some(&root),
            "http://localhost",
            "test-preview-key-01234567890123456789",
        )?;
        let state = Arc::new(AppState {
            store: Arc::new(Store::open(&keeps.join("db/control_plane.sqlite"), true)?),
            previews: Arc::new(previews),
            jobs: Arc::new(Jobs::open(&keeps.join("db/jobs.sqlite"), &root)?),
            access_token: String::new(),
            library_id: "lib".into(),
            original_root_names: Default::default(),
        });
        state.jobs.add_folder("lib", ".")?;
        let mut batch = prepare(
            &state.jobs,
            "lib",
            Prepare {
                id: uuid::Uuid::new_v4().to_string(),
                target_path: state.jobs.root().to_string_lossy().into_owned(),
                deduplicate: false,
                preserve_structure: false,
                files: vec![InputFile {
                    id: uuid::Uuid::new_v4().to_string(),
                    relative_path: "DCIM/photo.HEIC".into(),
                    size: 8,
                    sha256: None,
                }],
            },
        )?;
        let output = root.join(&batch.files[0].file_name);
        fs::write(&output, b"damaged!")?;
        fs::write(root.join("photo.xmp"), b"permanent-photo-identity")?;
        let old_hash = crate::media::sha256_file(&output)?;
        let source_hash = format!("{:x}", Sha256::digest(b"original"));
        fs::write(
            staged(&root, &batch.id, &batch.files[0].input.id, &source_hash),
            b"original",
        )?;
        batch.files[0].uploaded = true;
        batch.files[0].published = true;
        batch.files[0].publication_pending = false;
        batch.files[0].received_sha256 = Some(old_hash.clone());
        batch.files[0].repair = Some(Repair {
            old_hash,
            source_hash,
            old_stamp: DigestStamp::read(&output)?,
            phase: "staged".into(),
        });
        save(&state.jobs.db.lock().unwrap(), "lib", &batch)?;
        Ok((dir, state, batch))
    }

    #[test]
    fn repairs_only_bad_original_preserving_identity_and_repeat_is_noop() -> Result<()> {
        let (dir, state, batch) = fixture()?;
        let output = state.jobs.root().join("photo.HEIC");
        let recycled = dir.path().join("recycled");
        state.store.lock()?.execute("INSERT INTO remote_cache_tasks(id,library_id,asset_id,source_hash,input_hash,input_path,worker_id,expires_at,completed) VALUES('old','lib','asset','old','old',?,'worker',0,1)", [output.to_str().unwrap()])?;
        execute_with(&state.jobs, &state.store, "lib", &batch.id, |path| {
            fs::rename(path, &recycled)?;
            Ok(())
        })?;
        assert_eq!(fs::read(&recycled)?, b"damaged!");
        assert_eq!(
            state.store.read()?.query_row(
                "SELECT completed FROM remote_cache_tasks WHERE id='old'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            -1
        );
        assert_eq!(fs::read(&output)?, b"original");
        assert_eq!(
            fs::read(state.jobs.root().join("photo.xmp"))?,
            b"permanent-photo-identity"
        );
        let saved = load(&state.jobs.db.lock().unwrap(), "lib", &batch.id)?;
        assert!(saved.files[0].repair.is_none());
        assert_eq!(
            saved.files[0].received_sha256,
            Some(crate::media::sha256_file(&output)?)
        );
        assert_eq!(saved.group_jobs.len(), 1);
        assert!(
            state
                .jobs
                .job(saved.group_jobs.values().next().unwrap())?
                .unwrap()
                .refresh_metadata
        );
        execute_with(&state.jobs, &state.store, "lib", &batch.id, |_| {
            panic!("must not recycle twice")
        })?;
        Ok(())
    }

    #[test]
    fn damaged_staging_or_changed_original_never_recycles() -> Result<()> {
        for damage_staging in [true, false] {
            let (_dir, state, batch) = fixture()?;
            let file = &batch.files[0];
            let path = if damage_staging {
                staged(
                    state.jobs.root(),
                    &batch.id,
                    &file.input.id,
                    &file.repair.as_ref().unwrap().source_hash,
                )
            } else {
                state.jobs.root().join(&file.file_name)
            };
            fs::write(&path, b"changed!")?;
            assert!(
                execute_with(&state.jobs, &state.store, "lib", &batch.id, |_| panic!(
                    "must preserve original"
                ))
                .is_err()
            );
            assert!(state.jobs.root().join(&file.file_name).exists());
        }
        Ok(())
    }

    #[test]
    fn restart_after_recycle_resumes_without_second_deletion() -> Result<()> {
        let (dir, state, mut batch) = fixture()?;
        let output = state.jobs.root().join("photo.HEIC");
        fs::rename(&output, dir.path().join("recycled"))?;
        batch.files[0].repair.as_mut().unwrap().phase = "recycling".into();
        save(&state.jobs.db.lock().unwrap(), "lib", &batch)?;
        execute_with(&state.jobs, &state.store, "lib", &batch.id, |_| {
            panic!("already recycled")
        })?;
        assert_eq!(fs::read(&output)?, b"original");
        Ok(())
    }

    #[test]
    fn uncertain_recycle_is_not_repeated_and_collision_is_not_overwritten() -> Result<()> {
        for phase in ["recycling", "recycled"] {
            let (_dir, state, mut batch) = fixture()?;
            batch.files[0].repair.as_mut().unwrap().phase = phase.into();
            save(&state.jobs.db.lock().unwrap(), "lib", &batch)?;
            assert!(
                execute_with(&state.jobs, &state.store, "lib", &batch.id, |_| panic!(
                    "must not repeat recycle"
                ))
                .is_err()
            );
            assert_eq!(fs::read(state.jobs.root().join("photo.HEIC"))?, b"damaged!");
        }
        Ok(())
    }

    #[test]
    fn restart_after_publish_recognizes_own_inode() -> Result<()> {
        let (dir, state, mut batch) = fixture()?;
        let file = &mut batch.files[0];
        let repair = file.repair.as_mut().unwrap();
        let output = state.jobs.root().join(&file.file_name);
        fs::rename(&output, dir.path().join("recycled"))?;
        fs::hard_link(
            staged(
                state.jobs.root(),
                &batch.id,
                &file.input.id,
                &repair.source_hash,
            ),
            &output,
        )?;
        repair.phase = "recycled".into();
        save(&state.jobs.db.lock().unwrap(), "lib", &batch)?;
        execute_with(&state.jobs, &state.store, "lib", &batch.id, |_| {
            panic!("already published")
        })?;
        assert_eq!(fs::read(&output)?, b"original");
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_repair_upload_never_changes_original() -> Result<()> {
        let (_dir, state, batch) = fixture()?;
        let repair = batch.files[0].repair.as_ref().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-keeps-source-sha256", repair.source_hash.parse()?);
        headers.insert("x-keeps-replaced-sha256", repair.old_hash.parse()?);
        assert!(
            upload(
                State(state.clone()),
                Path((
                    "lib".into(),
                    batch.id.clone(),
                    batch.files[0].input.id.clone()
                )),
                headers,
                Body::from("bad-copy")
            )
            .await
            .is_err()
        );
        assert_eq!(fs::read(state.jobs.root().join("photo.HEIC"))?, b"damaged!");
        Ok(())
    }

    #[tokio::test]
    async fn normal_upload_rejects_camera_checksum_mismatch() -> Result<()> {
        let (_dir, state, mut batch) = fixture()?;
        batch.files[0].uploaded = false;
        batch.files[0].published = false;
        batch.files[0].received_sha256 = None;
        batch.files[0].repair = None;
        save(&state.jobs.db.lock().unwrap(), "lib", &batch)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-keeps-source-sha256",
            format!("{:x}", Sha256::digest(b"original")).parse()?,
        );
        assert!(
            super::super::upload(
                State(state.clone()),
                Path((
                    "lib".into(),
                    batch.id.clone(),
                    batch.files[0].input.id.clone()
                )),
                headers,
                Body::from("damaged!")
            )
            .await
            .is_err()
        );
        assert!(!load(&state.jobs.db.lock().unwrap(), "lib", &batch.id)?.files[0].uploaded);
        Ok(())
    }

    #[test]
    fn sidecars_and_deduplicated_files_cannot_be_recycled() -> Result<()> {
        let (_dir, _state, batch) = fixture()?;
        let mut file = batch.files[0].clone();
        file.skipped = true;
        assert!(repairable(&file).is_err());
        file.skipped = false;
        file.file_name = "photo.xmp".into();
        assert!(repairable(&file).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn maps_actual_media_path_to_source_and_rejects_same_basename_elsewhere() -> Result<()> {
        let (_dir, state, batch) = fixture()?;
        let path = state.jobs.root().join("photo.HEIC");
        state.store.lock()?.execute("INSERT INTO remote_cache_tasks(id,library_id,asset_id,source_hash,input_hash,input_path,worker_id,expires_at,completed) VALUES('media','lib','asset','old','old',?,'worker',0,-1)", [path.to_str().unwrap()])?;
        let result = source(
            State(state.clone()),
            Path(("lib".into(), batch.id.clone(), "media".into())),
        )
        .await;
        assert!(result.is_ok());
        let value = result.ok().unwrap().0;
        assert_eq!(value["fileID"], batch.files[0].input.id);
        assert_eq!(value["relativePath"], "DCIM/photo.HEIC");
        state.store.lock()?.execute(
            "UPDATE remote_cache_tasks SET input_path=?",
            [state.jobs.root().join("other/photo.HEIC").to_str().unwrap()],
        )?;
        assert!(
            source(
                State(state.clone()),
                Path(("lib".into(), batch.id.clone(), "media".into()))
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn restart_preserves_persisted_repair_for_background_resume() -> Result<()> {
        let (_dir, state, batch) = fixture()?;
        interrupt_pending(&state.jobs)?;
        let saved = load(&state.jobs.db.lock().unwrap(), "lib", &batch.id)?;
        assert!(saved.finish_error.is_none());
        assert!(saved.files[0].repair.is_some());
        assert_eq!(
            pipeline_status(&state, "lib", &batch.id)?["status"],
            "repairing"
        );
        Ok(())
    }
}
