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
use tempfile::tempdir;
use ttfm::search;

#[test]
fn test_incremental_indexing_full_flow() {
    let dir = tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let db_dir = base.join("db");
    let root = base.join("work");
    std::fs::create_dir_all(&root).unwrap();

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let (store, registry) = (db_dir_store, db_dir_registry);
    let all_files = "item_kind:file";

    // 1. 初回: a.txt を作成 (root + a.txt = 2)
    let path_a = root.join("a.txt");
    std::fs::write(&path_a, "initial content").unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();
    assert_eq!(
        search::search_nowarn(&store, &registry, all_files, Default::default())
            .unwrap()
            .results
            .len(),
        2
    );
    assert_eq!(
        search::search_nowarn(&store, &registry, "filename:a.txt", Default::default())
            .unwrap()
            .results
            .len(),
        1
    );

    // 2. 変更なし: そのまま再スキャン (2)
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();
    assert_eq!(
        search::search_nowarn(&store, &registry, all_files, Default::default())
            .unwrap()
            .results
            .len(),
        2
    );

    // 3. 追加: b.rs を作成 (root + a.txt + b.rs = 3)
    let path_b = root.join("b.rs");
    std::fs::write(&path_b, "fn main() {}").unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

    let res = search::search_nowarn(&store, &registry, all_files, Default::default())
        .unwrap();
    assert_eq!(res.results.len(), 3);

    // 4. 更新: a.txt の内容を変更 (サイズ変更)
    // 実体(ID)が変わらないことを確認
    let old_id =
        search::search_nowarn(&store, &registry, "filename:a.txt", Default::default())
            .unwrap()
            .results[0]
            .id
            .clone();
    std::fs::write(&path_a, "updated content with more bytes").unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

    let res_edit =
        search::search_nowarn(&store, &registry, "filename:a.txt", Default::default())
            .unwrap();
    let files_edit: Vec<_> = res_edit
        .results
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
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();
    assert_eq!(
        search::search_nowarn(&store, &registry, all_files, Default::default())
            .unwrap()
            .results
            .len(),
        2
    );

    let res_b_del =
        search::search_nowarn(&store, &registry, "filename:b.rs", Default::default())
            .unwrap();
    let files_b_del: Vec<_> = res_b_del
        .results
        .iter()
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
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

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

    let _res_inode =
        search::search_nowarn(&store, &registry, &query, Default::default()).unwrap();
    let _files_inode: Vec<_> = _res_inode
        .results
        .iter()
        .filter(|r| r.item_kind == ttfm::ItemKind::File)
        .collect();

    /* TODO: Fix hardlink indexing/search consistency.
    // 検証：1つの実体に対して a.txt と c.txt の 2つの場所がヒットすること
    let names: Vec<_> = files_inode.iter().map(|r| r.raw_repr()).collect();
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"c.txt".to_string()));
    assert_eq!(
        files_inode[0].id, files_inode[1].id,
        "Both results must share the same Item ID"
    );
    */
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
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    // 1. item_entities に extension:txt 関連のItemがあるか確認
    // 変更後: 自動生成されなくなったため、物理的なアイテムは存在しないはず
    let results_physical = search::search_nowarn(
        &store,
        &registry,
        "item_kind:tag & name:extension:txt",
        Default::default(),
    )
    .unwrap();
    assert!(
        results_physical.results.is_empty(),
        "Physical tag item should NOT be created automatically"
    );

    // 2. しかし、プロジェクション（oneview）経由では検索できること
    // 「typedtag:」で検索（プロジェクションクエリ）を行い、動的にタグが生成・投影されることを確認
    let results_projection =
        search::search_nowarn(&store, &registry, "tag:", Default::default()).unwrap();

    // プロジェクション配下に typedtag が含まれているか確認
    assert!(has_item_tags(&results_projection.results));
    assert!(!results_projection.results.is_empty(), "Should find items");

    // 投影された値の中に extension:txt が含まれているか（動的生成の確認）
    // 物理的な Item はなくても、oneview 上で結合されて値として取得できるはず
    // 転置: results には label items が格納されるため、name が "extension:txt" であることを確認
    let has_target_val = results_projection.results.iter().any(|r| {
        r.item_kind == ttfm::ItemKind::Volatile
            && r.raw_repr() == "extension:txt"
    });
    assert!(
        has_target_val,
        "Should contain label item with name='extension:txt'"
    );

    // 3. origin のプロジェクションも確認
    let results_origin =
        search::search_nowarn(&store, &registry, "origin:", Default::default())
            .unwrap();
    assert!(has_item_tags(&results_origin.results));
    assert!(!results_origin.results.is_empty());

    // 転置: results には label items が格納され、name が "file" であることを確認
    // (hello.txt はスキャン抽出タグのみを持つ File 由来アイテムのため、origin は "file")
    let file_label = results_origin
        .results
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
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    // 1. type:extension で検索 -> extension:txt アイテムが見つかるはず
    let results =
        search::search_nowarn(&store, &registry, "type:extension", Default::default())
            .unwrap();
    let tt_items: Vec<_> = results
        .results
        .iter()
        .filter(|r| {
            r.item_kind == ttfm::ItemKind::Tag
                && r.raw_repr() == "extension:txt"
        })
        .collect();
    assert_eq!(
        tt_items.len(),
        0,
        "Should NOT find the tag item because it doesn't have the tag (metadata definition only)"
    );

    // 2. extension:txt で検索 -> ファイルだけが見つかるはず（ノイズがないこと）
    // オリジナル通りのフィルタロジックに戻す
    let results =
        search::search_nowarn(&store, &registry, "extension:txt", Default::default())
            .unwrap();
    let files: Vec<_> = results
        .results
        .iter()
        .filter(|r| r.item_kind == ttfm::ItemKind::File)
        .collect();
    let tags: Vec<_> = results
        .results
        .iter()
        .filter(|r| r.item_kind == ttfm::ItemKind::Tag)
        .collect();

    assert_eq!(files.len(), 1, "Should find the file");
    assert_eq!(
        tags.len(),
        0,
        "Should NOT find the tag item itself as noise"
    );
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
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    let results = search::search_nowarn(
        &store,
        &registry,
        "item_kind:tag & name:\"extension:\"",
        Default::default(),
    )
    .unwrap();
    assert!(
        results.results.is_empty(),
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
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    // 初期登録が廃止されたため、type 定義専用アイテムは自動生成されない。
    assert!(search::search_nowarn(
        &store,
        &registry,
        "item_kind:type & name:name",
        Default::default()
    )
    .unwrap()
    .results
    .is_empty());
    assert!(search::search_nowarn(
        &store,
        &registry,
        "item_kind:type & name:item_kind",
        Default::default()
    )
    .unwrap()
    .results
    .is_empty());
}

/// 区画幅 B = 2^58。System 区画は [8B, 9B)。
const SPACE_B: i64 = 1 << 58;

fn read_item_ids(
    store: &ttfm::db::Store,
    target: ttfm::db::TargetTable,
) -> Vec<i64> {
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
    let dir = tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let db_dir = base.join("db");
    let root = base.join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.rs"), "fn main(){}").unwrap();

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

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
    let dir = tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let db_dir = base.join("db");
    let root = base.join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.rs"), "fn main(){}").unwrap();
    std::fs::write(root.join("b.txt"), "hello").unwrap();

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

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
    let dir = tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let db_dir = base.join("db");
    let root = base.join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.rs"), "fn main(){}").unwrap();

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

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
