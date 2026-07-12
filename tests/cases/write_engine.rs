use tempfile::tempdir;
use ttfm::db::{Store, TargetTable};
use ttfm::edit::write::{write, DeleteTarget, TagOp, WriteAction};
use ttfm::indexing::Indexer;
use ttfm::tag::TagRegistry;
use ttfm::types::{Bitical, ItemId, Label, SType, TagType};

fn setup() -> (Store, TagRegistry, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("db");
    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    (store, registry, dir)
}

fn read_user_tags(store: &Store) -> Vec<(i64, String, String)> {
    let path = store.path_for_target(TargetTable::UserTags);
    if !path.exists() {
        return vec![];
    }
    let sql = format!(
        "SELECT item_id, type, COALESCE(label_str, CAST(label_bool AS VARCHAR), CAST(label_int AS VARCHAR), CAST(label_double AS VARCHAR), '') \
         FROM read_parquet('{}') ORDER BY item_id, type, label_str",
        path.to_string_lossy()
    );
    store
        .conn
        .prepare(&sql)
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn read_item_ids_with_kind(store: &Store) -> Vec<(i64, String, String)> {
    let path = store.path_for_target(TargetTable::ItemReferences);
    let sql = format!(
        "SELECT item_id, item_kind, content FROM read_parquet('{}') ORDER BY item_id",
        path.to_string_lossy()
    );
    store
        .conn
        .prepare(&sql)
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

// ──────────────────────────────────────────────
// Slice 2: Volatile アイテムの新規作成
// ──────────────────────────────────────────────

#[test]
fn write_add_volatile_creates_item_ref_and_user_tag() {
    let (store, registry, _dir) = setup();

    let resp = write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Volatile(0),
            tags: vec![
                TagOp::Append(Label::ItemKind("tag".to_string())),
                TagOp::Append(Label::Content("project:A".to_string())),
                TagOp::Append(Label::Other(
                    TagType::from("project"),
                    Bitical::String("A".to_string()),
                )),
            ],
        }],
    )
    .unwrap();

    assert_eq!(resp.new_item_ids.len(), 1);
    let new_id = resp.new_item_ids[0];
    // User 空間 [0, 2^58) に採番されていること
    assert!(
        new_id >= 0 && new_id < (1i64 << 58),
        "id {new_id} not in User space"
    );

    // item_references に新規行が存在すること
    let items = read_item_ids_with_kind(&store);
    assert!(
        items.iter().any(|(id, kind, content)| *id == new_id
            && kind == "tag"
            && content == "project:A"),
        "item_references missing row for id {new_id}: {items:?}"
    );

    // user_tags に project:A が存在すること
    let tags = read_user_tags(&store);
    assert!(
        tags.iter().any(|(id, ty, val)| *id == new_id
            && ty == "project"
            && val == "A"),
        "user_tags missing project:A for id {new_id}: {tags:?}"
    );
    assert_eq!(resp.updated, 4); // ItemKind + Content + project:A + new item_id
}

// ──────────────────────────────────────────────
// Slice 3: タグの削除
// ──────────────────────────────────────────────

#[test]
fn write_delete_by_boolean_label_removes_only_matching_value() {
    let (store, registry, _dir) = setup();
    let id = ttfm::tagging::add_item(&store, &registry, "note", "n").unwrap();

    // bool:true と bool:false を両方追加
    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(id),
            tags: vec![
                TagOp::Append(Label::Other(
                    TagType::from("flag"),
                    Bitical::Boolean(true),
                )),
                TagOp::Append(Label::Other(
                    TagType::from("flag"),
                    Bitical::Boolean(false),
                )),
            ],
        }],
    )
    .unwrap();
    assert_eq!(read_user_tags(&store).len(), 2);

    // true だけ削除
    write(
        &store,
        &registry,
        vec![WriteAction::Delete {
            item: ItemId::Stored(id),
            tags: vec![DeleteTarget::Tag(Label::Other(
                TagType::from("flag"),
                Bitical::Boolean(true),
            ))],
        }],
    )
    .unwrap();

    let tags = read_user_tags(&store);
    assert_eq!(
        tags.len(),
        1,
        "only true should be deleted, false should remain: {tags:?}"
    );
}

#[test]
fn write_delete_by_type_removes_user_tags() {
    let (store, registry, _dir) = setup();
    let id = ttfm::tagging::add_item(&store, &registry, "note", "n").unwrap();

    // まずタグを2件追加
    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(id),
            tags: vec![
                TagOp::Append(Label::Other(
                    TagType::from("project"),
                    Bitical::String("A".to_string()),
                )),
                TagOp::Append(Label::Other(
                    TagType::from("project"),
                    Bitical::String("B".to_string()),
                )),
            ],
        }],
    )
    .unwrap();
    assert_eq!(read_user_tags(&store).len(), 2);

    // type 指定で全削除
    let resp = write(
        &store,
        &registry,
        vec![WriteAction::Delete {
            item: ItemId::Stored(id),
            tags: vec![DeleteTarget::Type(TagType::from("project"))],
        }],
    )
    .unwrap();

    assert_eq!(resp.deleted, 1);
    assert!(
        read_user_tags(&store).is_empty(),
        "user_tags should be empty after delete"
    );
}

#[test]
fn write_delete_by_label_removes_specific_tag() {
    let (store, registry, _dir) = setup();
    let id = ttfm::tagging::add_item(&store, &registry, "note", "n").unwrap();

    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(id),
            tags: vec![
                TagOp::Append(Label::Other(
                    TagType::from("project"),
                    Bitical::String("A".to_string()),
                )),
                TagOp::Append(Label::Other(
                    TagType::from("project"),
                    Bitical::String("B".to_string()),
                )),
            ],
        }],
    )
    .unwrap();

    // label 指定で A のみ削除
    write(
        &store,
        &registry,
        vec![WriteAction::Delete {
            item: ItemId::Stored(id),
            tags: vec![DeleteTarget::Tag(Label::Other(
                TagType::from("project"),
                Bitical::String("A".to_string()),
            ))],
        }],
    )
    .unwrap();

    let tags = read_user_tags(&store);
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].2, "B");
}

fn read_file_ids(store: &Store) -> Vec<i64> {
    let path = store.path_for_target(TargetTable::FileReferences);
    if !path.exists() {
        return vec![];
    }
    let sql = format!(
        "SELECT item_id FROM read_parquet('{}') ORDER BY item_id",
        path.to_string_lossy()
    );
    store
        .conn
        .prepare(&sql)
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn setup_with_indexed_file() -> (Store, TagRegistry, i64, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let db_dir = base.join("db");
    let root = base.join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "hello").unwrap();

    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

    let file_id = read_file_ids(&store)[0];
    (store, registry, file_id, dir)
}

// ──────────────────────────────────────────────
// Slice 4: カスケード削除
// ──────────────────────────────────────────────

#[test]
fn write_cascade_delete_reports_deleted_count() {
    let (store, registry, _dir) = setup();
    let id = ttfm::tagging::add_item(&store, &registry, "note", "n").unwrap();

    let resp = write(
        &store,
        &registry,
        vec![WriteAction::Delete {
            item: ItemId::Stored(id),
            tags: vec![DeleteTarget::Type(TagType::Base(SType::ItemId))],
        }],
    )
    .unwrap();

    assert!(
        resp.deleted > 0,
        "cascade delete should report deleted > 0, got {}",
        resp.deleted
    );
}

#[test]
fn write_cascade_delete_removes_item_from_item_references() {
    let (store, registry, _dir) = setup();
    let id =
        ttfm::tagging::add_item(&store, &registry, "note", "my note").unwrap();
    assert!(read_item_ids_with_kind(&store)
        .iter()
        .any(|(i, _, _)| *i == id));

    write(
        &store,
        &registry,
        vec![WriteAction::Delete {
            item: ItemId::Stored(id),
            tags: vec![DeleteTarget::Type(TagType::Base(SType::ItemId))],
        }],
    )
    .unwrap();

    assert!(
        !read_item_ids_with_kind(&store)
            .iter()
            .any(|(i, _, _)| *i == id),
        "item {id} should be removed from item_references after cascade delete"
    );
}

#[test]
fn write_cascade_delete_also_removes_user_tags() {
    let (store, registry, _dir) = setup();
    let id =
        ttfm::tagging::add_item(&store, &registry, "note", "my note").unwrap();

    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(id),
            tags: vec![TagOp::Append(Label::Other(
                TagType::from("project"),
                Bitical::String("A".to_string()),
            ))],
        }],
    )
    .unwrap();
    assert_eq!(read_user_tags(&store).len(), 1);

    write(
        &store,
        &registry,
        vec![WriteAction::Delete {
            item: ItemId::Stored(id),
            tags: vec![DeleteTarget::Type(TagType::Base(SType::ItemId))],
        }],
    )
    .unwrap();

    assert!(
        read_user_tags(&store).is_empty(),
        "user_tags should be empty after cascade delete"
    );
}

#[test]
fn write_cascade_delete_file_origin_removes_from_file_references() {
    let (store, registry, file_id, _dir) = setup_with_indexed_file();
    assert!(read_file_ids(&store).contains(&file_id));

    write(
        &store,
        &registry,
        vec![WriteAction::Delete {
            item: ItemId::Stored(file_id),
            tags: vec![DeleteTarget::Type(TagType::Base(SType::ItemId))],
        }],
    )
    .unwrap();

    assert!(
        !read_file_ids(&store).contains(&file_id),
        "file {file_id} should be removed from file_references after cascade delete"
    );
}

// ──────────────────────────────────────────────
// Slice 5: rank 更新
// ──────────────────────────────────────────────

fn read_rank(store: &Store, item_id: i64) -> Option<i64> {
    let path = store.path_for_target(TargetTable::ItemReferences);
    if !path.exists() {
        return None;
    }
    let sql = format!(
        "SELECT rank FROM read_parquet('{}') WHERE item_id = {}",
        path.to_string_lossy(),
        item_id
    );
    store.conn.query_row(&sql, [], |r| r.get(0)).ok()
}

#[test]
fn write_rank_update_changes_rank_in_item_references() {
    let (store, registry, _dir) = setup();
    let id =
        ttfm::tagging::add_item(&store, &registry, "note", "my note").unwrap();

    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(id),
            tags: vec![TagOp::Append(Label::Rank(5))],
        }],
    )
    .unwrap();

    assert_eq!(read_rank(&store, id), Some(5));
}

// ──────────────────────────────────────────────
// Slice 1: Stored アイテムへのタグ追加
// ──────────────────────────────────────────────

#[test]
fn write_add_stored_appends_user_tag() {
    let (store, registry, _dir) = setup();
    // add_item で User 空間の既存アイテムを作成
    let existing_id = ttfm::tagging::add_item(
        &store,
        &TagRegistry::with_standard(),
        "note",
        "my note",
    )
    .unwrap();

    let resp = write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(existing_id),
            tags: vec![TagOp::Append(Label::Other(
                TagType::from("project"),
                Bitical::String("A".to_string()),
            ))],
        }],
    )
    .unwrap();

    assert_eq!(resp.updated, 1);
    assert_eq!(resp.deleted, 0);
    assert!(resp.new_item_ids.is_empty());

    let tags = read_user_tags(&store);
    assert!(
        tags.iter().any(|(id, ty, val)| *id == existing_id && ty == "project" && val == "A"),
        "user_tags should contain project:A for item {existing_id}, got: {tags:?}"
    );
}

// ──────────────────────────────────────────────
// Slice 7: Delete + Add 同一 (item, type) を1バッチ
// ──────────────────────────────────────────────

#[test]
fn write_delete_and_add_same_type_in_one_batch_replaces_value() {
    let (store, registry, _dir) = setup();
    let id = ttfm::tagging::add_item(&store, &registry, "note", "n").unwrap();

    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(id),
            tags: vec![TagOp::Append(Label::Other(
                TagType::from("project"),
                Bitical::String("A".to_string()),
            ))],
        }],
    )
    .unwrap();

    write(
        &store,
        &registry,
        vec![
            WriteAction::Delete {
                item: ItemId::Stored(id),
                tags: vec![DeleteTarget::Type(TagType::from("project"))],
            },
            WriteAction::Add {
                item: ItemId::Stored(id),
                tags: vec![TagOp::Append(Label::Other(
                    TagType::from("project"),
                    Bitical::String("B".to_string()),
                ))],
            },
        ],
    )
    .unwrap();

    let tags = read_user_tags(&store);
    assert_eq!(
        tags.len(),
        1,
        "should have exactly one project tag: {tags:?}"
    );
    assert_eq!(tags[0].2, "B", "project tag should be B: {tags:?}");
}

#[test]
fn write_replace_delete_not_counted_in_deleted() {
    let (store, registry, _dir) = setup();
    let id = ttfm::tagging::add_item(&store, &registry, "note", "n").unwrap();

    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(id),
            tags: vec![TagOp::Append(Label::Other(
                TagType::from("project"),
                Bitical::String("A".to_string()),
            ))],
        }],
    )
    .unwrap();

    let resp = write(
        &store,
        &registry,
        vec![
            WriteAction::Delete {
                item: ItemId::Stored(id),
                tags: vec![DeleteTarget::Type(TagType::from("project"))],
            },
            WriteAction::Add {
                item: ItemId::Stored(id),
                tags: vec![TagOp::Replace(Label::Other(
                    TagType::from("project"),
                    Bitical::String("B".to_string()),
                ))],
            },
        ],
    )
    .unwrap();

    assert_eq!(
        resp.deleted, 0,
        "Replace companion delete must not count as deleted"
    );
    assert_eq!(resp.updated, 1);
}

// ──────────────────────────────────────────────
// Slice 8: 複数アイテムを1バッチ
// ──────────────────────────────────────────────

#[test]
fn write_multiple_items_in_one_batch() {
    let (store, registry, _dir) = setup();
    let id1 = ttfm::tagging::add_item(&store, &registry, "note", "n1").unwrap();
    let id2 = ttfm::tagging::add_item(&store, &registry, "note", "n2").unwrap();

    write(
        &store,
        &registry,
        vec![
            WriteAction::Add {
                item: ItemId::Stored(id1),
                tags: vec![TagOp::Append(Label::Other(
                    TagType::from("project"),
                    Bitical::String("A".to_string()),
                ))],
            },
            WriteAction::Add {
                item: ItemId::Stored(id2),
                tags: vec![TagOp::Append(Label::Other(
                    TagType::from("project"),
                    Bitical::String("A".to_string()),
                ))],
            },
        ],
    )
    .unwrap();

    let tags = read_user_tags(&store);
    assert_eq!(tags.len(), 2, "both items should have project:A: {tags:?}");
    assert!(tags.iter().any(|(id, _, _)| *id == id1));
    assert!(tags.iter().any(|(id, _, _)| *id == id2));
}

// ──────────────────────────────────────────────
// Slice 9: file_references の rank 更新
// ──────────────────────────────────────────────

fn read_file_rank(store: &Store, item_id: i64) -> Option<i64> {
    let path = store.path_for_target(TargetTable::FileReferences);
    if !path.exists() {
        return None;
    }
    let sql = format!(
        "SELECT rank FROM read_parquet('{}') WHERE item_id = {}",
        path.to_string_lossy(),
        item_id
    );
    store.conn.query_row(&sql, [], |r| r.get(0)).ok()
}

#[test]
fn write_rank_update_changes_rank_in_file_references() {
    let (store, registry, file_id, _dir) = setup_with_indexed_file();
    let initial = read_file_rank(&store, file_id).unwrap_or(0);

    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(file_id),
            tags: vec![TagOp::Append(Label::Rank(initial + 7))],
        }],
    )
    .unwrap();

    assert_eq!(read_file_rank(&store, file_id), Some(initial + 7));
}

// ──────────────────────────────────────────────
// Slice 10: Double 値の精密削除
// ──────────────────────────────────────────────

#[test]
fn write_delete_by_double_label_removes_only_matching_value() {
    let (store, registry, _dir) = setup();
    let id = ttfm::tagging::add_item(&store, &registry, "note", "n").unwrap();

    let v1 = Bitical::Double(1.0f64);
    let v2 = Bitical::Double(2.0f64);

    write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Stored(id),
            tags: vec![
                TagOp::Append(Label::Other(TagType::from("score"), v1.clone())),
                TagOp::Append(Label::Other(TagType::from("score"), v2)),
            ],
        }],
    )
    .unwrap();
    assert_eq!(read_user_tags(&store).len(), 2);

    write(
        &store,
        &registry,
        vec![WriteAction::Delete {
            item: ItemId::Stored(id),
            tags: vec![DeleteTarget::Tag(Label::Other(
                TagType::from("score"),
                v1,
            ))],
        }],
    )
    .unwrap();

    let tags = read_user_tags(&store);
    assert_eq!(tags.len(), 1, "only score:1.0 should be deleted: {tags:?}");
}
