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

use crate::cases::has_item_tags;
use file_id::get_file_id;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::{tempdir, TempDir};
use ttfm::{
    db::{Store, TargetTable},
    edit::{edit, QueryType, WriteOptions},
    indexing::Indexer,
    response::Item,
    tag::TagRegistry,
    types::ItemId,
    SearchOptions,
};

fn setup(files: &[&str]) -> (Store, TagRegistry, TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    for name in files {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, *name).unwrap();
    }
    let registry = TagRegistry::with_standard();
    let store = Store::open(base.join("db")).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    (store, registry, dir, root)
}

fn index(store: &Store, registry: &TagRegistry, roots: &[&Path]) {
    Indexer::new(store, registry)
        .run(roots, None::<&fn(usize)>, false)
        .unwrap();
}

fn find(store: &Store, registry: &TagRegistry, q: &str) -> Vec<Item> {
    ttfm::search::search_nowarn(store, registry, q, SearchOptions::default())
        .unwrap()
        .results
}

fn tag(store: &Store, registry: &TagRegistry, q: &str, e: &str) {
    edit(
        store,
        registry,
        q,
        Some(e),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )
    .unwrap();
}

#[test]
fn test_incremental_indexing_full_flow() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let path_a = root.join("a.txt");
    let all_files = "item_kind:file";

    // 1. 初回スキャン (root + a.txt = 2)
    index(&store, &registry, &[&root]);
    assert_eq!(find(&store, &registry, all_files).len(), 2);
    assert_eq!(find(&store, &registry, "filename:a.txt").len(), 1);

    // 2. 変更なし: そのまま再スキャン (2)
    index(&store, &registry, &[&root]);
    assert_eq!(find(&store, &registry, all_files).len(), 2);

    // 3. 追加: b.rs を作成 (root + a.txt + b.rs = 3)
    let path_b = root.join("b.rs");
    std::fs::write(&path_b, "fn main() {}").unwrap();
    index(&store, &registry, &[&root]);
    assert_eq!(find(&store, &registry, all_files).len(), 3);

    // 4. 更新: a.txt の内容を変更 (サイズ変更)
    // 実体(ID)が変わらないことを確認
    let old_id = find(&store, &registry, "filename:a.txt")[0].id.clone();
    std::fs::write(&path_a, "updated content with more bytes").unwrap();
    index(&store, &registry, &[&root]);

    let res_edit = find(&store, &registry, "filename:a.txt");
    let files_edit: Vec<_> = res_edit
        .iter()
        .filter(|r| r.item_kind == ttfm::ItemKind::File)
        .collect();
    assert_eq!(files_edit.len(), 1, "Should find exactly one a.txt");
    assert_eq!(
        files_edit[0].id, old_id,
        "Item ID must be reused after content edit"
    );

    // 5. 削除: b.rs を削除 (root + a.txt = 2)
    std::fs::remove_file(&path_b).unwrap();
    index(&store, &registry, &[&root]);
    assert_eq!(find(&store, &registry, all_files).len(), 2);

    let files_b_del: Vec<_> = find(&store, &registry, "filename:b.rs")
        .into_iter()
        .filter(|r| r.item_kind == ttfm::ItemKind::File)
        .collect();
    assert_eq!(
        files_b_del.len(),
        0,
        "b.rs must be removed from search results"
    );

    // 6. 別名追加 (ハードリンク): a.txt の別名として c.txt を作成
    let path_c = root.join("c.txt");
    std::fs::hard_link(&path_a, &path_c).unwrap();
    index(&store, &registry, &[&root]);

    // Inode 情報を直接取得して検索 (Uuid 形式のクエリを作成)
    let fid = get_file_id(&path_a).unwrap();
    let (upper, lower) = match fid {
        file_id::FileId::Inode {
            device_id,
            inode_number,
        } => (device_id, inode_number),
        file_id::FileId::LowRes {
            volume_serial_number,
            file_index,
        } => (volume_serial_number as u64, file_index),
        file_id::FileId::HighRes {
            volume_serial_number,
            file_id,
        } => (
            (file_id >> 64) as u64 ^ volume_serial_number,
            file_id as u64,
        ),
    };
    let uuid_str = uuid::Uuid::from_u64_pair(upper, lower).to_string();
    let query = format!("file_id:\"{}\"", uuid_str);

    let files_inode: Vec<_> = find(&store, &registry, &query)
        .into_iter()
        .filter(|r| r.item_kind == ttfm::ItemKind::File)
        .collect();

    // 検証：1つの実体に a.txt と c.txt の 2つの場所が紐づき、
    // inode 由来の属性（size/mtime）は実体単位で1つに集約されること
    assert_eq!(files_inode.len(), 1);
    let entries = &files_inode[0].tags.entries;
    let values = |t: &str| -> Vec<String> {
        entries
            .iter()
            .filter(|e| e.typed_tag.tag_type().as_str() == t)
            .map(|e| e.typed_tag.value().as_display_name())
            .collect()
    };

    let names = values("filename");
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"c.txt".to_string()));
    assert_eq!(values("size").len(), 1);
    assert_eq!(values("mtime").len(), 1);
}

#[test]
fn test_system_items_registration() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("hello.txt"), "hello").unwrap();

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let (store, registry) = (db_dir_store, db_dir_registry);
    index(&store, &registry, &[root]);

    // 1. item_entities に extension:txt 関連のItemがあるか確認
    // 変更後: 自動生成されなくなったため、物理的なアイテムは存在しないはず
    let results_physical =
        find(&store, &registry, "item_kind:tag & name:extension:txt");
    assert!(
        results_physical.is_empty(),
        "Physical tag item should NOT be created automatically"
    );

    // 2. しかし、プロジェクション（oneview）経由では検索できること
    // 「typedtag:」で検索（プロジェクションクエリ）を行い、動的にタグが生成・投影されることを確認
    let results_projection = find(&store, &registry, "tag:");

    // プロジェクション配下に typedtag が含まれているか確認
    assert!(has_item_tags(&results_projection));
    assert!(!results_projection.is_empty(), "Should find items");

    // 投影された値の中に extension:txt が含まれているか（動的生成の確認）
    // 物理的な Item はなくても、oneview 上で結合されて値として取得できるはず
    // 転置: results には label items が格納されるため、name が "extension:txt" であることを確認
    let has_target_val = results_projection.iter().any(|r| {
        r.item_kind == ttfm::ItemKind::Volatile
            && r.raw_repr() == "extension:txt"
    });
    assert!(
        has_target_val,
        "Should contain label item with name='extension:txt'"
    );

    // 3. origin のプロジェクションも確認
    let results_origin = find(&store, &registry, "origin:");
    assert!(has_item_tags(&results_origin));
    assert!(!results_origin.is_empty());

    // 転置: results には label items が格納され、name が "file" であることを確認
    // (hello.txt はスキャン抽出タグのみを持つ File 由来アイテムのため、origin は "file")
    let file_label = results_origin
        .iter()
        .find(|r| r.raw_repr() == "file")
        .expect("file label not found for origin check");
    assert_eq!(
        file_label.item_kind,
        ttfm::ItemKind::Volatile,
        "Should be a label item"
    );
    // このラベルの tags に "item:hello.txt#..." が含まれているはず
    let has_hello_txt = file_label
        .tags
        .entries
        .iter()
        .any(|entry| entry.typed_tag.as_str().contains("hello.txt"));
    assert!(
        has_hello_txt,
        "file origin label should contain reference to hello.txt"
    );
}

#[test]
fn test_typedtag_listing_via_type_query() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("test.txt"), "hello").unwrap();

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let (store, registry) = (db_dir_store, db_dir_registry);
    index(&store, &registry, &[root]);

    // 1. type:extension で検索 -> extension:txt アイテムが見つかるはず
    let tt_items: Vec<_> = find(&store, &registry, "type:extension")
        .into_iter()
        .filter(|r| {
            r.item_kind == ttfm::ItemKind::Tag && r.raw_repr() == "extension:txt"
        })
        .collect();
    assert_eq!(
        tt_items.len(),
        0,
        "Should NOT find the tag item because it doesn't have the tag (metadata definition only)"
    );

    // 2. extension:txt で検索 -> ファイルだけが見つかるはず（ノイズがないこと）
    // オリジナル通りのフィルタロジックに戻す
    let results = find(&store, &registry, "extension:txt");
    let files: Vec<_> = results
        .iter()
        .filter(|r| r.item_kind == ttfm::ItemKind::File)
        .collect();
    let tags: Vec<_> = results
        .iter()
        .filter(|r| r.item_kind == ttfm::ItemKind::Tag)
        .collect();

    assert_eq!(files.len(), 1, "Should find the file");
    assert_eq!(tags.len(), 0, "Should NOT find the tag item itself as noise");
}

#[test]
fn test_no_empty_extension_system_item() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("no_extension"), "test").unwrap();

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let (store, registry) = (db_dir_store, db_dir_registry);
    index(&store, &registry, &[root]);

    let results = find(&store, &registry, "item_kind:tag & name:\"extension:\"");
    assert!(
        results.is_empty(),
        "Should NOT register 'extension:' system item"
    );
}

#[test]
fn test_definition_only_items_not_registered() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("test.txt"), "").unwrap();

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let (store, registry) = (db_dir_store, db_dir_registry);
    index(&store, &registry, &[root]);

    // 初期登録が廃止されたため、type 定義専用アイテムは自動生成されない。
    assert!(find(&store, &registry, "item_kind:type & name:name").is_empty());
    assert!(find(&store, &registry, "item_kind:type & name:item_kind").is_empty());
}

/// 区画幅 B = 2^58。System 区画は [8B, 9B)。
const SPACE_B: i64 = 1 << 58;

fn read_item_ids(store: &ttfm::db::Store, target: ttfm::db::TargetTable) -> Vec<i64> {
    let path = store.path_for_target(target);
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

/// 初期登録が廃止されたため、indexing だけでは item_references に行は生成されない。
#[test]
fn indexing_alone_creates_no_system_definitions() {
    let (store, registry, _d, root) = setup(&["a.rs"]);
    index(&store, &registry, &[&root]);

    let def_ids = read_item_ids(&store, ttfm::db::TargetTable::ItemReferences);
    assert!(
        def_ids.is_empty(),
        "plain indexing must not create physical item_references rows"
    );
}

/// インデックス後、ファイル（file_references）は File 区画 [8B, ∞) に入る。
/// ファイル id と System 定義 id は区画が異なるため衝突しない。
#[test]
fn indexed_files_live_in_file_space_without_collision() {
    let (store, registry, _d, root) = setup(&["a.rs", "b.txt"]);
    index(&store, &registry, &[&root]);

    let file_ids = read_item_ids(&store, ttfm::db::TargetTable::FileReferences);
    let def_ids = read_item_ids(&store, ttfm::db::TargetTable::ItemReferences);
    assert!(!file_ids.is_empty(), "files should exist");

    let lo = 8 * SPACE_B;
    for id in &file_ids {
        assert!(*id >= lo, "file id {id} not in File space [{lo}, ∞)");
    }

    // ファイル（File 区画 [8B, ∞)）と定義（System 区画 [-B, 0)）は区画が異なるため衝突しない。
    let mut all = file_ids.clone();
    all.extend(def_ids.iter().copied());
    let unique: std::collections::HashSet<_> = all.iter().copied().collect();
    assert_eq!(
        unique.len(),
        all.len(),
        "File and System ids must all be distinct"
    );
}

/// item_references が書き出し時点で item_id 昇順に整列していること。
/// 複数回 add_item すると採番順とファイル書き出し順が食い違いうるため、
/// ORDER BY が効いていることを保証する。
#[test]
fn item_references_sorted_by_item_id_after_add_item() {
    let (store, registry, _d, root) = setup(&["a.rs"]);
    index(&store, &registry, &[&root]);

    ttfm::tagging::add_item(&store, &registry, "type", "my_type_a").unwrap();
    ttfm::tagging::add_item(&store, &registry, "type", "my_type_b").unwrap();

    let path = store.path_for_target(ttfm::db::TargetTable::ItemReferences);
    let sql = format!(
        "SELECT item_id FROM read_parquet('{}')",
        path.to_string_lossy()
    );
    let ids: Vec<i64> = store
        .conn
        .prepare(&sql)
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(ids.len() >= 2, "should have both added items");
    assert!(
        ids.windows(2).all(|w| w[0] <= w[1]),
        "item_references not sorted by item_id (ORDER BY missing): {:?}",
        ids
    );
}

#[test]
fn narrow_index_keeps_out_of_scope_items() {
    let (store, registry, _d, root) = setup(&["x/a.txt", "y/b.txt"]);
    index(&store, &registry, &[&root]);
    index(&store, &registry, &[&root.join("x")]);
    assert_eq!(find(&store, &registry, "filename:b.txt").len(), 1);
}

#[test]
fn tagged_item_is_recorded_as_removed_and_rediscovered() {
    let (store, registry, _d, root) = setup(&["a.txt", "b.txt"]);
    index(&store, &registry, &[&root]);
    tag(&store, &registry, "filename:a.txt", "color:red");
    let id = find(&store, &registry, "filename:a.txt")[0].id.clone();

    let kept = root.parent().unwrap().join("a.txt");
    std::fs::rename(root.join("a.txt"), &kept).unwrap();
    std::fs::remove_file(root.join("b.txt")).unwrap();
    index(&store, &registry, &[&root]);

    assert_eq!(find(&store, &registry, "color:red").len(), 1);
    assert_eq!(find(&store, &registry, "removed_file:true")[0].id, id);
    assert!(find(&store, &registry, "filename:b.txt").is_empty());

    std::fs::rename(&kept, root.join("a.txt")).unwrap();
    index(&store, &registry, &[&root]);
    assert_eq!(find(&store, &registry, "filename:a.txt")[0].id, id);
    assert!(find(&store, &registry, "removed_file:true").is_empty());
}

fn removed_rows(store: &Store) -> Vec<(i64, String, i64)> {
    let p = store.path_for_target(TargetTable::RemovedFiles);
    let sql = format!(
        "SELECT item_id, path, removed_file_at FROM read_parquet('{}')",
        p.to_string_lossy()
    );
    store
        .conn
        .prepare(&sql)
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn only_a_tagged_lost_file_is_recorded_as_removed() {
    let (store, registry, _d, root) = setup(&["a.txt", "b.txt"]);
    index(&store, &registry, &[&root]);
    tag(&store, &registry, "filename:a.txt", "color:red");
    let id = find(&store, &registry, "filename:a.txt")[0].id;

    std::fs::remove_file(root.join("a.txt")).unwrap();
    std::fs::remove_file(root.join("b.txt")).unwrap();
    index(&store, &registry, &[&root]);

    let rows = removed_rows(&store);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, id.as_i64());
    assert!(rows[0].1.ends_with("a.txt"));
    assert!(rows[0].2 > 0);
}

#[test]
fn a_removed_file_is_searchable_through_its_removed_file_types() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    index(&store, &registry, &[&root]);
    tag(&store, &registry, "filename:a.txt", "color:red");
    let id = find(&store, &registry, "filename:a.txt")[0].id;

    std::fs::remove_file(root.join("a.txt")).unwrap();
    index(&store, &registry, &[&root]);

    assert_eq!(find(&store, &registry, "removed_file:true")[0].id, id);
    assert_eq!(find(&store, &registry, "removed_file_path:*a.txt")[0].id, id);
    assert_eq!(find(&store, &registry, "removed_file_is_dir:false")[0].id, id);
    assert!(find(&store, &registry, "removed_file_at:>0")[0].id == id);
    assert!(find(&store, &registry, "removed_file:false")
        .iter()
        .all(|r| r.id != id));
}

#[test]
fn a_returning_file_leaves_the_graveyard_and_keeps_its_item_id() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    index(&store, &registry, &[&root]);
    tag(&store, &registry, "filename:a.txt", "color:red");
    let id = find(&store, &registry, "filename:a.txt")[0].id;

    let kept = root.parent().unwrap().join("a.txt");
    std::fs::rename(root.join("a.txt"), &kept).unwrap();
    index(&store, &registry, &[&root]);
    assert_eq!(removed_rows(&store).len(), 1);

    std::fs::rename(&kept, root.join("a.txt")).unwrap();
    index(&store, &registry, &[&root]);

    assert!(removed_rows(&store).is_empty());
    assert_eq!(find(&store, &registry, "filename:a.txt")[0].id, id);
    assert_eq!(find(&store, &registry, "color:red").len(), 1);
}

#[test]
fn untagging_item_id_clears_the_removed_files_row() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    index(&store, &registry, &[&root]);
    tag(&store, &registry, "filename:a.txt", "color:red");
    let id = find(&store, &registry, "filename:a.txt")[0].id;

    std::fs::remove_file(root.join("a.txt")).unwrap();
    index(&store, &registry, &[&root]);
    assert_eq!(removed_rows(&store).len(), 1);

    edit(
        &store,
        &registry,
        &format!("item_id:{}", id.as_i64()),
        Some("item_id:"),
        QueryType::Untag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )
    .unwrap();

    assert!(removed_rows(&store).is_empty());
    assert!(find(&store, &registry, "color:red").is_empty());
}

#[test]
fn a_lost_hardlink_drops_only_its_own_path() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    std::fs::hard_link(root.join("a.txt"), root.join("link.txt")).unwrap();
    index(&store, &registry, &[&root]);
    std::fs::remove_file(root.join("link.txt")).unwrap();
    index(&store, &registry, &[&root]);
    assert!(find(&store, &registry, "path:*link.txt").is_empty());
    assert_eq!(find(&store, &registry, "path:*a.txt").len(), 1);
}

#[test]
fn hardlinks_do_not_duplicate_inode_attributes() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    std::fs::hard_link(root.join("a.txt"), root.join("link.txt")).unwrap();
    index(&store, &registry, &[&root]);

    let items = find(&store, &registry, "path:*a.txt");
    let tags = &items[0].tags.entries;
    let count = |t: &str| {
        tags.iter()
            .filter(|e| e.typed_tag.tag_type().as_str() == t)
            .count()
    };

    assert_eq!(count("size"), 1);
    assert_eq!(count("mtime"), 1);
    assert_eq!(count("is_dir"), 1);
    assert_eq!(count("filename"), 2);
    assert_eq!(count("path"), 2);
    assert_eq!(count("stem"), 2);
}

fn location_paths(store: &Store, id: ItemId) -> Vec<String> {
    let p = store.path_for_target(TargetTable::Locations);
    let sql = format!(
        "SELECT path FROM read_parquet('{}') WHERE item_id = {} ORDER BY path",
        p.to_string_lossy(),
        id.as_i64()
    );
    store
        .conn
        .prepare(&sql)
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn location_row_count(store: &Store) -> usize {
    let p = store.path_for_target(TargetTable::Locations);
    let sql = format!("SELECT count(*) FROM read_parquet('{}')", p.to_string_lossy());
    store
        .conn
        .prepare(&sql)
        .unwrap()
        .query_row([], |row| row.get::<_, i64>(0))
        .unwrap() as usize
}

#[test]
fn indexing_a_subdir_leaves_sibling_rows_untouched() {
    let (store, registry, _d, root) = setup(&["x/a.txt", "y/b.txt"]);
    index(&store, &registry, &[&root]);
    let before = location_row_count(&store);

    index(&store, &registry, &[&root.join("x")]);

    assert_eq!(location_row_count(&store), before);
}

#[test]
fn two_roots_are_both_indexed() {
    let (store, registry, _d, root) = setup(&["x/a.txt", "y/b.txt"]);
    index(&store, &registry, &[&root.join("x"), &root.join("y")]);
    assert_eq!(find(&store, &registry, "filename:a.txt").len(), 1);
    assert_eq!(find(&store, &registry, "filename:b.txt").len(), 1);
}

#[test]
fn indexing_no_roots_scans_nothing() {
    let (store, registry, _d, _root) = setup(&["a.txt"]);

    let empty: &[&Path] = &[];
    let count = Indexer::new(&store, &registry)
        .run(empty, None::<&fn(usize)>, false)
        .unwrap();

    assert_eq!(count, 0, "an empty root list must scan no entries");
    assert!(
        find(&store, &registry, "filename:a.txt").is_empty(),
        "an empty root list must index nothing"
    );
}

#[test]
fn losing_one_hardlink_drops_only_that_location_row() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    std::fs::hard_link(root.join("a.txt"), root.join("link.txt")).unwrap();
    index(&store, &registry, &[&root]);
    let id = find(&store, &registry, "path:*a.txt")[0].id;
    assert_eq!(location_paths(&store, id).len(), 2);

    std::fs::remove_file(root.join("link.txt")).unwrap();
    index(&store, &registry, &[&root]);
    let kept = location_paths(&store, id);
    assert_eq!(kept.len(), 1);
    assert!(kept[0].ends_with("a.txt"));
}

fn view_sql(store: &Store, name: &str) -> String {
    store
        .conn
        .query_row(
            "SELECT sql FROM duckdb_views() WHERE view_name = ?",
            [name],
            |r| r.get(0),
        )
        .unwrap()
}

fn file_ref_rank(store: &Store, id: ItemId) -> i64 {
    let p = store.path_for_target(TargetTable::FileReferences);
    let sql = format!(
        "SELECT rank FROM read_parquet('{}') WHERE item_id = {}",
        p.to_string_lossy(),
        id.as_i64()
    );
    store.conn.query_row(&sql, [], |r| r.get(0)).unwrap()
}

fn mtime_of(m: &std::fs::Metadata) -> SystemTime {
    m.modified().unwrap()
}

fn recreate_apart(root: &Path, names: &[&str], len: usize, at: SystemTime) {
    for name in names {
        let p = root.join(name);
        std::fs::write(&p, vec![b'x'; len]).unwrap();
        filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(at))
            .unwrap();
    }
}

#[test]
fn oneview_reads_user_tags_and_removed_files_once_per_role() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    index(&store, &registry, &[&root]);

    let sql = view_sql(&store, "_oneview");
    assert_eq!(sql.matches("user_tags.parquet").count(), 2);
    assert_eq!(sql.matches("removed_files.parquet").count(), 6);
}

#[test]
fn a_returning_file_keeps_its_system_rank() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    index(&store, &registry, &[&root]);
    tag(&store, &registry, "filename:a.txt", "color:red");
    let id = find(&store, &registry, "filename:a.txt")[0].id;
    ttfm::rank::set_rank_by_id(&store, &registry, id.as_i64(), true, 42)
        .unwrap();

    let away = root.parent().unwrap().join("a.txt");
    std::fs::rename(root.join("a.txt"), &away).unwrap();
    index(&store, &registry, &[&root]);
    std::fs::rename(&away, root.join("a.txt")).unwrap();
    index(&store, &registry, &[&root]);

    assert_eq!(file_ref_rank(&store, id), 42);
}

#[test]
fn a_reindexed_file_keeps_its_system_rank() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    index(&store, &registry, &[&root]);
    let id = find(&store, &registry, "filename:a.txt")[0].id;
    ttfm::rank::set_rank_by_id(&store, &registry, id.as_i64(), true, 42)
        .unwrap();

    let later = SystemTime::now() + std::time::Duration::from_secs(120);
    filetime::set_file_mtime(
        root.join("a.txt"),
        filetime::FileTime::from_system_time(later),
    )
    .unwrap();
    index(&store, &registry, &[&root]);

    assert_eq!(file_ref_rank(&store, id), 42);
}

#[test]
fn a_removed_file_returning_in_another_dir_keeps_its_item_id() {
    let (store, registry, _d, root) = setup(&["a.txt", "sub/keep.txt"]);
    index(&store, &registry, &[&root]);
    tag(&store, &registry, "filename:a.txt", "color:red");
    let id = find(&store, &registry, "filename:a.txt")[0].id;

    let away = root.parent().unwrap().join("a.txt");
    std::fs::rename(root.join("a.txt"), &away).unwrap();
    index(&store, &registry, &[&root]);
    assert_eq!(removed_rows(&store).len(), 1);

    std::fs::rename(&away, root.join("sub").join("a.txt")).unwrap();
    index(&store, &registry, &[&root]);

    assert_eq!(find(&store, &registry, "filename:a.txt")[0].id, id);
    assert_eq!(find(&store, &registry, "color:red").len(), 1);
    assert!(removed_rows(&store).is_empty());
}

#[test]
fn a_split_hardlink_pair_does_not_share_an_item_id() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    std::fs::hard_link(root.join("a.txt"), root.join("b.txt")).unwrap();
    index(&store, &registry, &[&root]);
    tag(&store, &registry, "filename:a.txt", "color:red");
    let m = std::fs::metadata(root.join("a.txt")).unwrap();

    std::fs::remove_file(root.join("a.txt")).unwrap();
    std::fs::remove_file(root.join("b.txt")).unwrap();
    index(&store, &registry, &[&root]);

    recreate_apart(&root, &["a.txt", "b.txt"], m.len() as usize, mtime_of(&m));
    index(&store, &registry, &[&root]);

    let a = find(&store, &registry, "filename:a.txt")[0].id;
    let b = find(&store, &registry, "filename:b.txt")[0].id;
    assert_ne!(a, b);
}

#[test]
fn initialize_tables_creates_removed_files_with_typed_columns() {
    let (store, _registry, _d, _root) = setup(&[]);
    let p = store.path_for_target(TargetTable::RemovedFiles);
    assert!(p.exists());

    let sql = format!(
        "SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM read_parquet('{}'))",
        p.to_string_lossy()
    );
    let cols: Vec<(String, String)> = store
        .conn
        .prepare(&sql)
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        cols,
        vec![
            ("item_id".into(), "BIGINT".into()),
            ("rank".into(), "BIGINT".into()),
            ("file_id".into(), "UUID".into()),
            ("scan_hash".into(), "BIGINT".into()),
            ("basename_scan_hash".into(), "BIGINT".into()),
            ("path".into(), "VARCHAR".into()),
            ("size".into(), "BIGINT".into()),
            ("mtime".into(), "BIGINT".into()),
            ("is_dir".into(), "BOOLEAN".into()),
            ("removed_file_at".into(), "BIGINT".into()),
        ]
    );
}
