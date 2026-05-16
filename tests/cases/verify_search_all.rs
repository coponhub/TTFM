use std::fs::File;
use tempfile::tempdir;
use ttfm::search;
use ttfm::{SearchOptions};

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

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables()?;
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry).run(root, None::<&fn(usize)>, false)?;

    // Search all (n=None) -> Should retrieve all 25 items without paging
    let options = SearchOptions {
        n: None,
        ..Default::default()
    };
    // Query must match all files
    let res = search::search(&store, &registry, &cache, "extension:txt", options)?;

    assert_eq!(res.results.len(), 25, "Should retrieve all 25 items");
    assert!(!res.has_more, "Should not have more results when n is None");

    Ok(())
}
