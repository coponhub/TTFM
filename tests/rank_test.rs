use ttfm::FileManager;
use tempfile::tempdir;
use std::fs;

#[test]
fn test_rank_sorting_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 1. ファイルを3つ作成
    fs::write(root.join("low.txt"), "low").unwrap();
    fs::write(root.join("high.txt"), "high").unwrap();
    fs::write(root.join("mid.txt"), "mid").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 2. ランクを設定
    // デフォルトは 0。high.txt を 100, mid.txt を 50 に設定
    fm.set_rank(&root.join("high.txt").to_string_lossy(), 100).unwrap();
    fm.set_rank(&root.join("mid.txt").to_string_lossy(), 50).unwrap();

    // 3. 検索して順序を確認
    let results = fm.search("extension:txt").unwrap();
    assert_eq!(results.len(), 3);

    // 順序: high (100) -> mid (50) -> low (0)
    assert!(results[0].primary_value().unwrap().contains("high.txt"));
    assert!(results[1].primary_value().unwrap().contains("mid.txt"));
    assert!(results[2].primary_value().unwrap().contains("low.txt"));
}

#[test]
fn test_rank_sorting_items() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    
    // インデックスを作成しないと entities が存在しないので作成
    fs::create_dir_all(&db_dir).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();

    // 1. ノートを2つ作成
    let id_low = fm.add_item("note", "low priority note").unwrap();
    let id_high = fm.add_item("note", "high priority note").unwrap();

    // 2. ランク設定
    fm.set_rank(&id_high.to_string(), 10).unwrap();
    fm.set_rank(&id_low.to_string(), 1).unwrap();

    // 3. 検索
    let results = fm.search("itemtype:note").unwrap();
    assert_eq!(results.len(), 2);

    // 順序: high (10) -> low (1)
    assert_eq!(results[0].id, id_high);
    assert_eq!(results[1].id, id_low);
}
