use std::fs::File;
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_integration_file_tagging() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. ファイル作成とインデックス
    let file_path = dir.path().join("doc.txt");
    File::create(&file_path).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    // 2. タグ付与
    let registered_paths = fm.search("extension:txt", Default::default()).unwrap();
    let item = registered_paths.results[0].primary_value().unwrap();

    fm.tag_item(&item, "status:reviewed").unwrap();

    // 3. 付与したタグで検索
    let results = fm.search("status:reviewed", Default::default()).unwrap();
    assert_eq!(results.results.len(), 1);
    assert_eq!(results.results[0].primary_value().unwrap(), item);
}

#[test]
fn test_integration_tag_tagging() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. タグ自体のItemを作成 (tag_itemの副作用を利用)
    let file_path = dir.path().join("dummy.txt");
    File::create(&file_path).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false)
        .unwrap();
    let registered_paths = fm.search("extension:txt", Default::default()).unwrap();
    let item = registered_paths.results[0].primary_value().unwrap();

    fm.tag_item(&item, "project:mars").unwrap();

    // 2. タグ (project:mars) 自体にタグ (priority:high) を付ける
    let tag_id = fm.get_or_create_item("typedtag", "project:mars").unwrap();
    fm.tag_item(&tag_id.to_string(), "priority:high").unwrap();

    // 3. 確認
    // 対象のタグ定義(typedtag)のみを検証するため、item_kindで絞り込む
    let results = fm.search("priority:high & item_kind:typedtag", Default::default()).unwrap();
    assert_eq!(results.results.len(), 1);
    assert_eq!(results.results[0].id, tag_id);
    assert_eq!(results.results[0].primary_value().unwrap(), "project:mars");

    // さらに、ファイル検索に影響しないことも確認
    let file_results = fm.search("project:mars", Default::default()).unwrap();
    assert!(file_results.results.iter().any(|r| r.item_kind == "file"));
}

#[test]
fn test_integration_note_tagging() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. Note作成
    let note_id = fm.add_item("note", "Meeting Memo").unwrap();

    // 2. Noteにタグ付与
    fm.tag_item(&note_id.to_string(), "category:meeting")
        .unwrap();

    // 3. 検索 (Noteがヒットすることを確認)
    // Note以外のアイテム（タグ定義など）を除外するため、item_kind:note で絞り込む
    let results = fm.search("category:meeting & item_kind:note", Default::default()).unwrap();
    assert_eq!(results.results.len(), 1);
    assert_eq!(results.results[0].id, note_id);
    assert_eq!(results.results[0].item_kind, "note");
    assert_eq!(results.results[0].primary_value().unwrap(), "Meeting Memo");
}

#[test]
fn test_system_item_metadata_integration() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. 拡張子ありとなしのファイルを準備
    File::create(dir.path().join("test.rs")).unwrap();
    File::create(dir.path().join("no_ext")).unwrap();

    fm.index_directory(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    // 2. 拡張子なしファイルによって 'extension:' タグが作られていないことを確認
    // type:extension 検索に 'extension:' という文字列が含まれないことをチェック
    let ext_list = fm.search("type:extension", Default::default()).unwrap();
    assert!(
        !ext_list.results.iter().any(|r| r.name == "extension:"),
        "Empty extension tag should not exist"
    );

    // 3. 'extension:rs' という typedtag Item（物理）は自動生成されないことを確認
    let results_physical = fm.search("item_kind:typedtag & label:rs", Default::default()).unwrap();
    assert!(
        results_physical.results.is_empty(),
        "Physical typedtag item should NOT be created automatically"
    );

    // 4. 代わりにプロジェクションで確認
    // extension: で検索し、test.rs がヒットすること、そのタグに rs が含まれていることを確認
    let results_proj = fm.search("extension:", Default::default()).unwrap();
    assert!(
        !results_proj.results.is_empty(),
        "Should find files via projection"
    );
    let test_file = results_proj
        .results
        .iter()
        .find(|r| r.name == "test.rs")
        .expect("test.rs not found");
    // SearchResult.tags に (extension, rs) が含まれているはず
    let ext_val = test_file.get_tag_value("extension");
    assert_eq!(
        ext_val.as_deref(),
        Some("rs"),
        "Projection should return extension value"
    );
}
