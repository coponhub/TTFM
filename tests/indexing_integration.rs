use ttfm::FileManager;
use ttfm::db::TargetTable;
use std::path::Path;
use tempfile::tempdir;
use sea_query::{Query, Expr, PostgresQueryBuilder};
use ttfm::db::{Col, Tbl};
use ttfm::util;

#[test]
fn test_incremental_indexing_full_flow() {
    let dir = tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let db_dir = base.join("db");
    let root = base.join("work");
    std::fs::create_dir_all(&root).unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    let all_files = "item_kind:file";

    // 1. 初回: a.txt を作成 (root + a.txt = 2)
    let path_a = root.join("a.txt");
    std::fs::write(&path_a, "initial content").unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 2);
    assert_eq!(fm.search("filename:a.txt").unwrap().len(), 1);

    // 2. 変更なし: そのまま再スキャン (2)
    fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 2);

    // 3. 追加: b.rs を作成 (root + a.txt + b.rs = 3)
    let path_b = root.join("b.rs");
    std::fs::write(&path_b, "fn main() {}").unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 3);
    assert_eq!(fm.search("filename:b.rs").unwrap().len(), 1);

    // 4. 更新: a.txt のサイズを変更 (3)
    std::fs::write(&path_a, "updated content with more bytes").unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 3);

    // 5. 削除: b.rs を削除 (root + a.txt = 2)
    std::fs::remove_file(&path_b).unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 2);
    assert_eq!(fm.search("filename:b.rs").unwrap().len(), 0);

    // 6. 移動: a.txt -> c.txt (root + c.txt = 2)
    let path_c = root.join("c.txt");
    std::fs::rename(&path_a, &path_c).unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 2);
    assert_eq!(fm.search("filename:a.txt").unwrap().len(), 0);
    assert_eq!(fm.search("filename:c.txt").unwrap().len(), 1);
}

#[test]
fn test_system_items_registration() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("hello.txt"), "hello").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 1. item_entities に extension:txt 関連のItemがあるか確認
    let items_path_buf = fm.path_for_target(TargetTable::ItemEntities);
    let items_path = items_path_buf.to_string_lossy();
    let query = Query::select()
        .columns([Col::ItemKind, Col::Content])
        .from_subquery(util::parquet_query(&items_path), Tbl::ItemEntities)
        .and_where(Expr::col(Col::Content).is_in(["extension", "txt", "extension:txt"]))
        .to_string(PostgresQueryBuilder);

    // 内部DB接続へのアクセスが必要なため、 search 等で代用するか、
    // オリジナルのテストが意図していた「システムへの登録確認」を search で行う。
    let results = fm.search("name:extension:txt & origin:system").unwrap();
    assert!(!results.is_empty());
}

#[test]
fn test_typedtag_listing_via_type_query() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("test.txt"), "hello").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 1. type:extension で検索 -> extension:txt アイテムが見つかるはず
    let results = fm.search("type:extension").unwrap();
    let tt_items: Vec<_> = results.iter()
        .filter(|r| r.item_kind == "typedtag" && r.name == "extension:txt")
        .collect();
    assert_eq!(tt_items.len(), 1, "Should find the typedtag item for extension:txt");

    // 2. extension:txt で検索 -> ファイルだけが見つかるはず（ノイズがないこと）
    // オリジナル通りのフィルタロジックに戻す
    let results = fm.search("extension:txt").unwrap();
    let files: Vec<_> = results.iter().filter(|r| r.item_kind == "file").collect();
    let tags: Vec<_> = results.iter().filter(|r| r.item_kind == "typedtag").collect();
    
    assert_eq!(files.len(), 1, "Should find the file");
    assert_eq!(tags.len(), 0, "Should NOT find the typedtag item itself as noise");
}

#[test]
fn test_no_empty_extension_system_item() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("no_extension"), "test").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    let results = fm.search("item_kind:typedtag & name:extension:").unwrap();
    assert!(results.is_empty(), "Should NOT register 'extension:' system item");
}

#[test]
fn test_definition_only_items_registration() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    
    std::fs::write(root.join("test.txt"), "").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    assert!(!fm.search("item_kind:type & name:name").unwrap().is_empty());
    assert!(!fm.search("item_kind:type & name:kind").unwrap().is_empty());
}