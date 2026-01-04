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
    let registered_paths = fm.search("extension:txt & item_kind:file").unwrap();
    let item = registered_paths[0].primary_value().unwrap();
    
    fm.tag_item(item, "status:reviewed").unwrap();

    // 3. 付与したタグで検索
    let results = fm.search("status:reviewed & item_kind:file").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].primary_value().unwrap(), item);
}

#[test]
fn test_integration_tag_tagging() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. タグ自体のItemを作成 (tag_itemの副作用を利用)
    let file_path = dir.path().join("dummy.txt");
    File::create(&file_path).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();
    let registered_paths = fm.search("extension:txt & item_kind:file").unwrap();
    let item = registered_paths[0].primary_value().unwrap();
    
    fm.tag_item(item, "project:mars").unwrap();

    // 2. タグ (project:mars) 自体にタグ (priority:high) を付ける
    let tag_id = fm.get_or_create_item("typedtag", "project:mars").unwrap();
    fm.tag_item(&tag_id.to_string(), "priority:high").unwrap();

    // 3. 確認
    // 対象のタグ定義(typedtag)のみを検証するため、item_kindで絞り込み、かつ目的の名前を持つものを確認
    let results = fm.search("priority:high & item_kind:typedtag").unwrap();
    assert!(results.len() >= 1);
    assert!(results.iter().any(|r| r.id == tag_id && r.primary_value().unwrap() == "project:mars"));
    
    // さらに、ファイル検索に影響しないことも確認
    let file_results = fm.search("project:mars").unwrap();
    assert!(file_results.iter().any(|r| r.item_kind == "file"));
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
    // Note以外のアイテム（タグ定義など）を除外するため、item_kind:note で絞り込む
        let results = fm.search("category:meeting & item_kind:note").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, note_id);
        assert_eq!(results[0].item_kind, "note");
        assert_eq!(results[0].primary_value().unwrap(), "Meeting Memo");
    }
    
    #[test]
    fn test_type_search_functionality() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    
        // 1. ファイル作成とインデックス（これにより extension:txt 等の typedtag が生成される）
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();
            std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
                fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();
            
                // 1. 'type:type' の検索（型定義アイテムそのものがヒットするはず）
                let type_results = fm.search("type:type").unwrap();
                assert!(type_results.len() > 0, "Should find type definitions");
                assert!(
                    type_results.iter().any(|r| r.item_kind == "type" && r.primary_value().unwrap() == "extension"),
                    "Should find 'extension' type definition item"
                );
            
                // 2. 'type:extension' の検索（拡張子のリストがヒットするはず、型定義自体は含まれない）
                let ext_results = fm.search("type:extension").unwrap();
                assert!(ext_results.len() >= 2, "Should find extension:txt and extension:rs");
                assert!(
                    ext_results.iter().any(|r| r.item_kind == "typedtag" && r.primary_value().unwrap() == "extension:txt"),
                    "Should find 'extension:txt' typedtag"
                );
                // 型定義自体 (item_kind: type) が混じっていないことを確認
                assert!(
                    !ext_results.iter().any(|r| r.item_kind == "type"),
                    "Type definition should NOT be in the results for type:extension"
                );
            
                // 3. 'extension:txt' という検索で、ファイルだけでなく「タグの定義」もヒットするか確認
            
            // (これは type:[prefix] & [prefix]:[suffix] の両方が生成されることを期待)
            let txt_def_results = fm.search("extension:txt & item_kind:typedtag").unwrap();
            assert_eq!(txt_def_results.len(), 1, "Should find the definition of extension:txt");
        }
        