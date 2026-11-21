use crate::catalog::AssetQuery;
use anyhow::{Result, anyhow};
use rusqlite::{Connection, params, types::Value};
use std::{collections::VecDeque, sync::Mutex};

pub(crate) fn excludes_hidden(db: &Connection, lib: &str, query: &AssetQuery) -> Result<bool> {
    if query.show_hidden.unwrap_or(false) {
        return Ok(false);
    }
    let Some(directory) = query.directory.as_deref() else {
        return Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM catalog_hidden_directories WHERE library_id=?)",
            [lib],
            |row| row.get(0),
        )?);
    };
    if query.recursive == Some(false) {
        return Ok(false);
    }
    let stem = directory.trim_end_matches('/');
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM catalog_hidden_directories
         WHERE library_id=? AND path>=? AND path<? AND path<>?)",
        params![
            lib,
            format!("{stem}/"),
            format!("{stem}0"),
            if stem.is_empty() { "/" } else { stem }
        ],
        |row| row.get(0),
    )?)
}

struct Entry {
    revision: i64,
    predicate: String,
    bindings: Vec<Value>,
    total: i64,
}

#[derive(Default)]
pub(crate) struct AssetTotals(Mutex<VecDeque<Entry>>);

impl AssetTotals {
    pub(crate) fn get_or_count(
        &self,
        revision: i64,
        predicate: &str,
        bindings: &[Value],
        count: impl FnOnce() -> Result<i64>,
    ) -> Result<i64> {
        let matches = |entry: &Entry| {
            entry.revision == revision && entry.predicate == predicate && entry.bindings == bindings
        };
        let lock = || {
            self.0
                .lock()
                .map_err(|_| anyhow!("asset totals mutex poisoned"))
        };
        if let Some(entry) = lock()?.iter().find(|entry| matches(entry)) {
            return Ok(entry.total);
        }
        let total = count()?;
        let mut entries = lock()?;
        if !entries.iter().any(matches) {
            if entries.len() == 128 {
                entries.pop_front();
            }
            entries.push_back(Entry {
                revision,
                predicate: predicate.into(),
                bindings: bindings.to_vec(),
                total,
            });
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{revisions, store::Store};
    use serde_json::json;

    #[test]
    fn misses_count_outside_the_lock_and_keep_at_most_128_results() -> Result<()> {
        let cache = AssetTotals::default();
        let bindings = ["photos".to_owned().into()];
        for revision in 0..129 {
            assert_eq!(
                cache.get_or_count(revision, "p", &bindings, || {
                    assert!(cache.0.try_lock().is_ok());
                    Ok(revision)
                })?,
                revision
            );
        }
        assert_eq!(cache.0.lock().unwrap().len(), 128);
        assert_eq!(
            cache.get_or_count(1, "p", &bindings, || panic!("already cached"))?,
            1
        );
        assert_eq!(cache.get_or_count(0, "p", &bindings, || Ok(1000))?, 1000);
        Ok(())
    }

    #[test]
    fn page_total_tracks_membership_independently_of_sort_and_cursor() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(&temp.path().join("db"), true)?;
        {
            let mut write = store.lock()?;
            let tx = write.transaction()?;
            for (library, id, path, rating) in [
                ("A", "a", "/照片/first.jpg", 4),
                ("A", "b", "/照片/sub/second.jpg", 3),
                ("B", "a", "/照片/third.jpg", 4),
            ] {
                tx.execute("INSERT INTO catalog_assets(library_id,id,snapshot,content_hash,fingerprint,sort_time,filename,rating,flag)
                    VALUES(?1,?2,json_object('id',?2),'h','','2026','photo.jpg',?3,'none')",params![library,id,rating])?;
                tx.execute(
                    "INSERT INTO catalog_physical_files(library_id,path,asset_id,content_hash,role)
                    VALUES(?1,?2,?3,'h','jpeg_original')",
                    params![library, path, id],
                )?;
            }
            revisions::flush(&tx)?;
            tx.commit()?;
        }
        let mut q = AssetQuery {
            min_rating: Some(3),
            limit: Some(1),
            ..Default::default()
        };
        let first = store.query_assets("A", &q)?;
        assert_eq!(first["total"], 2);
        assert_eq!(first["items"][0]["id"], "a");
        q.cursor = first["nextCursor"].as_str().map(str::to_owned);
        q.limit = Some(2);
        let second = store.query_assets("A", &q)?;
        assert_eq!(second["total"], 2);
        assert_eq!(second["items"][0]["id"], "b");
        assert!(second["nextCursor"].is_null());
        q.cursor = None;
        q.sort = Some("filename".into());
        assert_eq!(store.query_assets("A", &q)?["total"], 2);
        assert_eq!(store.asset_totals.0.lock().unwrap().len(), 1);
        assert_eq!(store.query_assets("B", &q)?["total"], 1);
        q.min_rating = Some(4);
        assert_eq!(store.query_assets("A", &q)?["total"], 1);
        q.min_rating = Some(3);
        store.set_hidden_directory("A", "/照片/sub", true)?;
        assert_eq!(store.query_assets("A", &q)?["total"], 1);
        q.directory = Some("/照片".into());
        assert_eq!(store.query_assets("A", &q)?["total"], 1);
        q.show_hidden = Some(true);
        assert_eq!(store.query_assets("A", &q)?["total"], 2);
        q.recursive = Some(false);
        assert_eq!(store.query_assets("A", &q)?["total"], 1);
        q.show_hidden = None;
        q.directory = Some("/照片/sub".into());
        assert_eq!(store.query_assets("A", &q)?["total"], 1);
        store.lock()?.execute(
            "UPDATE catalog_assets SET rating=1 WHERE library_id='A' AND id='b'",
            [],
        )?;
        let pending = store.query_assets("A", &q)?;
        assert_eq!(pending["total"], 0);
        assert_eq!(pending["items"], json!([]));
        let mut write = store.lock()?;
        let tx = write.transaction()?;
        revisions::flush(&tx)?;
        tx.commit()?;
        assert_eq!(store.query_assets("A", &q)?["total"], 0);
        Ok(())
    }
}
