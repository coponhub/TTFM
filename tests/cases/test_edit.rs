use tempfile::TempDir;
use ttfm::{
    CacheManager, SearchOptions,
    db::Store,
    edit::{edit, QueryType, WriteOptions},
    indexing::Indexer,
    tag::TagRegistry,
};

fn setup() -> (Store, TagRegistry, CacheManager, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("foo.txt"), "content").unwrap();

    let db_dir = base.join("db");
    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();
    let cache = CacheManager::new(db_dir.join("cache"), 0);
    (store, registry, cache, dir)
}

// edit → tag: filename:foo.txt に project:A を付与し、search で確認できる
#[test]
fn edit_tag_adds_user_tag() -> anyhow::Result<()> {
    let (store, registry, cache, _dir) = setup();

    let resp = edit(
        &store,
        &registry,
        &cache,
        "filename:foo.txt",
        "project:A",
        QueryType::Tag,
        None,
        WriteOptions::default(),
    )?;
    assert_eq!(resp.added, 1);

    let results = ttfm::search::search(
        &store,
        &registry,
        &cache,
        "project:A",
        SearchOptions::default(),
    )?;
    assert_eq!(results.results.len(), 1);
    Ok(())
}

// edit → untag: 付与済みの project:A を削除し、search で 0 件になる
#[test]
fn edit_untag_removes_user_tag() -> anyhow::Result<()> {
    let (store, registry, cache, _dir) = setup();

    edit(
        &store,
        &registry,
        &cache,
        "filename:foo.txt",
        "project:A",
        QueryType::Tag,
        None,
        WriteOptions::default(),
    )?;

    let resp = edit(
        &store,
        &registry,
        &cache,
        "filename:foo.txt",
        "project:A",
        QueryType::Untag,
        None,
        WriteOptions::default(),
    )?;
    assert_eq!(resp.deleted, 1);

    let results = ttfm::search::search(
        &store,
        &registry,
        &cache,
        "project:A",
        SearchOptions::default(),
    )?;
    assert_eq!(results.results.len(), 0);
    Ok(())
}
