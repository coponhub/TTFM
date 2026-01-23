use std::fs::File;
use tempfile::tempdir;
use ttfm::{FileManager, SearchOptions};

#[test]
fn test_search_all_no_paging() -> anyhow::Result<()> {
    // Override TTFM_HOME to point to temp dir
    let dir = tempdir()?;
    let root = dir.path();
    unsafe {
        std::env::set_var("TTFM_HOME", root.join(".ttfm"));
    }

    let db_dir = root.join(".ttfm/db");

    // Create 25 files
    for i in 0..25 {
        File::create(root.join(format!("file_{:03}.txt", i)))?;
    }

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // Search all (n=None) -> Should retrieve all 25 items without paging
    let options = SearchOptions {
        n: None,
        ..Default::default()
    };
    // Query must match all files
    let res = fm.search("extension:txt", options)?;

    assert_eq!(res.results.len(), 25, "Should retrieve all 25 items");
    assert!(!res.has_more, "Should not have more results when n is None");
    
    Ok(())
}
