use crate::{
    api::{AppState, router},
    asset_move::{FileMove, reconcile_files},
    catalog,
    jobs::Jobs,
    previews::PreviewStorage,
    store::Store,
};
use anyhow::Result;
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc, time::Instant};
use tower::ServiceExt;

const KEY: &str = "test-preview-key-01234567890123456789";
const DB: &str = "keeps/db/control_plane.sqlite";

fn state(home: &Path, originals: &Path) -> Result<Arc<AppState>> {
    let previews = PreviewStorage::new(
        &home.join("keeps"),
        Some(originals),
        "http://localhost",
        KEY,
    )?;
    let store = Arc::new(Store::open(&home.join(DB), true)?);
    let jobs = Arc::new(Jobs::open_managed(
        &home.join("keeps/db/jobs.sqlite"),
        originals,
        store.clone(),
    )?);
    Ok(Arc::new(AppState {
        store,
        previews: Arc::new(previews),
        jobs,
        access_token: KEY.into(),
        library_id: "photos".into(),
        original_root_names: Default::default(),
    }))
}

fn seed(home: &Path) -> Result<()> {
    let originals = home.join("originals");
    fs::create_dir_all(&originals)?;
    let state = state(home, &originals)?;
    state.jobs.add_folder("photos", ".")?;
    let root = state.jobs.root().to_str().unwrap();
    for branch in 0..3 {
        for leaf in 0..18 {
            std::fs::create_dir_all(format!("{root}/{branch:02}/{leaf:02}"))?;
        }
    }
    let mut db = state.store.lock()?;

    let tx = db.transaction()?;
    tx.execute_batch("
        WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<152745)
        INSERT INTO catalog_assets(library_id,id,snapshot,content_hash,fingerprint,sort_time,filename,rating,flag,trashed)
        SELECT 'photos',printf('%032x',i),json_object('id',printf('%032x',i),'originalFilename','sample.jpg','rating',0,'flagState','unflagged','tags',json('[]'),'trashed',json(CASE WHEN i%300=0 THEN 'true' ELSE 'false' END)),'hash','fingerprint',printf('2026-01-%02dT00:00:00Z',i%28+1),'sample.jpg',0,'unflagged',i%300=0 FROM n;
    ")?;
    tx.execute("
        WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<152745)
        INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role,versioned)
        SELECT 'photos',printf('%s/%02d/%02d/%032x.jpg',?1,i%3,(i/3)%18,i),printf('%032x',i),'hash','jpeg_original',1 FROM n
    ", [root])?;
    tx.execute("INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role,versioned) SELECT library_id,replace(path,'.jpg','.raw'),asset_id,'raw','raw_original',1 FROM catalog_physical_files WHERE substr(asset_id,-1) IN('0','2','4','6','8','a','c','e')", [])?;
    tx.execute("INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role) SELECT library_id,path||'.xmp',asset_id,'xmp','sidecar' FROM catalog_physical_files WHERE role='jpeg_original'", [])?;
    tx.execute("INSERT INTO catalog_deprecated_files SELECT library_id,path,path,'identity','duplicate' FROM catalog_physical_files WHERE role<>'sidecar' AND rowid%31=0", [])?;
    crate::revisions::flush(&tx)?;
    tx.commit()?;

    Ok(())
}

async fn get(app: &Router, path: &str) -> Result<Value> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", format!("Bearer {KEY}"))
                .body(Body::empty())?,
        )
        .await?;
    let status = response.status();
    let body: Value = serde_json::from_slice(&response.into_body().collect().await?.to_bytes())?;
    assert!(status.is_success(), "{path}: {body}");
    Ok(body)
}

fn report(label: &str, durations: &[f64]) {
    let mut steady = durations[3..].to_vec();
    steady.sort_by(f64::total_cmp);
    println!(
        "{label} first_ms={:.3} p50_ms={:.3} p95_ms={:.3}",
        durations[0],
        (steady[10] + steady[11]) / 2.,
        steady[20]
    );
}

async fn reads(shared: &Arc<AppState>, home: &Path, originals: &Path) -> Result<()> {
    let app = router(shared.clone());
    let root = originals.to_str().unwrap();
    let all = "/libraries/photos/assets?limit=100";
    let filtered = format!("{all}&flagState=unflagged");
    let db = shared.store.read()?;
    let expected_ids=db.prepare("SELECT id FROM catalog_assets WHERE library_id='photos' AND trashed=0 ORDER BY sort_time DESC,id LIMIT 200")?.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut second = String::new();
    for (name, url) in [
        ("root", "/libraries/photos/navigation".into()),
        (
            "branch",
            format!("/libraries/photos/navigation?path={root}/00"),
        ),
        ("first_page", all.into()),
        ("second_page", String::new()),
        ("filtered_miss", filtered),
    ] {
        let mut durations = Vec::new();
        let url = if name == "second_page" {
            second.clone()
        } else {
            url
        };
        for _ in 0..25 {
            let fresh;
            let route = if name == "filtered_miss" {
                fresh = router(Arc::new(AppState {
                    store: Arc::new(Store::open(&home.join(DB), false)?),
                    previews: shared.previews.clone(),
                    jobs: shared.jobs.clone(),
                    access_token: KEY.into(),
                    library_id: "photos".into(),
                    original_root_names: Default::default(),
                }));
                &fresh
            } else {
                &app
            };
            let start = Instant::now();
            let body = get(route, &url).await?;
            durations.push(start.elapsed().as_secs_f64() * 1000.);
            if name == "root" || name == "branch" {
                let nodes = body["directories"].as_array().unwrap();
                assert_eq!(nodes.len(), if name == "root" { 3 } else { 18 });
                let total: i64 = nodes
                    .iter()
                    .map(|n| n["photoCount"].as_i64().unwrap())
                    .sum();
                assert_eq!(total, if name == "root" { 152236 } else { 50406 });
            } else {
                assert_eq!(body["total"], 152236);
                let start = if name == "second_page" { 100 } else { 0 };
                let ids: Vec<_> = body["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a["id"].as_str().unwrap())
                    .collect();
                assert_eq!(ids, expected_ids[start..start + 100]);
                if name == "first_page" {
                    second = format!("{all}&cursor={}", body["nextCursor"].as_str().unwrap());
                }
            }
        }
        report(name, &durations);
    }
    Ok(())
}

fn writes(state: &AppState, home: &Path) -> Result<()> {
    let source = home.join("source");
    let target = home.join("target");
    fs::create_dir(&source)?;
    fs::create_dir(&target)?;
    let mut imports = Vec::new();
    let mut moves = Vec::new();
    for batch in 0..25 {
        let mut files = Vec::new();
        let mut metadata = Vec::new();
        for index in 0..50 {
            let name = format!("bench-{batch}-{index}.jpg");
            let id = uuid::Uuid::from_u128(1_000_000 + batch * 50 + index).to_string();
            let file = json!({"role":"jpeg_original","contentHash":name,"sizeBytes":1});
            let asset = json!({"photoID":id,"contentFingerprint":name,"metadataFingerprint":"meta","createdAt":"2026","updatedAt":"2026","originalFilename":name,"rating":0,"flagState":"unflagged","tags":[]});
            let src = source.join(&name);
            let dst = target.join(&name);
            fs::write(&dst, b"x")?;
            files.push(FileMove {
                id,
                source: src.to_str().unwrap().into(),
                destination: dst.to_str().unwrap().into(),
            });
            metadata.push((file, asset));
        }
        let start = Instant::now();
        {
            let mut db = state.store.lock()?;
            let tx = db.transaction()?;
            for (f, (file, asset)) in files.iter().zip(metadata.iter()) {
                catalog::ingest_original_in_transaction(
                    &tx,
                    "photos",
                    &f.source,
                    file,
                    asset,
                    &Default::default(),
                )?;
            }
            crate::revisions::flush(&tx)?;
            tx.commit()?;
        }
        imports.push(start.elapsed().as_secs_f64() * 1000.);
        let start = Instant::now();
        reconcile_files(&state.jobs, &state.store, "photos", &files)?;
        moves.push(start.elapsed().as_secs_f64() * 1000.);
        assert_eq!(
            state.store.directory_photo_counts(
                "photos",
                &[
                    source.to_str().unwrap().into(),
                    target.to_str().unwrap().into()
                ]
            )?,
            vec![
                (0, 0),
                (((batch + 1) * 50) as i64, ((batch + 1) * 50) as i64)
            ]
        );
    }
    report("publish_50", &imports);
    report("move_50", &moves);
    Ok(())
}

#[tokio::test]
#[ignore = "benchmarks a synthetic 152745-photo SQLite catalog"]
async fn gallery_counts_and_paging() -> Result<()> {
    let work = tempfile::tempdir()?;
    let fixture = std::env::var_os("KEEPS_GALLERY_BENCH_FIXTURE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| work.path().join("fixture"));
    let marker = fixture.join(".keeps-synthetic-benchmark-v1");
    if !fixture.try_exists()? {
        fs::create_dir(&fixture)?;
        seed(&fixture)?;
        fs::write(&marker, b"152745")?;
    }
    anyhow::ensure!(
        fs::read(&marker)? == b"152745",
        "expected a generated gallery benchmark fixture"
    );
    let home = work.path().join("run");
    fs::create_dir_all(home.join("keeps/db"))?;
    let original = Connection::open_with_flags(fixture.join(DB), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    original.backup("main", home.join(DB), None)?;
    drop(original);
    let size = |db: &Connection| {
        db.query_row(
            "SELECT page_count*page_size,(page_count-freelist_count)*page_size
             FROM pragma_page_count(),pragma_page_size(),pragma_freelist_count()",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
    };
    let path = home.join(DB);
    let db = Connection::open(&path)?;
    let (bytes_before, occupied_before) = size(&db)?;
    let schema: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    drop(db);
    let start = Instant::now();
    let migrated = Store::open(&path, true)?;
    let ms = start.elapsed().as_secs_f64() * 1000.;
    let db = migrated.read()?;
    let new: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let (bytes, occupied) = size(&db)?;
    println!(
        "migration from={schema} to={new} elapsed_ms={ms:.3} delta_bytes={} delta_occupied_bytes={}",
        bytes - bytes_before,
        occupied - occupied_before
    );
    assert!(
        db.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none()
    );
    drop(db);
    drop(migrated);
    let originals = fixture.join("originals").canonicalize()?;
    let state = state(&home, &originals)?;
    reads(&state, &home, &originals).await?;
    writes(&state, &home)
}
