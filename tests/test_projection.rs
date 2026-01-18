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
    let results = fm.search("extension:").unwrap();
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
    assert_eq!(results.projections, vec!["extension"]);

    // 2. directory: (投影 -> is_dir:true)
    let results = fm.search("directory:").unwrap();
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

    // 3. filename: (投影 -> is_dir:false)
    let results = fm.search("filename:").unwrap();
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

    // 4. origin:system
    // 全てのアイテムは system 由来のタグを持つはず（初期状態）
    let results = fm.search("origin:system").unwrap();
    assert!(results.results.len() >= 3);

    // 5. 複合クエリ
    let results = fm.search("extension: & directory:").unwrap();
    assert_eq!(
        results.results.len(),
        0,
        "No directories should have an extension in this test"
    );
}
