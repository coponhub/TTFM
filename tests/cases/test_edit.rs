use tempfile::TempDir;
use ttfm::{
    db::{Store, TargetTable},
    edit::{
        edit, modify::modify, write::write_and_refresh, QueryType, WriteOptions,
    },
    indexing::Indexer,
    tag::TagRegistry,
    types::{SType, TagType},
    SearchOptions,
};

fn setup() -> (Store, TagRegistry, TempDir) {
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
    (store, registry, dir)
}

// edit → tag: filename:foo.txt に project:A を付与し、search で確認できる
#[test]
fn edit_tag_adds_user_tag() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let resp = edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:A"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;
    assert_eq!(resp.updated, 1);

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "project:A",
        SearchOptions::default(),
    )?;
    assert_eq!(results.results.len(), 1);
    Ok(())
}

// Volatile な tag 定義に EditQuery → DB に登録されて rank が付く
#[test]
fn modify_volatile_tag_def_gets_registered_with_rank() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // foo.txt に project:A を付与して tag 定義を作る
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:A"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    // tag:"project:A" → Volatile な tag 定義アイテムが返る
    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    assert_eq!(results.results.len(), 1);
    let item = &results.results[0];
    assert!(
        !item.id.is_stored(),
        "tag def should not be Stored before registration"
    );
    assert_eq!(
        item.representative.first().map(|l| l.tag_type()),
        Some(TagType::Base(SType::TypedTag))
    );

    // modify → write で登録 + rank 付与
    let actions = modify(item, Some("rank:100"), QueryType::Tag, &registry)?;
    write_and_refresh(&store, &registry, actions)?;

    // Stored になり rank が付いている
    let results2 = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    assert_eq!(results2.results.len(), 1);
    assert!(
        results2.results[0].id.is_stored(),
        "tag def should be Stored after registration"
    );
    assert_eq!(results2.results[0].rank, 100);
    Ok(())
}

// Volatile な Projection (parentdir:) に EditQuery → note として登録される
#[test]
fn modify_volatile_projection_gets_registered_as_note() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // parentdir: → Volatile な Projection アイテムが返る
    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "parentdir:",
        SearchOptions::default(),
    )?;
    assert!(!results.results.is_empty());
    let item = &results.results[0];
    assert!(
        item.id.is_volatile(),
        "parentdir projection should be Volatile"
    );

    // modify → write で note として登録 + project:archived 付与
    let actions =
        modify(item, Some("project:archived"), QueryType::Tag, &registry)?;
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
fn modify_volatile_tag_def_no_edit_query_registers_only() -> anyhow::Result<()>
{
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:A"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    let item = &results.results[0];
    assert!(!item.id.is_stored());

    // None → 登録 Add のみ生成
    let actions = modify(item, None, QueryType::Tag, &registry)?;
    assert_eq!(
        actions.len(),
        1,
        "None query on Volatile should generate registration Add only"
    );
    write_and_refresh(&store, &registry, actions)?;

    // Stored になっている (rank は付かない)
    let results2 = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    assert!(results2.results[0].id.is_stored());
    assert_eq!(results2.results[0].rank, 0);
    Ok(())
}

// search 層: exact tag:"X" は「タグ定義アイテム」を返す。
// 未登録なら Volatile、item_references に登録済みなら Stored。
#[test]
fn tag_exact_returns_definition_item() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // foo.txt に project:A を付与（user_tag は出来るが tag 定義アイテムは未登録）
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:A"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    // 定義未登録 → タグ定義は Volatile 1件（タグ付きファイル foo.txt ではない）
    let r = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    assert_eq!(r.results.len(), 1);
    assert!(
        !r.results[0].id.is_stored(),
        "unregistered tag def must not be Stored"
    );
    assert_eq!(
        r.results[0].representative.first().map(|l| l.tag_type()),
        Some(TagType::Base(SType::TypedTag))
    );

    // item_references に tag 定義を登録 → Stored
    ttfm::tagging::add_item(&store, &registry, "tag", "project:A")?;
    let r2 = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    assert_eq!(r2.results.len(), 1);
    assert!(
        r2.results[0].id.is_stored(),
        "registered tag def must be Stored"
    );

    Ok(())
}

// 計算値クエリを edit() で流すと、結果 note に query: が注入され DB に保存される。
// （value タグを持つ集計／計算値のみが対象。由来保持 EDIT.md §5.7(B)）
#[test]
fn edit_calc_result_persists_query_tag() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // count(extension:txt) は単一スカラ（value タグ持ち Volatile）を返す
    let search_query = "count(extension:txt)";
    edit(
        &store,
        &registry,
        search_query,
        Some("rank:5"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    // user_tags に type='query', label_str=元クエリ の行が保存されている
    let path = store.path_for_target(TargetTable::UserTags);
    let sql = format!(
        "SELECT COUNT(*) FROM read_parquet('{}') WHERE type = 'query' AND label_str = '{}'",
        path.to_string_lossy(),
        search_query
    );
    let count: i64 = store.conn.query_row(&sql, [], |r| r.get(0))?;
    assert_eq!(
        count, 1,
        "calc result note must carry the source query: tag"
    );
    Ok(())
}

// §5.7: SearchQuery のみ（EditQuery なし）で edit() を呼ぶと定義が登録される。
#[test]
fn edit_no_edit_query_registers_definition() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // foo.txt に project:A を付与（tag 定義は未登録 Volatile のまま）
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:A"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;
    let r = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    assert!(
        !r.results[0].id.is_stored(),
        "tag def is not Stored before §5.7 registration"
    );

    // EditQuery なし（None）で登録のみ
    edit(
        &store,
        &registry,
        "tag:\"project:A\"",
        None,
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;
    let r2 = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    assert!(
        r2.results[0].id.is_stored(),
        "tag def must be Stored after §5.7 registration"
    );
    Ok(())
}

// 登録済み User タグ定義は projection の item 表示で 'unknown' でなく登録名を引く。
// name は user_tags(type:name) に入り item_references.name は NULL（system専用）のため、
// representative の name 解決が非NULL（lens 一般 read）で user_tags 名を引く必要がある回帰テスト。
#[test]
fn registered_tag_def_name_shown_in_projection() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // foo.txt に project:A を付与 → tag 定義(Volatile) と user_tag が出来る
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:A"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    // tag 定義を rank 付きで登録（§5.7 + rank）。name は user_tags に注入される。
    let r = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:\"project:A\"",
        SearchOptions::default(),
    )?;
    let item = &r.results[0];
    assert!(
        !item.id.is_stored(),
        "tag def must not be Stored before registration"
    );
    let actions = modify(item, Some("rank:77"), QueryType::Tag, &registry)?;
    write_and_refresh(&store, &registry, actions)?;

    // rank: projection の item 表示が 'unknown' でなく 'project:A' を含む
    let proj = ttfm::search::search_nowarn(
        &store,
        &registry,
        "rank:",
        SearchOptions::default(),
    )?;
    let item_names: Vec<String> = proj
        .results
        .iter()
        .flat_map(|g| g.tags.entries.iter())
        .filter(|e| e.typed_tag.tag_type() == TagType::from("item"))
        .map(|e| e.typed_tag.value().as_display_name())
        .collect();
    assert!(
        item_names.iter().any(|v| v.starts_with("project:A#")),
        "registered tag def name must show in projection, got: {:?}",
        item_names
    );
    assert!(
        !item_names.iter().any(|v| v.starts_with("unknown#")),
        "no projection item should display as 'unknown', got: {:?}",
        item_names
    );
    Ok(())
}

// TTFM 上でファイルに name: を付与（rename 相当）すると filename(locations) と
// user名(user_tags) が type='name' で併存する。§4.1 で user 名を優先表示する回帰テスト。
#[test]
fn renamed_file_shows_user_name() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // foo.txt（name=filename）に user 名を付与（rename 相当）
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("name:renamed_foo"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    // list() の入力順を反転させてバグを顕在化させる（user名を先頭・filename(system)を末尾へ）。
    // §4.1 が oneview で解決されていれば name は1行に畳まれ、この並べ替えに依らず user 名になる。
    let def: String = store.conn.query_row(
        "SELECT sql FROM duckdb_views() WHERE view_name = 'oneview'",
        [],
        |r| r.get(0),
    )?;
    let select = def
        .split_once(" AS ")
        .map(|(_, s)| s.trim().trim_end_matches(';'))
        .expect("oneview view def");
    store.conn.execute(
        &format!("CREATE OR REPLACE VIEW oneview AS SELECT * FROM ({select}) _o ORDER BY (origin = 'user') DESC"),
        [],
    )?;

    // ファイルを検索 → representative の name は user 名（filename ではない）
    let r = ttfm::search::search_nowarn(
        &store,
        &registry,
        "filename:foo.txt",
        SearchOptions::default(),
    )?;
    assert_eq!(r.results.len(), 1);
    let name = r.results[0].representative.iter().find_map(|l| {
        if l.tag_type() == ttfm::types::TagType::Base(ttfm::types::SType::Name) {
            let s = l.as_str();
            Some(s.clone())
        } else {
            None
        }
    });
    assert_eq!(
        name.as_deref(),
        Some("renamed_foo"),
        "renamed file must display the user name, not the filename; got: {:?}",
        r.results[0].representative
    );
    Ok(())
}

// edit → untag: 付与済みの project:A を削除し、search で 0 件になる
#[test]
fn edit_untag_removes_user_tag() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:A"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let resp = edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:A"),
        QueryType::Untag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;
    assert_eq!(resp.deleted, 1);

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "project:A",
        SearchOptions::default(),
    )?;
    assert_eq!(results.results.len(), 0);
    Ok(())
}

// edit → tag: 150 件（>旧仮実装の 100 件上限）を一括タグ付けし、全件に付与されることを確認。
// 旧 unwrap_or(100) では先頭 100 件で切れて RED になる回帰テスト。
#[test]
fn edit_tag_applies_to_all_over_100_files() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..150 {
        std::fs::write(root.join(format!("f{i:03}.txt")), "content").unwrap();
    }

    let db_dir = base.join("db");
    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

    let resp = edit(
        &store,
        &registry,
        "extension:txt",
        Some("project:bulk"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;
    assert_eq!(resp.updated, 150, "all 150 files should be tagged");

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "project:bulk",
        SearchOptions::default(),
    )?;
    assert_eq!(
        results.results.len(),
        150,
        "search should return all 150 tagged files"
    );
    Ok(())
}
