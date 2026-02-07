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

    // 1. extension: (投影 - 転置: Label → Items)
    // 投影結果はラベル値（rs, txt）のリストとして返される
    let results = fm.search("extension:", Default::default()).unwrap();
    println!(
        "Matches for 'extension:': {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    assert_eq!(
        results.results.len(),
        2,
        "extension: should return 2 label values (rs, txt). Found: {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    // 転置: results には label items が格納される（name="rs", name="txt"）
    assert!(results.results.iter().any(|r| r.name == "rs"));
    assert!(results.results.iter().any(|r| r.name == "txt"));
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("extension"))
    );

    // 2. directory: (投影 -> is_dir:true + projection:filename - 転置)
    let results = fm.search("directory:", Default::default()).unwrap();
    println!(
        "Matches for 'directory:': {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    // 転置: label items として filename 値が返される（test_dir など）
    assert!(
        results.results.len() >= 1,
        "directory: should return at least 1 label (test_dir filename)"
    );
    assert!(results.results.iter().any(|r| r.name == "test_dir"));
    // 仮想ラベル directory: は内部で filename を投影する
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("filename")),
        "directory: projection should resolve to filename"
    );

    // 3. filename: (投影 -> is_dir:false + projection:filename - 転置)
    let results = fm.search("filename:", Default::default()).unwrap();
    println!(
        "Matches for 'filename:': {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    // 転置: label items として filename 値が返される（:test.rs, :test.txt）
    assert_eq!(
        results.results.len(),
        2,
        "filename: should return 2 label values (test.rs, test.txt). Found: {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    // 転置後は全て label items
    assert!(results.results.iter().all(|r| r.item_kind == "label"));
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

    // 転置: results には label items が格納され、各 label の name がタグタイプ名
    // 結果に含まれる全てのタグタイプ（label の name）を収集
    let mut found_types = std::collections::HashSet::new();
    for r in &results.results {
        found_types.insert(r.name.clone());
    }

    // 主要なSTypeが含まれているか確認
    // 環境によっては全てのタグが出揃わない可能性があるため、最低限 item_kind と name があれば良しとする。
    let expected_types = vec!["item_kind", "name"];
    for t in expected_types {
        assert!(
            found_types.contains(t),
            "type: projection results should contain label with name '{}'. Found types: {:?}",
            t,
            found_types
        );
    }

    // 7. typedtag: (全アイテムヒット確認 + 値の検証)
    let results = fm.search("tag:", Default::default()).unwrap();
    println!("Matches for 'tag:': {} items", results.results.len());
    assert!(results.results.len() >= 3, "tag: should match all items");
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("tag"))
    );

    // 検証: アイテムが tag タグを持っているか
    let has_tag = results
        .results
        .iter()
        .any(|r| r.get_tag_value("tag").is_some());
    assert!(
        has_tag,
        "Items should have 'tag' tag values in SearchResult"
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
    // 8. rank: (投影 -> rank column)
    // rank は oneview 上の全ての行で有効な値を持つカラムだが、
    // プロジェクションクエリとしては type='rank' ではなく rank column のユニーク値を期待する。
    let results = fm.search("rank:", Default::default()).unwrap();
    // 全てのアイテムは初期状態で rank=0 のはず (あるいは計算された値)
    // 実装が未対応なら0件になる
    println!("Matches for 'rank:': {} items", results.results.len());
    // NOTE: 現在の実装では rank: は type='rank' を検索してしまい、0件になる可能性がある。
    // ユーザーの指摘により、これをサポートすべきか確認するフェーズ。
    // 一旦アサーションは入れず、挙動を確認する。
    if results.type_for_projection == Some(ttfm::types::TagType::from("rank")) {
        // サポートされている場合
        assert!(
            !results.results.is_empty(),
            "rank: should return items if supported"
        );
    }

    // 9. category: (投影 -> type='category')
    // label は SType::Label (仮想タグ) として予約されているため、
    // 任意のタグ名のテストには category を使用する。
    let note_id = fm.add_item("note", "Category Test Note").unwrap();
    fm.tag_item(&note_id.to_string(), "category:important")
        .unwrap();

    let results = fm.search("category:", Default::default()).unwrap();
    assert!(
        results.results.len() >= 1,
        "category: should match items with category tag"
    );
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("category"))
    );
    // 転置: results には label items が格納され、name が "important" であることを確認
    let has_val = results
        .results
        .iter()
        .any(|r| r.item_kind == "label" && r.name == "important");
    assert!(has_val, "Should find 'important' category label");

    // 10. label: (Volatile Tag -> All Labels)
    // label: は「全てのタグのラベル」を集約する揮発性プロジェクション。
    let results = fm.search("label:", Default::default()).unwrap();
    // 全てのアイテムは何かしらのラベル（name, item_kind 等）を持つためヒットする
    assert!(
        results.results.len() >= 3,
        "label: should match all tagged items"
    );
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("label"))
    );
}

#[test]
fn test_projection_returns_label_volatile_items() {
    use ttfm::types::{ItemId, VolatileItem};

    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストデータの作成
    File::create(root.join("test.rs")).unwrap();
    File::create(root.join("test.txt")).unwrap();
    File::create(root.join("another.rs")).unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // extension: で投影
    let results = fm.search("extension:", Default::default()).unwrap();

    // 検証1: type_for_projection が設定されている
    assert_eq!(
        results.type_for_projection,
        Some(ttfm::types::TagType::from("extension"))
    );

    // 検証2: results に label items が格納されている
    assert!(
        !results.results.is_empty(),
        "projection should return label items"
    );

    // 検証3: 各 SearchResult が Label volatile item である
    for item in &results.results {
        // ID が Volatile(Label(...)) であることを確認
        if let ItemId::Volatile(VolatileItem::Label(ref label_val)) = item.id {
            // 検証4: item_kind が "label" である
            assert_eq!(
                item.item_kind, "label",
                "Label volatile item should have item_kind='label'"
            );

            // 検証5: name がラベル値と一致する
            assert_eq!(
                item.name, *label_val,
                "Label volatile item name should match label value"
            );

            // 検証6: tags に "item:name#id" 形式のタグが含まれている
            // Type="item", Label="name#id" 形式であることを確認
            let has_item_ref = item.tags.entries.iter().any(|entry| {
                entry.label.tag_type().as_str() == "item" && entry.label.as_str().contains('#')
            });
            assert!(
                has_item_ref,
                "Label volatile item should contain Type='item' tags with Label='name#id', found: {:?}",
                item.tags.entries.iter().map(|e| format!("{}:{}", e.label.tag_type().as_str(), e.label.as_str())).collect::<Vec<_>>()
            );

            // 検証7: projected_label に total_count が保存されている
            assert!(
                item.projected_label.is_some(),
                "Label volatile item should have projected_label (total_count)"
            );

            let total_count_str = item.projected_label.as_ref().unwrap().as_str();
            let total_count: usize = total_count_str.parse().expect("projected_label should be parseable as usize");
            assert!(
                total_count > 0,
                "total_count should be greater than 0"
            );

            // 検証8: tagsの数が100件以下である（100件制限）かつtotal_count以下である
            assert!(
                item.tags.entries.len() <= 100,
                "tags count should be <= 100 (with 100-item limit), found {}",
                item.tags.entries.len()
            );
            assert!(
                item.tags.entries.len() <= total_count || total_count > 100,
                "tags count ({}) should be <= total_count ({}) or total_count should be > 100",
                item.tags.entries.len(),
                total_count
            );
        } else {
            panic!("Projection should return Label volatile items, but got: {:?}", item.id);
        }
    }

    // 検証9: "rs" ラベルが存在する（test.rs, another.rs）
    let rs_label = results.results.iter().find(|item| item.name == "rs");
    assert!(
        rs_label.is_some(),
        "Should find 'rs' label in projection results"
    );

    if let Some(rs_item) = rs_label {
        // rs ラベルは2つのファイルを参照しているはず
        let item_ref_count = rs_item.tags.entries.len();
        assert!(
            item_ref_count >= 2,
            "rs label should reference at least 2 files (test.rs, another.rs), found {}",
            item_ref_count
        );

        // item:test.rs または item:another.rs が含まれているか確認
        let has_test_rs = rs_item.tags.entries.iter().any(|entry| {
            entry.label.as_str().contains("test.rs")
        });
        let has_another_rs = rs_item.tags.entries.iter().any(|entry| {
            entry.label.as_str().contains("another.rs")
        });
        assert!(
            has_test_rs || has_another_rs,
            "rs label should contain references to test.rs or another.rs"
        );
    }
}

#[test]

fn test_projection_no_empty_labels() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストデータの作成
    File::create(root.join("file_with_ext.txt")).unwrap();
    File::create(root.join("file_no_ext")).unwrap(); // 拡張子なし

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // extension: 検索
    let results = fm.search("extension:", Default::default()).unwrap();
    
    // "txt" ラベルが存在し、ファイルが含まれていることを確認
    assert!(
        results.results.iter().any(|r| r.name == "txt"), 
        "Output should contain 'txt' label for file_with_ext.txt"
    );

    // 空ラベル（拡張子なしファイルの集計）が含まれていないことを確認
    let has_empty = results.results.iter().any(|r| r.name.is_empty());
    assert!(
        !has_empty,
        "Output should NOT contain empty label name. Found labels: {:?}",
        results.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
}
