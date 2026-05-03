use std::fs::File;
use std::thread::sleep;
use std::time::Duration;
use tempfile::tempdir;
use ttfm::{FileManager, SearchOptions};

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

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 1. Initial Search (n=10)
    let options = SearchOptions {
        n: Some(10),
        ..Default::default()
    };
    // Query must match all files
    let res = fm.search("extension:txt", options)?;

    assert_eq!(res.results.len(), 10);
    assert!(res.has_more, "Should have more results");
    let cid = res.cid.expect("Should issue CID when has_more is true");

    // 2. Wait for Cache Worker
    let mut finished = false;
    for _ in 0..100 {
        // max 10s
        let res_cid = fm.search(
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
    };
    let res_page2 = fm.search("extension:txt", options_page2)?;

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
    let res_fresh_page2 = fm.search(
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
