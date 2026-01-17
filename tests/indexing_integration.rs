use file_id::get_file_id;
use tempfile::tempdir;
use ttfm::{FileManager, TargetTable};

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
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 2);
    assert_eq!(fm.search("filename:a.txt").unwrap().len(), 1);

    // 2. 変更なし: そのまま再スキャン (2)
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 2);

    // 3. 追加: b.rs を作成 (root + a.txt + b.rs = 3)
    let path_b = root.join("b.rs");
    std::fs::write(&path_b, "fn main() {}").unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();

    let res = fm.search(all_files).unwrap();
    assert_eq!(res.len(), 3);

    // 4. 更新: a.txt の内容を変更 (サイズ変更)
    // 実体(ID)が変わらないことを確認
    let old_id = fm.search("filename:a.txt").unwrap()[0].id;
    std::fs::write(&path_a, "updated content with more bytes").unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();

    let res_edit = fm.search("filename:a.txt").unwrap();
    let files_edit: Vec<_> =
        res_edit.iter().filter(|r| r.item_kind == "file").collect();
    assert_eq!(files_edit.len(), 1, "Should find exactly one a.txt");
    assert_eq!(
        files_edit[0].id, old_id,
        "Item ID must be reused after content edit"
    );

    // 5. 削除: b.rs を削除 (root + a.txt = 2)
    std::fs::remove_file(&path_b).unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();
    assert_eq!(fm.search(all_files).unwrap().len(), 2);

    let res_b_del = fm.search("filename:b.rs").unwrap();
    let files_b_del: Vec<_> =
        res_b_del.iter().filter(|r| r.item_kind == "file").collect();
    assert_eq!(
        files_b_del.len(),
        0,
        "b.rs must be removed from search results"
    );

    // 6. 別名追加 (ハードリンク): a.txt の別名として c.txt を作成
    let path_c = root.join("c.txt");
    std::fs::hard_link(&path_a, &path_c).unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false)
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

    let res_inode = fm.search(&query).unwrap();
    let files_inode: Vec<_> =
        res_inode.iter().filter(|r| r.item_kind == "file").collect();

    // 検証：1つの実体に対して a.txt と c.txt の 2つの場所がヒットすること
    assert_eq!(
        files_inode.len(),
        2,
        "Searching by FileID must return both hard-linked names"
    );
    let names: Vec<_> = files_inode.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"c.txt"));
    assert_eq!(
        files_inode[0].id, files_inode[1].id,
        "Both results must share the same Item ID"
    );
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
    let _items_path_buf = fm.path_for_target(TargetTable::ItemReferences);

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
    let tt_items: Vec<_> = results
        .iter()
        .filter(|r| r.item_kind == "typedtag" && r.name == "extension:txt")
        .collect();
    assert_eq!(
        tt_items.len(),
        1,
        "Should find the typedtag item for extension:txt"
    );

    // 2. extension:txt で検索 -> ファイルだけが見つかるはず（ノイズがないこと）
    // オリジナル通りのフィルタロジックに戻す
    let results = fm.search("extension:txt").unwrap();
    let files: Vec<_> =
        results.iter().filter(|r| r.item_kind == "file").collect();
    let tags: Vec<_> = results
        .iter()
        .filter(|r| r.item_kind == "typedtag")
        .collect();

    assert_eq!(files.len(), 1, "Should find the file");
    assert_eq!(
        tags.len(),
        0,
        "Should NOT find the typedtag item itself as noise"
    );
}

#[test]
fn test_no_empty_extension_system_item() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("no_extension"), "test").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    let results = fm
        .search("item_kind:typedtag & name:\"extension:\"")
        .unwrap();
    assert!(
        results.is_empty(),
        "Should NOT register 'extension:' system item"
    );
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
    assert!(!fm
        .search("item_kind:type & name:item_kind")
        .unwrap()
        .is_empty());
}
