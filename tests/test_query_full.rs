use ttfm::search;
use tempfile::tempdir;

#[test]
fn test_binder_error() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let db_dir = dir.path().join(".ttfm/db");

    // Create test files
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.rs"), "a")?;
    std::fs::write(src_dir.join("b.txt"), "b")?;

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables()?;
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry).run(dir.path(), None::<&fn(usize)>, false)?;

    let q = r#"((parentdir: &: count(extension:rs)) / (parentdir: &: count())) :> 1"#;
    match search::search(&store, &registry, &cache, q, Default::default()) {
        Ok(res) => eprintln!("SUCCESS: {:?}", res),
        Err(e) => {
            eprintln!("ERROR: {}", e);
            panic!("Search failed");
        }
    }
    Ok(())
}
