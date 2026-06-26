use tempfile::TempDir;
use ttfm::{
    CacheManager, SearchOptions,
    db::{Store, TargetTable},
    edit::{edit, modify::modify, write::write_and_refresh, QueryType, WriteOptions},
    indexing::Indexer,
    tag::TagRegistry,
    types::{SType, TagType},
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

// Volatile な tag 定義に EditQuery → DB に登録されて rank が付く
#[test]
fn modify_volatile_tag_def_gets_registered_with_rank() -> anyhow::Result<()> {
    let (store, registry, cache, _dir) = setup();

    // foo.txt に project:A を付与して tag 定義を作る
    edit(&store, &registry, &cache, "filename:foo.txt", "project:A", QueryType::Tag, None, WriteOptions::default())?;

    // tag:"project:A" → Volatile な tag 定義アイテムが返る
    let results = ttfm::search::search(&store, &registry, &cache, "tag:\"project:A\"", SearchOptions::default())?;
    assert_eq!(results.results.len(), 1);
    let item = &results.results[0];
    assert!(item.id.is_volatile(), "tag def should be Volatile before registration");
    assert_eq!(item.representative.first().map(|l| l.tag_type()), Some(TagType::Base(SType::TypedTag)));

    // modify → write で登録 + rank 付与
    let actions = modify(item, Some("rank:100"), QueryType::Tag, &registry)?;
    write_and_refresh(&store, &registry, actions)?;

    // Stored になり rank が付いている
    let results2 = ttfm::search::search(&store, &registry, &cache, "tag:\"project:A\"", SearchOptions::default())?;
    assert_eq!(results2.results.len(), 1);
    assert!(results2.results[0].id.is_stored(), "tag def should be Stored after registration");
    assert_eq!(results2.results[0].rank, 100);
    Ok(())
}

// Volatile な Projection (parentdir:) に EditQuery → note として登録される
#[test]
fn modify_volatile_projection_gets_registered_as_note() -> anyhow::Result<()> {
    let (store, registry, cache, _dir) = setup();

    // parentdir: → Volatile な Projection アイテムが返る
    let results = ttfm::search::search(&store, &registry, &cache, "parentdir:", SearchOptions::default())?;
    assert!(!results.results.is_empty());
    let item = &results.results[0];
    assert!(item.id.is_volatile(), "parentdir projection should be Volatile");

    // modify → write で note として登録 + project:archived 付与
    let actions = modify(item, Some("project:archived"), QueryType::Tag, &registry)?;
    write_and_refresh(&store, &registry, actions)?;

    // item_references に note 行が存在する
    let path = store.path_for_target(TargetTable::ItemReferences);
    let sql = format!(
        "SELECT COUNT(*) FROM read_parquet('{}') WHERE item_kind = 'note'",
        path.to_string_lossy()
    );
    let count: i64 = store.conn.query_row(&sql, [], |r| r.get(0))?;
    assert_eq!(count, 1, "one note item should exist in item_references");
    Ok(())
}

// EditQuery なし (§5.7) で Volatile な tag 定義を登録のみ
#[test]
fn modify_volatile_tag_def_no_edit_query_registers_only() -> anyhow::Result<()> {
    let (store, registry, cache, _dir) = setup();

    edit(&store, &registry, &cache, "filename:foo.txt", "project:A", QueryType::Tag, None, WriteOptions::default())?;

    let results = ttfm::search::search(&store, &registry, &cache, "tag:\"project:A\"", SearchOptions::default())?;
    let item = &results.results[0];
    assert!(item.id.is_volatile());

    // None → 登録 Add のみ生成
    let actions = modify(item, None, QueryType::Tag, &registry)?;
    assert_eq!(actions.len(), 1, "None query on Volatile should generate registration Add only");
    write_and_refresh(&store, &registry, actions)?;

    // Stored になっている (rank は付かない)
    let results2 = ttfm::search::search(&store, &registry, &cache, "tag:\"project:A\"", SearchOptions::default())?;
    assert!(results2.results[0].id.is_stored());
    assert_eq!(results2.results[0].rank, 0);
    Ok(())
}

// search 層: exact tag:"X" は「タグ定義アイテム」を返す。
// 未登録なら Volatile、item_references に登録済みなら Stored。
#[test]
fn tag_exact_returns_definition_item() -> anyhow::Result<()> {
    let (store, registry, cache, _dir) = setup();

    // foo.txt に project:A を付与（user_tag は出来るが tag 定義アイテムは未登録）
    edit(&store, &registry, &cache, "filename:foo.txt", "project:A", QueryType::Tag, None, WriteOptions::default())?;

    // 定義未登録 → タグ定義は Volatile 1件（タグ付きファイル foo.txt ではない）
    let r = ttfm::search::search(&store, &registry, &cache, "tag:\"project:A\"", SearchOptions::default())?;
    assert_eq!(r.results.len(), 1);
    assert!(r.results[0].id.is_volatile(), "unregistered tag def must be Volatile");
    assert_eq!(
        r.results[0].representative.first().map(|l| l.tag_type()),
        Some(TagType::Base(SType::TypedTag))
    );

    // item_references に tag 定義を登録 → Stored
    ttfm::tagging::add_item(&store, &registry, "tag", "project:A")?;
    let r2 = ttfm::search::search(&store, &registry, &cache, "tag:\"project:A\"", SearchOptions::default())?;
    assert_eq!(r2.results.len(), 1);
    assert!(r2.results[0].id.is_stored(), "registered tag def must be Stored");

    Ok(())
}

// 計算値クエリを edit() で流すと、結果 note に query: が注入され DB に保存される。
// （value タグを持つ集計／計算値のみが対象。由来保持 EDIT.md §5.7(B)）
#[test]
fn edit_calc_result_persists_query_tag() -> anyhow::Result<()> {
    let (store, registry, cache, _dir) = setup();

    // count(extension:txt) は単一スカラ（value タグ持ち Volatile）を返す
    let search_query = "count(extension:txt)";
    edit(&store, &registry, &cache, search_query, "rank:5", QueryType::Tag, None, WriteOptions::default())?;

    // user_tags に type='query', label_str=元クエリ の行が保存されている
    let path = store.path_for_target(TargetTable::UserTags);
    let sql = format!(
        "SELECT COUNT(*) FROM read_parquet('{}') WHERE type = 'query' AND label_str = '{}'",
        path.to_string_lossy(),
        search_query
    );
    let count: i64 = store.conn.query_row(&sql, [], |r| r.get(0))?;
    assert_eq!(count, 1, "calc result note must carry the source query: tag");
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
