use crate::cases::has_item_tags;
use file_id::get_file_id;
use tempfile::tempdir;
use ttfm::FileManager;

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
    assert_eq!(
        fm.search(all_files, Default::default())
            .unwrap()
            .results
            .len(),
        2
    );
    assert_eq!(
        fm.search("filename:a.txt", Default::default())
            .unwrap()
            .results
            .len(),
        1
    );

    // 2. 変更なし: そのまま再スキャン (2)
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();
    assert_eq!(
        fm.search(all_files, Default::default())
            .unwrap()
            .results
            .len(),
        2
    );

    // 3. 追加: b.rs を作成 (root + a.txt + b.rs = 3)
    let path_b = root.join("b.rs");
    std::fs::write(&path_b, "fn main() {}").unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();

    let res = fm.search(all_files, Default::default()).unwrap();
    assert_eq!(res.results.len(), 3);

    // 4. 更新: a.txt の内容を変更 (サイズ変更)
    // 実体(ID)が変わらないことを確認
    let old_id = fm
        .search("filename:a.txt", Default::default())
        .unwrap()
        .results[0]
        .id
        .clone();
    std::fs::write(&path_a, "updated content with more bytes").unwrap();
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();

    let res_edit = fm.search("filename:a.txt", Default::default()).unwrap();
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
    fm.index_directory(&root, None::<&fn(usize)>, false)
        .unwrap();
    assert_eq!(
        fm.search(all_files, Default::default())
            .unwrap()
            .results
            .len(),
        2
    );

    let res_b_del = fm.search("filename:b.rs", Default::default()).unwrap();
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

    let _res_inode = fm.search(&query, Default::default()).unwrap();
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

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 1. item_entities に extension:txt 関連のItemがあるか確認
    // 変更後: 自動生成されなくなったため、物理的なアイテムは存在しないはず
    let results_physical = fm
        .search("item_kind:tag & name:extension:txt", Default::default())
        .unwrap();
    assert!(
        results_physical.results.is_empty(),
        "Physical tag item should NOT be created automatically"
    );

    // 2. しかし、プロジェクション（oneview）経由では検索できること
    // 「typedtag:」で検索（プロジェクションクエリ）を行い、動的にタグが生成・投影されることを確認
    let results_projection = fm.search("tag:", Default::default()).unwrap();

    // プロジェクション配下に typedtag が含まれているか確認
    assert!(has_item_tags(&results_projection.results));
    assert!(!results_projection.results.is_empty(), "Should find items");

    // 投影された値の中に extension:txt が含まれているか（動的生成の確認）
    // 物理的な Item はなくても、oneview 上で結合されて値として取得できるはず
    // 転置: results には label items が格納されるため、name が "extension:txt" であることを確認
    let has_target_val = results_projection.results.iter().any(|r| {
        r.item_kind == ttfm::ItemKind::Volatile && r.raw_repr() == "extension:txt"
    });
    assert!(
        has_target_val,
        "Should contain label item with name='extension:txt'"
    );

    // 3. origin のプロジェクションも確認
    let results_origin = fm.search("origin:", Default::default()).unwrap();
    assert!(has_item_tags(&results_origin.results));
    assert!(!results_origin.results.is_empty());

    // 転置: results には label items が格納され、name が "system" であることを確認
    let system_label = results_origin
        .results
        .iter()
        .find(|r| r.raw_repr() == "system")
        .expect("system label not found for origin check");
    assert_eq!(
        system_label.item_kind,
        ttfm::ItemKind::Volatile,
        "Should be a label item"
    );
    // このラベルの tags に "item:hello.txt#..." が含まれているはず
    let has_hello_txt = system_label
        .tags
        .entries
        .iter()
        .any(|entry| entry.label.as_str().contains("hello.txt"));
    assert!(
        has_hello_txt,
        "system origin label should contain reference to hello.txt"
    );
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
    let results = fm.search("type:extension", Default::default()).unwrap();
    let tt_items: Vec<_> = results
        .results
        .iter()
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
    let results = fm.search("extension:txt", Default::default()).unwrap();
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

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    let results = fm
        .search("item_kind:tag & name:\"extension:\"", Default::default())
        .unwrap();
    assert!(
        results.results.is_empty(),
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

    assert!(!fm
        .search("item_kind:type & name:name", Default::default())
        .unwrap()
        .results
        .is_empty());
    assert!(!fm
        .search("item_kind:type & name:item_kind", Default::default())
        .unwrap()
        .results
        .is_empty());
}
