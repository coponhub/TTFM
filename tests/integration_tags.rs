use ttfm::FileManager;
use tempfile::tempdir;
use std::fs::File;

#[test]
fn test_integration_file_tagging() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. ファイル作成とインデックス
    let file_path = dir.path().join("doc.txt");
    File::create(&file_path).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();

    // 2. タグ付与
    let _path_str = file_path.to_string_lossy();
    // 実際には相対パスで登録されているかもしれないので、searchで取得したパスを使うのが確実
    let registered_paths = fm.search("extension:txt").unwrap();
    let target = registered_paths[0].primary_value().unwrap();
    
    fm.tag_item(target, "status:reviewed").unwrap();

    // 3. 付与したタグで検索
    let results = fm.search("status:reviewed").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].primary_value().unwrap(), target);
}

#[test]
fn test_integration_tag_tagging() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. タグ自体のItemを作成 (tag_itemの副作用を利用)
    // ファイルに適当なタグを付ける
    let file_path = dir.path().join("dummy.txt");
    File::create(&file_path).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();
    let registered_paths = fm.search("extension:txt").unwrap();
    let target = registered_paths[0].primary_value().unwrap();
    
    fm.tag_item(target, "project:mars").unwrap();

    // 2. タグ (project:mars) 自体にタグ (priority:high) を付ける
    // get_or_create_item を使ってIDを取得
    let tag_id = fm.get_or_create_item("typedtag", "project:mars").unwrap();
    
    // そのIDに対してタグを付ける
    fm.tag_item(&tag_id.to_string(), "priority:high").unwrap();

    // 3. 確認
    // "priority:high" で検索して、"project:mars" がヒットすることを確認
    let results = fm.search("priority:high").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, tag_id);
    assert_eq!(results[0].primary_value().unwrap(), "project:mars");
    
    // さらに、ファイル検索に影響しないことも確認
    let file_results = fm.search("project:mars").unwrap();
    assert_eq!(file_results.len(), 1); // 元のファイルはヒットする
}

#[test]
fn test_integration_note_tagging() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. Note作成
    let note_id = fm.add_item("note", "Meeting Memo").unwrap();

    // 2. Noteにタグ付与
    fm.tag_item(&note_id.to_string(), "category:meeting").unwrap();

    // 3. 検索 (Noteがヒットすることを確認)
    let results = fm.search("category:meeting").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, note_id);
    assert_eq!(results[0].kind, "item");
    assert_eq!(results[0].primary_value().unwrap(), "Meeting Memo");
}
