//! Read-only, lazy navigation over each library's active tracking roots.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

use crate::{
    jobs::Jobs,
    store::{Store, StoreError},
};

pub type RootNames = BTreeMap<PathBuf, String>;

pub fn parse_root_names(root: &Path, raw: Option<&str>) -> Result<RootNames> {
    let Some(raw) = raw else {
        return Ok(RootNames::new());
    };
    let sources: BTreeMap<PathBuf, PathBuf> = serde_json::from_str(raw).context(
        "KEEPS_ORIGINAL_ROOT_SOURCES must be a JSON object of container paths to source paths",
    )?;
    let mut names = RootNames::new();
    for (path, source) in sources {
        if !path.is_absolute()
            || !path.starts_with(root)
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
            || !source.is_absolute()
        {
            bail!(
                "invalid KEEPS_ORIGINAL_ROOT_SOURCES entry for {}",
                path.display()
            );
        }
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty() && !name.contains('\0'))
            .context("source path must have a UTF-8 basename")?;
        names.insert(path, name.to_owned());
    }
    Ok(names)
}

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "DROP TRIGGER IF EXISTS directories_insert;
         DROP TABLE directories;
         CREATE TABLE directories(
            library_id TEXT NOT NULL, path TEXT NOT NULL CHECK(substr(path,1,1)='/'),
            parent_path TEXT, PRIMARY KEY(library_id,path),
            FOREIGN KEY(library_id,parent_path) REFERENCES directories(library_id,path)
                DEFERRABLE INITIALLY DEFERRED
         );
         DROP TABLE IF EXISTS catalog_sync_navigation;
         CREATE INDEX IF NOT EXISTS directories_parent ON directories(library_id,parent_path,path);
         CREATE TABLE IF NOT EXISTS directory_inventory_pending(id INTEGER PRIMARY KEY CHECK(id=1));
         CREATE TABLE IF NOT EXISTS folders(
           id TEXT PRIMARY KEY, library_id TEXT NOT NULL, path TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
           UNIQUE(library_id,path)
         );
         CREATE TABLE IF NOT EXISTS catalog_file_checks(
            library_id TEXT NOT NULL, path TEXT NOT NULL, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
            verified_hash TEXT NOT NULL, error TEXT, metadata_stamp TEXT NOT NULL, metadata_operation_id TEXT,
            identity_status TEXT NOT NULL DEFAULT 'pending', identity_error TEXT,
            PRIMARY KEY(library_id,path),
            FOREIGN KEY(library_id,path) REFERENCES catalog_physical_files(library_id,path)
                ON UPDATE CASCADE ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS directory_creations(
           library_id TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(library_id,path)
         );
         CREATE TRIGGER IF NOT EXISTS directories_added AFTER INSERT ON directories BEGIN
             INSERT OR IGNORE INTO catalog_revision_dirty VALUES(NEW.library_id,'navigation',NEW.path);
         END;
         CREATE TRIGGER IF NOT EXISTS directories_removed AFTER DELETE ON directories BEGIN
             INSERT OR IGNORE INTO catalog_revision_dirty VALUES(OLD.library_id,'navigation',OLD.path);
         END;
         CREATE TRIGGER IF NOT EXISTS directories_moved AFTER UPDATE ON directories
           WHEN NEW.path<>OLD.path OR NEW.parent_path IS NOT OLD.parent_path BEGIN
             INSERT OR IGNORE INTO catalog_revision_dirty VALUES(OLD.library_id,'navigation',OLD.path);
             INSERT OR IGNORE INTO catalog_revision_dirty VALUES(NEW.library_id,'navigation',NEW.path);
         END;",
    )?;
    let paths = db
        .prepare("SELECT DISTINCT library_id,directory FROM catalog_asset_directories")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (library, path) in paths {
        record(db, &library, Path::new(&path))?;
    }
    Ok(())
}

pub(crate) fn record(db: &Connection, library: &str, path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "directory path must be absolute");
    let mut insert = db.prepare_cached(
        "INSERT OR IGNORE INTO directories(library_id,path,parent_path) VALUES(?,?,?)",
    )?;
    for current in path.ancestors() {
        let changed = insert.execute(params![
            library,
            current.to_str().context("directory path must be UTF-8")?,
            current.parent().and_then(Path::to_str),
        ])?;
        if changed == 0 {
            break;
        }
    }
    Ok(())
}

pub(crate) fn registered(db: &Connection, library: &str, path: &Path) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM directories WHERE library_id=? AND path=?)",
        params![
            library,
            path.to_str().context("directory path must be UTF-8")?
        ],
        |row| row.get(0),
    )?)
}

pub(crate) fn move_tree(
    db: &Connection,
    library: &str,
    source: &str,
    destination: &str,
) -> Result<()> {
    if !registered(db, library, Path::new(source))? {
        return Ok(());
    }
    let parent = Path::new(destination)
        .parent()
        .context("destination has no parent")?;
    record(db, library, parent)?;
    db.execute(
        "UPDATE directories SET path=?3 || substr(path,length(?2)+1),
         parent_path=CASE WHEN path=?2 THEN ?6 ELSE ?3 || substr(parent_path,length(?2)+1) END
         WHERE library_id=?1 AND (path=?2 OR (path>=?4 AND path<?5))",
        params![
            library,
            source,
            destination,
            format!("{source}/"),
            format!("{source}0"),
            parent.to_str()
        ],
    )?;
    Ok(())
}

pub(crate) fn withdraw_missing(db: &Connection, library: &str, source: &Path) -> Result<()> {
    let source = source.to_str().context("directory path must be UTF-8")?;
    let delete = |path: &str| -> Result<()> {
        db.execute(
            "DELETE FROM directories WHERE library_id=?1 AND (path=?2 OR (path>=?3 AND path<?4))",
            params![library, path, format!("{path}/"), format!("{path}0")],
        )?;
        Ok(())
    };
    let missing = |path: &Path| -> Result<bool> {
        match path.symlink_metadata() {
            Ok(info) if info.is_dir() => Ok(false),
            Ok(_) => bail!("registered directory was replaced: {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e).with_context(|| format!("check directory {}", path.display())),
        }
    };
    if missing(Path::new(source))? {
        delete(source)?;
    } else {
        let mut statement = db.prepare(
            "SELECT path FROM directories WHERE library_id=?1 AND path>=?2 AND path<?3 ORDER BY path",
        )?;
        let paths = statement
            .query_map(
                params![library, format!("{source}/"), format!("{source}0")],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut removed: Option<PathBuf> = None;
        for path in paths {
            let current = Path::new(&path);
            if !removed
                .as_ref()
                .is_some_and(|ancestor| current.starts_with(ancestor))
                && missing(current)?
            {
                delete(&path)?;
                removed = Some(current.to_path_buf());
            }
        }
    }
    db.execute(
        "UPDATE folders SET active=0 WHERE library_id=?1 AND active=1 AND (path=?2 OR (path>=?3 AND path<?4))
          AND NOT EXISTS(SELECT 1 FROM directories d WHERE d.library_id=folders.library_id AND d.path=folders.path)",
        params![library,source,format!("{source}/"),format!("{source}0")],
    )?;
    Ok(())
}

pub(crate) fn roots(db: &Connection, library: &str) -> Result<Vec<PathBuf>> {
    let active = db
        .prepare_cached("SELECT path FROM folders WHERE library_id=? AND active=1 ORDER BY path")?
        .query_map([library], |row| row.get::<_, String>(0).map(PathBuf::from))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(active
        .iter()
        .filter(|root| {
            !active
                .iter()
                .any(|ancestor| ancestor != *root && root.starts_with(ancestor))
        })
        .cloned()
        .collect())
}

const CHILD_DIRECTORIES: &str = "SELECT d.path, EXISTS(SELECT 1 FROM directories child WHERE child.library_id=d.library_id AND child.parent_path=d.path)
         FROM directories d WHERE d.library_id=? AND d.parent_path=? ORDER BY d.path";

fn children(db: &Connection, library: &str, parent: &Path) -> Result<Vec<(String, bool)>> {
    let mut query = db.prepare_cached(CHILD_DIRECTORIES)?;
    Ok(query
        .query_map(
            params![
                library,
                parent.to_str().context("directory path must be UTF-8")?
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn page(
    db: &Connection,
    library: &str,
    root: &Path,
    roots: &[PathBuf],
    selected: Option<&Path>,
    names: &RootNames,
) -> Result<Value> {
    let nodes = if let Some(parent) = selected {
        children(db, library, parent)?
    } else if roots.iter().any(|path| path == root) {
        children(db, library, root)?
    } else {
        let mut nodes = Vec::new();
        for path in roots {
            let text = path.to_str().context("directory path must be UTF-8")?;
            let has_children = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM directories WHERE library_id=?1 AND parent_path=?2)",
                params![library, text],
                |r| r.get(0),
            )?;
            nodes.push((text.to_owned(), has_children));
        }
        nodes
    };
    let directories = nodes.into_iter().map(|(path, has_children)| {
        let file = Path::new(&path);
        let name = names.get(file).map(String::as_str).or_else(|| file.file_name().and_then(|name| name.to_str()))
            .context("directory name must be UTF-8")?;
        let (direct, recursive) = crate::catalog::directory_photo_counts(db, library, &path)?;
        Ok(json!({"path":path,"name":name,"hasChildren":has_children,"directPhotoCount":direct,"photoCount":recursive}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(json!({"path":selected,"directories":directories}))
}

pub(crate) fn navigation(
    store: &Store,
    jobs: &Jobs,
    library: &str,
    path: Option<&str>,
    names: &RootNames,
) -> Result<Value> {
    let mut db = store.read()?;
    let tx = db.transaction()?;
    let roots = roots(&tx, library)?;
    let selected = path.map(|path| -> Result<_> {
        let path = Path::new(path);
        let path = if path.is_absolute() { path.to_path_buf() } else { jobs.root().join(path) };
        let invalid = || StoreError { status:422, code:"invalid_directory".into(), message:"invalid managed directory path".into() };
        let relative = path.strip_prefix(jobs.root()).map_err(|_| invalid())?;
        ensure!(relative.components().all(|component| matches!(component, Component::Normal(name) if
            name.to_str().is_some_and(|name| !name.starts_with('.') && name != "@eaDir" && name != "#recycle")
        )), invalid());
        let normalized = relative.components().fold(jobs.root().to_path_buf(), |path, part| path.join(part.as_os_str()));
        if !roots.iter().any(|root| normalized.starts_with(root)) || !registered(&tx, library, &normalized)? {
            return Err(StoreError { status:404, code:"directory_not_tracked".into(), message:"directory is not registered in this library".into() }.into());
        }
        Ok(normalized)
    }).transpose()?;
    page(
        &tx,
        library,
        jobs.root(),
        &roots,
        selected.as_deref(),
        names,
    )
}

pub(crate) fn pages(
    db: &Connection,
    library: &str,
    root: &Path,
    roots: &[PathBuf],
    names: &RootNames,
) -> Result<BTreeMap<String, Value>> {
    let mut result = BTreeMap::new();
    let mut pending = vec![String::new()];
    while let Some(path) = pending.pop() {
        let selected = (!path.is_empty()).then(|| Path::new(&path));
        let value = page(db, library, root, roots, selected, names)?;
        for child in value["directories"]
            .as_array()
            .context("directory page missing children")?
        {
            pending.push(
                child["path"]
                    .as_str()
                    .context("directory missing path")?
                    .to_owned(),
            );
        }
        result.insert(path, value);
    }
    Ok(result)
}

#[cfg(test)]
pub(crate) fn fixture_directories(store: &Store, jobs: &Jobs, library: &str) -> Result<()> {
    let mut db = store.lock()?;
    let tx = db.transaction()?;
    for folder in jobs
        .folders()?
        .into_iter()
        .filter(|folder| folder.library_id == library)
    {
        tx.execute(
            "INSERT OR IGNORE INTO folders VALUES(?,?,?,?)",
            params![folder.id, folder.library_id, folder.path, folder.active],
        )?;
    }
    let mut pending = roots(&tx, library)?;
    while let Some(path) = pending.pop() {
        record(&tx, library, &path)?;
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().context("fixture directory name")?;
            if entry.file_type()?.is_dir()
                && !name.starts_with('.')
                && name != "@eaDir"
                && name != "#recycle"
            {
                pending.push(entry.path());
            }
        }
    }
    crate::revisions::flush(&tx)?;
    tx.commit()?;
    Ok(())
}

fn complete_creation(store: &Store, library: &str, path: &Path) -> Result<()> {
    let parent = path.parent().context("directory must have a parent")?;
    std::fs::File::open(parent)?.sync_all()?;
    let mut db = store.lock()?;
    let tx = db.transaction()?;
    record(&tx, library, path)?;
    tx.execute(
        "DELETE FROM directory_creations WHERE library_id=? AND path=?",
        params![library, path.to_str()],
    )?;
    crate::revisions::flush(&tx)?;
    tx.commit()?;
    Ok(())
}

fn forget_creation(store: &Store, library: &str, path: &Path) -> Result<()> {
    store.lock()?.execute(
        "DELETE FROM directory_creations WHERE library_id=? AND path=?",
        params![library, path.to_str()],
    )?;
    Ok(())
}

fn directory_exists() -> anyhow::Error {
    StoreError {
        status: 409,
        code: "directory_exists".into(),
        message: "destination directory already exists".into(),
    }
    .into()
}

pub(crate) fn create(store: &Store, library: &str, path: &Path) -> Result<()> {
    let pending = store.read()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM directory_creations WHERE library_id=? AND path=?)",
        params![library, path.to_str()],
        |r| r.get::<_, bool>(0),
    )?;
    if pending && recover_creation(store, library, path)? {
        return Ok(());
    }
    match path.symlink_metadata() {
        Ok(_) => return Err(directory_exists()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    store.lock()?.execute(
        "INSERT INTO directory_creations VALUES(?,?)",
        params![library, path.to_str()],
    )?;
    if let Err(error) = std::fs::create_dir(path) {
        forget_creation(store, library, path)?;
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(directory_exists());
        }
        return Err(error).with_context(|| format!("create {}", path.display()));
    }
    complete_creation(store, library, path)
}

fn recover_creation(store: &Store, library: &str, path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() => {
            complete_creation(store, library, path)?;
            Ok(true)
        }
        Ok(_) => bail!(
            "pending directory creation was replaced: {}",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            forget_creation(store, library, path)?;
            Ok(false)
        }
        Err(e) => Err(e).with_context(|| format!("check pending creation {}", path.display())),
    }
}

pub fn recover_creations(store: &Store, jobs: &Jobs) -> Result<()> {
    let db = store.read()?;
    let mut q =
        db.prepare("SELECT library_id,path FROM directory_creations ORDER BY library_id,path")?;
    let pending = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (library, path) in pending {
        let path = Path::new(&path);
        jobs.validate_path(path.parent().context("pending creation has no parent")?)?;
        recover_creation(store, &library, path)?;
    }
    Ok(())
}

pub(crate) fn tracking_changed(db: &Connection, library: &str, path: &str) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO catalog_revision_dirty VALUES(?,'navigation',?)",
        params![library, path],
    )?;
    crate::revisions::flush(db)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Inventory {
    library_id: String,
    schema_version: i64,
    roots: Vec<PathBuf>,
    issues: Vec<serde_json::Value>,
    directory_manifest: InventoryDirectories,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InventoryDirectories {
    catalog_revision: i64,
    directories: Vec<PathBuf>,
}

impl Store {
    pub fn require_directory_inventory(&self) -> Result<()> {
        let db = self.read()?;
        ensure!(
            !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM directory_inventory_pending)",
                [],
                |r| r.get::<_, bool>(0)
            )?,
            "directory inventory required: use keeps-server migrate with the verified schema 26 inventory for each library"
        );
        Ok(())
    }

    pub fn initialize_original_root(&self, library: &str, path: &Path) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        record(&tx, library, path)?;
        crate::revisions::flush(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn install_directory_inventory(&self, jobs: &Jobs, manifests: &[PathBuf]) -> Result<()> {
        let folders = jobs.folders()?;
        crate::directory_move::ensure_reconciled(jobs)?;
        crate::directory_trash::ensure_reconciled(jobs)?;
        let inventory = manifests
            .iter()
            .map(|path| {
                let report: Inventory = serde_json::from_reader(std::fs::File::open(path)?)
                    .with_context(|| format!("read directory inventory {}", path.display()))?;
                ensure!(
                    matches!(report.schema_version, 26..=crate::catalog::VERSION)
                        && report.issues.is_empty(),
                    "inventory contains unresolved differences: {}",
                    path.display()
                );
                Ok(report)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        let pending = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM directory_inventory_pending)",
            [],
            |r| r.get::<_, bool>(0),
        )?;
        if !pending {
            ensure!(
                inventory.is_empty(),
                "directory inventory is already installed"
            );
            return Ok(());
        }
        let mut present_roots = Vec::new();
        for folder in &folders {
            match Path::new(&folder.path).symlink_metadata() {
                Ok(info) => {
                    ensure!(
                        info.is_dir(),
                        "tracking root is not a real directory: {}",
                        folder.path
                    );
                    present_roots.push(folder);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !folder.active => (),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("check tracking root {}", folder.path));
                }
            }
        }
        let mut libraries = std::collections::BTreeSet::new();
        for report in &inventory {
            ensure!(
                libraries.insert(report.library_id.clone()),
                "duplicate directory inventory for {}",
                report.library_id
            );
            let covered: Vec<PathBuf> = present_roots
                .iter()
                .filter(|f| f.library_id == report.library_id)
                .map(|f| PathBuf::from(&f.path))
                .collect();
            ensure!(
                covered
                    .iter()
                    .all(|path| report.roots.iter().any(|root| path.starts_with(root)))
                    && report
                        .roots
                        .iter()
                        .all(|root| covered.iter().any(|path| root == path)),
                "inventory roots do not cover the saved tracking roots for {}",
                report.library_id
            );
            ensure!(
                !jobs.pending_file_publication(&report.library_id, None)?,
                "finish published file operations before inventory migration for {}",
                report.library_id
            );
            let revision: i64 = tx.query_row(
                "SELECT coalesce(max(revision),0) FROM catalog_version_revision WHERE library_id=?",
                [&report.library_id],
                |r| r.get(0),
            )?;
            ensure!(
                revision == report.directory_manifest.catalog_revision,
                "catalog revision changed since inventory for {}",
                report.library_id
            );
            let directories = &report.directory_manifest.directories;
            let indexed = tx
                .prepare(
                    "SELECT DISTINCT directory FROM catalog_physical_files WHERE library_id=?",
                )?
                .query_map([&report.library_id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let known: std::collections::BTreeSet<&Path> =
                directories.iter().map(PathBuf::as_path).collect();
            ensure!(
                report
                    .roots
                    .iter()
                    .all(|root| known.contains(root.as_path()))
                    && indexed.iter().all(|path| known.contains(Path::new(path))),
                "inventory omits registered directories for {}",
                report.library_id
            );
            for path in directories {
                ensure!(
                    report.roots.iter().any(|root| path.starts_with(root)),
                    "directory inventory path is outside its roots: {}",
                    path.display()
                );
                ensure!(
                    jobs.validate_path(path)? == *path,
                    "noncanonical inventory path: {}",
                    path.display()
                );
                record(&tx, &report.library_id, path)?;
            }
            tx.execute(
                "UPDATE catalog_sync_state SET generation=?2 WHERE library_id=?1",
                params![report.library_id, uuid::Uuid::new_v4().to_string()],
            )?;
            tx.execute(
                "DELETE FROM catalog_replicas WHERE library_id=?",
                [&report.library_id],
            )?;
        }
        ensure!(
            present_roots
                .iter()
                .all(|folder| libraries.contains(&folder.library_id)),
            "verified directory inventories are required for all tracked libraries"
        );
        let mut registered =
            tx.prepare("SELECT DISTINCT library_id FROM catalog_physical_files")?;
        let existing = registered
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            existing.iter().all(|library| libraries.contains(library)),
            "inventory omits registered media libraries"
        );
        drop(registered);
        for folder in &folders {
            tx.execute(
                "INSERT INTO folders VALUES(?,?,?,?)",
                params![folder.id, folder.library_id, folder.path, folder.active],
            )?;
        }
        tx.execute("DELETE FROM directory_inventory_pending", [])?;
        crate::revisions::flush(&tx)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_creation_recovers_without_importing_unrelated_directories() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?.join("photos");
        std::fs::create_dir(&root)?;
        let store = std::sync::Arc::new(Store::open(&fixture.path().join("catalog.sqlite"), true)?);
        let jobs = Jobs::open_managed(&fixture.path().join("jobs.sqlite"), &root, store.clone())?;
        jobs.add_folder("photos", ".")?;
        let target = root.join("empty");
        store.lock()?.execute_batch(
            "CREATE TRIGGER interrupt_creation BEFORE INSERT ON directories
          BEGIN SELECT RAISE(ABORT,'fixture failure after mkdir'); END;",
        )?;
        assert!(create(&store, "photos", &target).is_err());
        assert!(target.is_dir());
        assert!(!registered(&store.read()?, "photos", &target)?);
        let unknown = root.join("external");
        std::fs::create_dir(&unknown)?;
        store
            .lock()?
            .execute_batch("DROP TRIGGER interrupt_creation")?;
        recover_creations(&store, &jobs)?;
        recover_creations(&store, &jobs)?;
        let online = navigation(&store, &jobs, "photos", None, &RootNames::new())?;
        let db = store.read()?;
        let plan = db
            .prepare(&format!("EXPLAIN QUERY PLAN {CHILD_DIRECTORIES}"))?
            .query_map(params!["photos", root.to_str()], |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(
            plan.iter()
                .filter(|step| step.contains("SEARCH") && step.contains("directories_parent"))
                .count(),
            2,
            "{plan:?}"
        );
        let pages = pages(
            &db,
            "photos",
            &root,
            &roots(&db, "photos")?,
            &RootNames::new(),
        )?;
        assert_eq!(online, pages[""]);
        assert_eq!(online["directories"][0]["path"], json!(target));
        assert_eq!(online["directories"][0]["hasChildren"], false);
        assert!(online["directories"].as_array().unwrap().len() == 1);
        assert!(!registered(&db, "photos", &unknown)?);
        Ok(())
    }
}
