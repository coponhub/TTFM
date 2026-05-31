// Copyright (C) 2026 coponhub
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use anyhow::Result;
use std::fs::File;
use tempfile::tempdir;
use ttfm::search;
use ttfm::{SearchOptions};

#[test]
fn test_search_progress_zero_results() -> Result<()> {
    let dir = tempdir()?;
    let db_dir = dir.path().join("db");
    std::fs::create_dir(&db_dir)?;
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables()?;
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);

    // 0件検索 (name:non-existent)
    let res = search::search(&store, &registry, &cache, "name:non_existent", SearchOptions::default())?;

    // 期待値:
    // 1. total_count が Some(0) であること
    // 2. has_more が false であること
    // 3. progress.is_finished() が true であること
    assert_eq!(
        res.total_count,
        Some(0),
        "total_count should be Some(0) for zero results"
    );
    assert_eq!(res.has_more, false, "has_more should be false");

    // Progress構造体の改修後、is_finished() は is_done フラグを見るようになるため
    // total が None であっても is_done = true なら is_finished() = true となるべき
    assert!(
        res.progress.is_finished(),
        "progress should be finished for zero results"
    );

    Ok(())
}

#[test]
fn test_search_progress_finished_small_results() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join("db");
    std::fs::create_dir(&db_dir)?;

    // データ作成
    File::create(root.join("test.txt"))?;

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables()?;
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry).run(root, None::<&fn(usize)>, false)?;

    // 少数ヒット (1件)
    let res = search::search(&store, &registry, &cache, "extension:txt", SearchOptions::default())?;

    assert_eq!(res.results.len(), 1);

    // 期待値:
    // 1. total_count が Some(1) であること
    // 2. has_more が false であること
    // 3. progress.is_finished() が true であること
    assert_eq!(
        res.total_count,
        Some(1),
        "total_count should be Some(1) for small results"
    );
    assert_eq!(res.has_more, false, "has_more should be false");
    assert!(
        res.progress.is_finished(),
        "progress should be finished for small results"
    );

    Ok(())
}

#[test]
fn test_progress_struct_behavior() {
    use ttfm::Progress;

    // case 1: total=None, is_done=false -> unfinished
    let p = Progress {
        current: 0,
        total: None,
        is_done: false,
    };
    assert!(!p.is_finished());

    // case 2: total=None, is_done=true -> finished
    let p_done = Progress {
        current: 0,
        total: None,
        is_done: true,
    };
    assert!(p_done.is_finished());

    // case 3: total=Some(10), current=5, is_done=false -> unfinished
    let p_mid = Progress {
        current: 5,
        total: Some(10),
        is_done: false,
    };
    assert!(!p_mid.is_finished());

    // case 4: total=Some(10), current=10, but is_done must be explicitly set
    // total based check is removed in new implementation?
    // Based on implementation plan:
    // pub fn is_finished(&self) -> bool { self.is_done }
    // So even if current >= total, if is_done is false, it returns false.
    // This assumes that whoever sets total also sets is_done correctly.
}
