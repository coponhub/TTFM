// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
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

use std::fs::File;
use tempfile::tempdir;
use ttfm::search;
use ttfm::SearchOptions;

#[test]
fn test_search_cache_flow() -> anyhow::Result<()> {
    // Override TTFM_HOME to point to temp dir
    let dir = tempdir()?;
    // 物理パスを解決して絶対パス化
    let root = dir.path().canonicalize()?;
    unsafe {
        std::env::set_var("TTFM_HOME", root.join(".ttfm"));
    }

    let db_dir = root.join(".ttfm/db");

    // Create 110 files to trigger has_more with n=100 (default)
    // or we use n=10 to trigger it with fewer files.
    for i in 0..25 {
        File::create(root.join(format!("file_{:03}.txt", i)))?;
    }

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    // 1. Initial Search (n=10)
    let options = SearchOptions {
        n: Some(10),
        ..Default::default()
    };
    // Query must match all files
    let res =
        search::search_nowarn(&store, &registry, "extension:txt", options)?;

    assert_eq!(res.results.len(), 10);
    assert!(res.has_more, "Should have more results");
    let cid = res.cid.expect("Should issue CID when has_more is true");

    // 2. Generate Cache
    ttfm::search::run_cache_worker(
        store.db_dir.clone(),
        &cid,
        "extension:txt",
    )?;

    // 3. Second Search (Next Page) via Cache
    let options_page2 = SearchOptions {
        n: Some(10),
        offset: Some(10),
        cid: Some(cid.clone()),
        ..Default::default()
    };
    let res_page2 = search::search_nowarn(
        &store,
        &registry,
        "extension:txt",
        options_page2,
    )?;

    assert_eq!(
        res_page2.results.len(),
        10,
        "Page 2 should have 10 items from cache"
    );
    assert!(
        res_page2.progress.is_done,
        "Page 2 from cache must be marked as done"
    );
    assert!(
        res_page2.cid.is_some(),
        "CID should persist for cache-based paging"
    );
    assert_eq!(res_page2.cid.unwrap(), cid, "CID should remain same");

    // 4. Verify data consistency (Sort order should be rank DESC, item_id DESC)
    // Results from cache should match what we expect from a fresh search
    let res_fresh_page2 = search::search_nowarn(
        &store,
        &registry,
        "extension:txt",
        SearchOptions {
            n: Some(10),
            offset: Some(10),
            ..Default::default()
        },
    )?;

    assert_eq!(res_page2.results.len(), res_fresh_page2.results.len());
    for i in 0..10 {
        assert_eq!(
            res_page2.results[i].id, res_fresh_page2.results[i].id,
            "Mismatch at index {}",
            i
        );
    }

    Ok(())
}

#[test]
fn test_run_cache_worker_direct() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path().canonicalize()?;
    let db_dir = root.join(".ttfm/db");
    for i in 0..5 {
        File::create(root.join(format!("file_{:03}.txt", i)))?;
    }
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;
    let cid = "test-direct-cid";
    ttfm::search::run_cache_worker(store.db_dir.clone(), cid, "extension:txt")?;
    let cache = ttfm::search::CacheManager::new(store.db_dir.join("cache"), 0);
    assert!(cache.path_for(cid).exists());
    let res = search::search_nowarn(
        &store,
        &registry,
        "extension:txt",
        SearchOptions {
            n: Some(2),
            offset: Some(2),
            cid: Some(cid.to_string()),
            ..Default::default()
        },
    )?;
    assert_eq!(res.results.len(), 2);
    Ok(())
}

#[test]
fn test_cacher_fallback_when_generating() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path().canonicalize()?;
    let db_dir = root.join(".ttfm/db");
    for i in 0..5 {
        File::create(root.join(format!("file_{:03}.txt", i)))?;
    }
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;
    let res = search::search_nowarn(
        &store,
        &registry,
        "extension:txt",
        SearchOptions {
            n: Some(2),
            offset: Some(2),
            cid: Some("generating-cid".to_string()),
            ..Default::default()
        },
    )?;
    assert_eq!(res.results.len(), 2);
    Ok(())
}

#[test]
fn test_run_cache_worker_empty_query() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path().canonicalize()?;
    let db_dir = root.join(".ttfm/db");
    File::create(root.join("a.txt"))?;
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;
    let cid = "test-all-cid";
    ttfm::search::run_cache_worker(store.db_dir.clone(), cid, "*:*")?;
    let cache = ttfm::search::CacheManager::new(store.db_dir.join("cache"), 0);
    assert!(cache.path_for(cid).exists());
    Ok(())
}

#[test]
#[ignore = "実プロセス起動を伴うエンドツーエンドキャッシュ生成テスト"]
fn test_e2e_detached_cache_worker_process() -> anyhow::Result<()> {
    let ttfm_bin = env!("CARGO_BIN_EXE_ttfm");

    let dir = tempdir()?;
    let root = dir.path().canonicalize()?;
    let ttfm_home = root.join(".ttfm");

    for i in 0..10 {
        File::create(root.join(format!("file_{:02}.txt", i)))?;
    }

    // 1. Initialize & Index
    let status = std::process::Command::new(ttfm_bin)
        .env("TTFM_HOME", &ttfm_home)
        .arg("index")
        .arg(&root)
        .status()?;
    assert!(status.success(), "ttfm index must succeed");

    // 2. Search Page 1 (triggers detached background worker)
    let output = std::process::Command::new(ttfm_bin)
        .env("TTFM_HOME", &ttfm_home)
        .arg("search")
        .arg("extension:txt")
        .arg("-n")
        .arg("2")
        .output()?;
    assert!(output.status.success(), "ttfm search page 1 must succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let cid_line = stdout
        .lines()
        .find(|l| l.contains("--cid"))
        .expect("Search output should contain --cid");
    let cid = cid_line.split("--cid").nth(1).unwrap().trim().to_string();

    // 3. Wait for background worker to create <cid>.parquet
    let cache_parquet =
        ttfm_home.join("db/cache").join(format!("{}.parquet", cid));
    let mut finished = false;
    for _ in 0..50 {
        if cache_parquet.exists() {
            finished = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        finished,
        "Background worker should generate parquet file within 5s"
    );

    // 4. Search Page 2 via CID (resolves from parquet cache)
    let output_page2 = std::process::Command::new(ttfm_bin)
        .env("TTFM_HOME", &ttfm_home)
        .arg("search")
        .arg("extension:txt")
        .arg("--cid")
        .arg(&cid)
        .arg("-n")
        .arg("2")
        .arg("--offset")
        .arg("2")
        .output()?;
    assert!(
        output_page2.status.success(),
        "ttfm search page 2 via CID must succeed"
    );
    let stdout_page2 = String::from_utf8_lossy(&output_page2.stdout);
    assert!(
        !stdout_page2.contains("Background cache generating"),
        "Completed cache must not show generating message"
    );
    assert!(
        stdout_page2.contains(&cid),
        "Page 2 output must preserve the CID"
    );
    assert!(
        stdout_page2.contains("Total: 2 results displayed"),
        "Page 2 must display 2 results from cache"
    );

    Ok(())
}
