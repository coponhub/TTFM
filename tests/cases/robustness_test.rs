use std::fs::File;
use std::os::unix::fs::symlink;
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
#[cfg(unix)]
fn test_metadata_error_recovery_integration() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("db");

    // 1. 正常なファイルと、エラーになるリンクを作成
    let normal_file = dir.path().join("normal.txt");
    File::create(&normal_file).unwrap();

    let loop_link = dir.path().join("loop_link");
    // 自分自身を指すループリンク (ELOOPエラーを誘発)
    symlink(&loop_link, &loop_link).expect("Failed to create loop link");

    // 2. インデックス作成
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    // 3. エラー値がセットされたアイテムを検索して検証
    // 数値型のエラー値 (-1) で検索
    let results = fm
        .search("size:-1")
        .expect("Search for size:-1 should succeed");

    // 検証: loop_link がエラー値で登録されてヒットするはず
    assert_eq!(
        results.results.len(),
        1,
        "Should find exactly one file with metadata error"
    );
    assert!(results.results[0].primary_value().unwrap().contains("loop_link"));

    // 文字列表現のエラー値 ("-") も検証
    let results_str = fm
        .search("size_str:-")
        .expect("Search for size_str:- should succeed");

    // type_from_ext:Folder も "-" になるため、名前でフィルタリングして確認
    let found_loop = results_str.results.iter().any(|r| r.name.contains("loop_link"));
    assert!(found_loop, "Loop link should have '-' as size_str");
}
