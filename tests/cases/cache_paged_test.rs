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
use std::thread::sleep;
use std::time::Duration;
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

    // 2. Wait for Cache Worker
    let mut finished = false;
    for _ in 0..100 {
        // max 10s
        let res_cid = search::search_nowarn(
            &store,
            &registry,
            "extension:txt",
            SearchOptions {
                cid: Some(cid.clone()),
                n: Some(10),
                ..Default::default()
            },
        )?;

        if res_cid.progress.is_finished() {
            finished = true;
            break;
        }
        sleep(Duration::from_millis(100));
    }

    assert!(finished, "Cache worker did not finish in time or failed");

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
