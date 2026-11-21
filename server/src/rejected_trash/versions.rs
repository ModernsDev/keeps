use super::*;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct Pick {
    asset_id: String,
    keep_path: String,
    recycle_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct Selection {
    pub(super) pick: Pick,
    destination: String,
    keep_hash: String,
    recycle_hash: String,
    default_hash: Option<String>,
    changes_default: bool,
    changes_editing_negative: bool,
    photo_id: String,
    retained_files: Vec<File>,
    recycle_files: Vec<String>,
}

fn sidecars(path: &Path) -> Result<BTreeSet<PathBuf>> {
    let candidates = crate::media::sidecars(path)?;
    let mut result = BTreeSet::new();
    for candidate in candidates {
        result.insert(candidate.canonicalize()?);
    }
    Ok(result)
}

fn indexed_pick(
    lookup: &mut rusqlite::Statement<'_>,
    library: &str,
    pick: Pick,
) -> Result<(Pick, String, String)> {
    let (keep_path, keep_hash): (String, String) = lookup
        .query_row(params![library, pick.asset_id, pick.keep_path], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?
        .ok_or_else(|| error(409, "找不到保留文件的图库记录"))?;
    let (recycle_path, recycle_hash): (String, String) = lookup
        .query_row(params![library, pick.asset_id, pick.recycle_path], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?
        .ok_or_else(|| error(409, "找不到待回收文件的图库记录"))?;
    Ok((
        Pick {
            asset_id: pick.asset_id,
            keep_path,
            recycle_path,
        },
        keep_hash,
        recycle_hash,
    ))
}

fn selection(
    db: &Connection,
    library: &str,
    pick: Pick,
    keep_hash: String,
    recycle_hash: String,
) -> Result<Selection> {
    let keep = Path::new(&pick.keep_path);
    let recycle = Path::new(&pick.recycle_path);
    ensure!(
        keep != recycle && crate::media::is_photo(keep) && crate::media::is_photo(recycle),
        error(422, "请选择同一张照片的两个不同文件")
    );
    let photo_id: String = db.query_row("SELECT root_id FROM catalog_identity_roots JOIN catalog_assets USING(library_id) WHERE library_id=?1 AND asset_id=?2 AND id=asset_id AND trashed=0",
        params![library,pick.asset_id], |r| r.get(0)).optional()?
        .ok_or_else(|| error(409, "照片已经移除或在回收站") )?;
    let available: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM catalog_version_paths p WHERE library_id=?1 AND asset_id=?2 AND path=?3 AND content_hash=?4 AND available=1 AND NOT EXISTS(SELECT 1 FROM catalog_deprecated_files x WHERE x.library_id=p.library_id AND x.path=p.path))",
        params![library,pick.asset_id,pick.keep_path,keep_hash], |r|r.get(0))?;
    ensure!(available, error(409, "保留版本当前不可用"));
    let (negative, any_edit, edit_uses_loser): (Option<String>, bool, bool) = db.query_row(
        "SELECT negative_hash,EXISTS(SELECT 1 FROM photo_edits WHERE library_id=?1 AND asset_id=?2),
        EXISTS(SELECT 1 FROM photo_edits WHERE library_id=?1 AND asset_id=?2 AND negative_hash=?3)
        FROM catalog_assets WHERE library_id=?1 AND id=?2",
        params![library,pick.asset_id,recycle_hash], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let changes_editing_negative =
        negative.as_deref() == Some(&recycle_hash) && keep_hash != recycle_hash;
    ensure!(
        keep_hash == recycle_hash || !(edit_uses_loser || (changes_editing_negative && any_edit)),
        error(409, "待回收文件被现有编辑使用，请先处理编辑底片")
    );
    let location = crate::catalog::photo_location(db, library, &pick.asset_id)?
        .context(error(409, "找不到图库显示目录"))?;
    let gallery = Path::new(&location)
        .parent()
        .context("missing gallery directory")?;
    let keep_sidecars = sidecars(keep)?;
    let recycle_sidecars = sidecars(recycle)?;
    let (destination, sidecars_to_keep, sidecars_to_recycle) = if keep.parent() == Some(gallery) {
        let exclusive = recycle_sidecars
            .into_iter()
            .filter(|p| !keep_sidecars.contains(p))
            .collect();
        (keep, keep_sidecars, exclusive)
    } else {
        ensure!(
            recycle.parent() == Some(gallery)
                && recycle.file_name() == keep.file_name()
                && keep_sidecars.is_empty(),
            error(
                409,
                "保留文件必须已在显示目录，或可按原名归位并沿用显示目录里的 XMP"
            )
        );
        ensure!(
            fs::metadata(keep)?.dev() == fs::metadata(recycle)?.dev(),
            error(409, "不可跨文件系统归位")
        );
        (recycle, recycle_sidecars, BTreeSet::new())
    };
    ensure!(!sidecars_to_keep.is_empty(), error(409, "保留版本缺少 XMP"));
    for path in sidecars_to_keep.iter().chain(&sidecars_to_recycle) {
        let identity = crate::identity::file_sidecar_photo_id(path)?;
        let registered: Option<String> = db.query_row(
            "SELECT content_hash FROM catalog_physical_files WHERE library_id=? AND asset_id=? AND path=? AND role='sidecar'",
            params![library, pick.asset_id, path.to_str()],
            |r| r.get(0),
        ).optional()?;
        let valid = match (identity.as_deref(), registered) {
            (Some(id), _) => id == photo_id,
            (None, Some(hash)) => crate::media::sha256_file(path)? == hash,
            _ => false,
        };
        ensure!(
            valid,
            error(409, format!("XMP 的照片身份不符：{}", path.display()))
        );
    }
    ensure!(
        sidecars_to_keep
            .iter()
            .map(|path| crate::identity::file_sidecar_photo_id(path))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .any(|id| id.as_deref() == Some(&photo_id)),
        error(409, "保留版本缺少带照片 ID 的 XMP")
    );
    let retained_files = std::iter::once(keep)
        .chain(sidecars_to_keep.iter().map(PathBuf::as_path))
        .map(File::inspect)
        .collect::<Result<_>>()?;
    let recycle_files = std::iter::once(recycle)
        .chain(sidecars_to_recycle.iter().map(PathBuf::as_path))
        .map(|p| p.to_str().context("path must be UTF-8").map(str::to_owned))
        .collect::<Result<_>>()?;
    for (path, digest) in [(keep, &keep_hash), (recycle, &recycle_hash)] {
        ensure!(
            crate::media::sha256_file(path)? == *digest,
            error(409, format!("文件内容与图库记录不符：{}", path.display()))
        );
    }
    let default_hash = db
        .query_row(
            "SELECT content_hash FROM catalog_defaults WHERE library_id=? AND asset_id=?",
            params![library, pick.asset_id],
            |r| r.get(0),
        )
        .optional()?;
    let changes_default =
        default_hash.as_deref() == Some(recycle_hash.as_str()) && keep_hash != recycle_hash;
    Ok(Selection {
        destination: destination.to_str().context("path must be UTF-8")?.into(),
        pick,
        keep_hash,
        recycle_hash,
        default_hash,
        changes_default,
        changes_editing_negative,
        photo_id,
        retained_files,
        recycle_files,
    })
}

pub(super) fn inventory(
    jobs: &Jobs,
    store: &Store,
    library: &str,
    picks: Vec<Pick>,
) -> Result<Plan> {
    let assets: Vec<_> = picks.iter().map(|p| p.asset_id.clone()).collect();
    ensure!(
        !assets.is_empty() && assets.len() == assets.iter().collect::<HashSet<_>>().len(),
        error(422, "每张照片须各选一个保留文件和一个待回收文件")
    );
    let indexed = {
        let db = store.lock()?;
        let mut lookup = db.prepare(
            "SELECT path,content_hash FROM catalog_paths WHERE library_id=?1 AND asset_id=?2 AND path=?3",
        )?;
        picks
            .into_iter()
            .map(|pick| indexed_pick(&mut lookup, library, pick))
            .collect::<Result<Vec<_>>>()?
    };
    let picked_paths = indexed
        .iter()
        .flat_map(|(pick, _, _)| {
            [
                PathBuf::from(&pick.keep_path),
                PathBuf::from(&pick.recycle_path),
            ]
        })
        .collect();
    validate_paths(jobs, library, &picked_paths)?;
    let versions = {
        let db = store.lock()?;
        indexed
            .into_iter()
            .map(|(pick, keep_hash, recycle_hash)| {
                selection(&db, library, pick, keep_hash, recycle_hash)
            })
            .collect::<Result<Vec<_>>>()?
    };
    let files: BTreeSet<_> = versions
        .iter()
        .flat_map(|v| v.recycle_files.iter().map(PathBuf::from))
        .collect();
    let retained: BTreeSet<_> = versions
        .iter()
        .flat_map(|v| v.retained_files.iter().map(|f| PathBuf::from(&f.path)))
        .collect();
    ensure!(
        files.is_disjoint(&retained)
            && files.len()
                == versions
                    .iter()
                    .map(|v| v.recycle_files.len())
                    .sum::<usize>(),
        error(409, "预览中的版本共用回收文件")
    );
    validate_paths(jobs, library, &retained)?;
    validate_paths(jobs, library, &files)?;
    validate_ownership(store, library, &assets, &files)?;
    Ok(Plan {
        keeps_trash: false,
        soft_delete: false,
        cutoff: None,
        asset_files: BTreeMap::new(),
        assets,
        files: files
            .iter()
            .map(|p| File::inspect(p))
            .collect::<Result<_>>()?,
        versions,
    })
}

pub(super) fn withdraw_recycled_paths(
    store: &Store,
    library: &str,
    selections: &[Selection],
    recycled: &[File],
) -> Result<()> {
    let recycled: HashSet<_> = recycled.iter().map(|f| f.path.as_str()).collect();
    let mut db = store.lock()?;
    let tx = db.transaction()?;
    for row in selections {
        for xmp in row
            .recycle_files
            .iter()
            .filter(|p| *p != &row.pick.recycle_path && recycled.contains(p.as_str()))
        {
            ensure!(
                !Path::new(xmp).try_exists()?,
                error(409, "待回收 XMP 路径重新出现，停止协调")
            );
            let owner: Option<(String,String)> = tx.query_row(
                "SELECT asset_id,role FROM catalog_physical_files WHERE library_id=? AND path=?",
                params![library,xmp], |r| Ok((r.get(0)?,r.get(1)?)),
            ).optional()?;
            ensure!(
                owner.is_none_or(|(asset, role)| asset == row.pick.asset_id && role == "sidecar"),
                error(409, "待回收 XMP 的图库归属已变化")
            );
            crate::physical_files::withdraw(&tx, library, xmp)?;
        }
    }
    for row in selections
        .iter()
        .filter(|row| recycled.contains(row.pick.recycle_path.as_str()))
    {
        let path = &row.pick.recycle_path;
        let asset = &row.pick.asset_id;
        ensure!(
            !Path::new(path).try_exists()?,
            error(409, "待回收路径重新出现，停止协调")
        );
        let indexed: Option<(String,String,String)> = tx.query_row(
            "SELECT asset_id,content_hash,role FROM catalog_paths WHERE library_id=? AND path=?",
            params![library,path], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).optional()?;
        if let Some((owner, hash, role)) = indexed {
            ensure!(
                owner == *asset && hash == row.recycle_hash,
                error(409, "待回收路径的图库归属已变化")
            );
            crate::versions::missing_path(&tx, library, asset, path)?;
            tx.execute(
                "UPDATE catalog_files AS f SET availability='missing' WHERE library_id=?1 AND asset_id=?2 AND content_hash=?3 AND role=?4 AND holder='keeps-nas' AND NOT EXISTS(SELECT 1 FROM catalog_paths p WHERE p.library_id=f.library_id AND p.asset_id=f.asset_id AND p.content_hash=f.content_hash AND p.role=f.role)",
                params![library,asset,hash,role],
            )?;
        }
        tx.execute(
            "DELETE FROM catalog_version_path_history WHERE library_id=? AND path=? AND asset_id=? AND content_hash=?",
            params![library,path,asset,row.recycle_hash],
        )?;
        if row.changes_default {
            crate::versions::select_default(&tx, library, asset, &row.keep_hash)?;
        }
        if row.changes_editing_negative {
            ensure!(tx.execute("UPDATE catalog_assets SET negative_hash=?4,edit_revision=edit_revision+1,last_edit_request=NULL
                 WHERE library_id=?1 AND id=?2 AND negative_hash=?3
                 AND NOT EXISTS(SELECT 1 FROM photo_edits WHERE library_id=?1 AND asset_id=?2)",
                params![library,asset,row.recycle_hash,row.keep_hash])? == 1
                || tx.query_row("SELECT negative_hash=?3 FROM catalog_assets WHERE library_id=?1 AND id=?2",
                    params![library,asset,row.keep_hash], |r|r.get::<_,bool>(0))?,
                error(409,"编辑底片已变化，停止归并"));
        }
    }
    crate::revisions::flush(&tx)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn finish(
    jobs: &Jobs,
    store: &Store,
    library: &str,
    selections: &[Selection],
) -> Result<()> {
    let mut moves = Vec::new();
    for row in selections {
        for file in &row.retained_files {
            ensure!(
                File::inspect(Path::new(&file.path))? == *file,
                error(409, "保留文件已变化，停止归位")
            );
        }
        if row.destination != row.pick.keep_path {
            moves.push(crate::asset_move::FileMove {
                id: uuid::Uuid::new_v4().to_string(),
                source: row.pick.keep_path.clone(),
                destination: row.destination.clone(),
            });
        }
    }
    for row in selections {
        ensure!(
            !Path::new(&row.pick.recycle_path).try_exists()?,
            error(409, "待回收路径重新出现，停止归位")
        );
    }
    crate::asset_move::preflight(jobs, store, library, &moves)?;
    crate::asset_move::move_files(jobs, store, library, moves, || Ok(()))?;
    for row in selections {
        let destination = Path::new(&row.destination);
        let db = store.read()?;
        let (directory, photo_id, default, negative): (String, String, Option<String>, Option<String>) = db.query_row(
            "SELECT p.directory,i.root_id,d.content_hash,a.negative_hash
            FROM catalog_physical_files p
            JOIN catalog_identity_roots i USING(library_id,asset_id)
            JOIN catalog_assets a ON a.library_id=p.library_id AND a.id=p.asset_id
            LEFT JOIN catalog_defaults d ON d.library_id=p.library_id AND d.asset_id=p.asset_id
            WHERE p.library_id=? AND p.asset_id=? AND p.path=? AND p.content_hash=? AND p.versioned=1",
            params![library,row.pick.asset_id,row.destination,row.keep_hash],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
        )?;
        let expected_default = if row.changes_default {
            Some(row.keep_hash.as_str())
        } else {
            row.default_hash.as_deref()
        };
        ensure!(
            destination.parent() == Some(Path::new(&directory))
                && photo_id == row.photo_id
                && expected_default.is_none_or(|hash| default.as_deref() == Some(hash))
                && (!row.changes_editing_negative || negative.as_deref() == Some(&row.keep_hash)),
            error(409, "保留版本的目录、照片身份或版本引用与预览不符")
        );
        ensure!(
            crate::media::sha256_file(destination)? == row.keep_hash,
            error(409, "归位后的文件内容与预览不符")
        );
        let mut canonical_xmp = false;
        for sidecar in &row.retained_files[1..] {
            let path = Path::new(&sidecar.path);
            ensure!(
                File::inspect(path)? == *sidecar,
                error(409, "保留 XMP 与预览不符")
            );
            if let Some(id) = crate::identity::file_sidecar_photo_id(path)? {
                ensure!(id == photo_id, error(409, "保留 XMP 的照片身份与图库不符"));
                canonical_xmp = true;
            }
        }
        ensure!(canonical_xmp, error(409, "保留版本缺少带照片 ID 的 XMP"));
    }
    store.reconcile_cache()?;
    Ok(())
}

async fn preview_api(
    State(s): State<Arc<AppState>>,
    RoutePath(lib): RoutePath<String>,
    Json(picks): Json<Vec<Pick>>,
) -> Result<Response, ApiError> {
    super::run(
        move || {
            let _guard = s.jobs.directory_mutation.read().unwrap();
            crate::directory_move::ensure_reconciled(&s.jobs)?;
            save_preview(&s.jobs, &lib, &inventory(&s.jobs, &s.store, &lib, picks)?)
        },
        StatusCode::OK,
    )
    .await
}

async fn confirm_api(
    State(s): State<Arc<AppState>>,
    RoutePath((lib, id)): RoutePath<(String, String)>,
    Json(body): Json<Confirmation>,
) -> Result<Response, ApiError> {
    super::run(
        move || {
            ensure!(
                !load(&s.jobs, &lib, &id)?.1.versions.is_empty(),
                error(404, "找不到版本回收任务")
            );
            submit(&s.jobs, &s.store, &lib, &id, body.confirmation_count)
        },
        StatusCode::ACCEPTED,
    )
    .await
}

async fn version_status_api(
    State(s): State<Arc<AppState>>,
    RoutePath((lib, id)): RoutePath<(String, String)>,
) -> Result<Response, ApiError> {
    super::run(
        move || {
            let (task, plan, _) = load(&s.jobs, &lib, &id)?;
            ensure!(!plan.versions.is_empty(), error(404, "找不到版本回收任务"));
            Ok(task)
        },
        StatusCode::OK,
    )
    .await
}

async fn draft_api(
    State(s): State<Arc<AppState>>,
    RoutePath((lib, asset)): RoutePath<(String, String)>,
) -> Result<Response, ApiError> {
    super::run(
        move || {
            let id: String = s
                .jobs
                .trash_db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT id FROM rejected_trash_tasks WHERE library_id=?1 AND
             status IN ('draft','pending','running') AND json_array_length(plan,'$.versions')>0
             AND EXISTS(SELECT 1 FROM json_each(plan,'$.assets') WHERE value=?2)
             ORDER BY (status='draft'),rowid DESC LIMIT 1",
                    params![lib, asset],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| error(404, "当前照片没有待确认或正在执行的版本回收任务"))?;
            Ok(load(&s.jobs, &lib, &id)?.0)
        },
        StatusCode::OK,
    )
    .await
}

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/libraries/{library}/version-recycle/preview",
            post(preview_api),
        )
        .route(
            "/libraries/{library}/version-recycle/draft/{asset}",
            get(draft_api),
        )
        .route(
            "/libraries/{library}/version-recycle/{id}",
            get(version_status_api).post(confirm_api),
        )
}
