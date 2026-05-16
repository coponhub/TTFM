use ttfm::search;
use std::fs::File;
use std::os::unix::fs::symlink;
use tempfile::tempdir;

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
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables().unwrap();
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry).run(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    // 3. エラー値がセットされたアイテムを検索して検証
    // 数値型のエラー値 (-1) で検索
    let results = search::search(&store, &registry, &cache, "size:-1", Default::default())
        .expect("Search for size:-1 should succeed");

    // 検証: loop_link がエラー値で登録されてヒットするはず
    assert_eq!(
        results.results.len(),
        1,
        "Should find exactly one file with metadata error"
    );
    assert!(results.results[0]
        .primary_value()
        .unwrap()
        .contains("loop_link"));
}
