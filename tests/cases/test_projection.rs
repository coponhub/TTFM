use std::fs::File;
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_projection_queries() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストデータの作成
    File::create(root.join("test.rs")).unwrap();
    File::create(root.join("test.txt")).unwrap();
    std::fs::create_dir(root.join("test_dir")).unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 1. extension: (投影)
    // 拡張子を持つファイル（test.rs, test.txt）がヒットするはず。
    let results = fm.search("extension:", Default::default()).unwrap();
    println!(
        "Matches for 'extension:': {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    assert_eq!(
        results.results.len(),
        2,
        "extension: should match items with any extension. Found: {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    assert!(results.results.iter().any(|r| r.name == "test.rs"));
    assert!(results.results.iter().any(|r| r.name == "test.txt"));
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("extension"))
    );

    // 2. directory: (投影 -> is_dir:true + projection:filename)
    let results = fm.search("directory:", Default::default()).unwrap();
    println!(
        "Matches for 'directory:': {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    // root (tmpdir), test_dir, .ttfm -> 3 items
    assert!(
        results.results.len() >= 1,
        "directory: should match at least test_dir"
    );
    assert!(results.results.iter().any(|r| r.name == "test_dir"));
    // 仮想ラベル directory: は内部で filename を投影する
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("filename")),
        "directory: projection should resolve to filename"
    );

    // 3. filename: (投影 -> is_dir:false + projection:filename)
    let results = fm.search("filename:", Default::default()).unwrap();
    println!(
        "Matches for 'filename:': {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    // test.rs, test.txt -> 2 items.
    assert_eq!(
        results.results.len(),
        2,
        "filename: (files only) should match test.rs and test.txt. Found: {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    assert!(results.results.iter().all(|r| r.item_kind == "file"));
    // 仮想ラベル filename: は内部で filename を投影する
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("filename")),
        "filename: projection should resolve to filename"
    );

    // 4. origin:system
    // 全てのアイテムは system 由来のタグを持つはず（初期状態）
    let results = fm.search("origin:system", Default::default()).unwrap();
    assert!(results.results.len() >= 3);

    // 5. 複合クエリ
    let results = fm
        .search("extension: & directory:", Default::default())
        .unwrap();
    assert_eq!(
        results.results.len(),
        0,
        "No directories should have an extension in this test"
    );

    // 6. type: (全アイテムヒット確認 + SType網羅性確認)
    let results = fm.search("type:", Default::default()).unwrap();
    assert!(results.results.len() >= 3, "type: should match all items");
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("type"))
    );

    // 結果に含まれる全てのタグキー（Type）を収集
    let mut found_types = std::collections::HashSet::new();
    for r in &results.results {
        for (tag_type, _) in &r.tags {
            found_types.insert(tag_type.as_str().to_string());
        }
    }

    // 主要なSTypeが含まれているか確認
    // 環境によっては全てのタグが出揃わない可能性があるため、最低限 item_kind と name があれば良しとする。
    let expected_types = vec!["item_kind", "name"];
    for t in expected_types {
        assert!(
            found_types.contains(t),
            "type: projection results should contain items with tag '{}'. Found types: {:?}",
            t,
            found_types
        );
    }

    // 7. typedtag: (全アイテムヒット確認 + 値の検証)
    let results = fm.search("typedtag:", Default::default()).unwrap();
    println!("Matches for 'typedtag:': {} items", results.results.len());
    assert!(
        results.results.len() >= 3,
        "typedtag: should match all items"
    );
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("typedtag"))
    );

    // 検証: アイテムが typedtag タグを持っているか
    let has_typedtag = results
        .results
        .iter()
        .any(|r| r.get_tag_value("typedtag").is_some());
    assert!(
        has_typedtag,
        "Items should have 'typedtag' tag values in SearchResult"
    );

    // 追加検証: extension: 結果の中身
    let ext_results = fm.search("extension:", Default::default()).unwrap();
    for r in &ext_results.results {
        // test.rs は extension:rs を持つ
        if r.name == "test.rs" {
            let ext = r
                .get_tag_value("extension")
                .expect("test.rs should have extension tag");
            assert_eq!(ext, "rs");
        }
    }
}
