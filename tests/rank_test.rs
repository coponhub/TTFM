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

    // 2. クエリでランクを設定
    // high.txt を 100 に
    let res_high = fm.search("filename:high.txt").unwrap();
    fm.update_ranks(&res_high, 100).unwrap();
    
    // mid.txt を 50 に
    let res_mid = fm.search("filename:mid.txt").unwrap();
    fm.update_ranks(&res_mid, 50).unwrap();

    // 3. 検索して順序を確認
    let results = fm.search("extension:txt").unwrap();
    assert_eq!(results.len(), 3);

    // 順序: high (100) -> mid (50) -> low (0)
    assert!(results[0].primary_value().unwrap().contains("high.txt"));
    assert!(results[1].primary_value().unwrap().contains("mid.txt"));
    assert!(results[2].primary_value().unwrap().contains("low.txt"));
}

#[test]
fn test_rank_batch_update() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    
    fs::write(root.join("a.txt"), "a").unwrap();
    fs::write(root.join("b.txt"), "b").unwrap();
    fs::write(root.join("c.rs"), "c").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 1. *.txt のランクを一括で 10 に設定
    let results = fm.search("extension:txt").unwrap();
    assert_eq!(results.len(), 2);
    fm.update_ranks(&results, 10).unwrap();

    // 2. 結果を確認
    let res = fm.search("extension:txt | extension:rs").unwrap();
    // ランク順に a.txt(10), b.txt(10), c.rs(0) のはず
    assert_eq!(res.len(), 3);
    assert!(res[0].primary_value().unwrap().contains(".txt"));
    assert!(res[1].primary_value().unwrap().contains(".txt"));
    assert!(res[2].primary_value().unwrap().contains(".rs"));
}

#[test]
fn test_rank_set_by_id_low_level() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    
    fs::create_dir_all(&db_dir).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();

    let id = fm.add_item("note", "test note").unwrap();
    fm.set_rank_by_id(id, false, 500).unwrap();

    let results = fm.search("itemtype:note").unwrap();
    assert_eq!(results[0].id, id);
    // ランクに基づいたソートが効いているか（他にアイテムがあればより明確）
}