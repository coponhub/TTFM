/// 連鎖比較の統合テスト
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_chain_comparison_logic() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let files_dir = root.join("files");
    std::fs::create_dir(&files_dir)?;
    let db_dir = root.join(".ttfm/db");

    // テストファイル作成 (1KB, 50B, 200B)
    std::fs::write(files_dir.join("small.txt"), vec![0u8; 50])?;
    std::fs::write(files_dir.join("medium.txt"), vec![0u8; 200])?;
    std::fs::write(files_dir.join("large.txt"), vec![0u8; 1000])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&files_dir, None::<&fn(usize)>, false)?;

    // クエリ: 100 :< size: :<= 500 (汎用ラベル比較)
    // medium.txt (200B) のみがヒットすべき
    let result = fm.search("100 :< size: :<= 500", Default::default())?;

    let names: Vec<_> =
        result.results.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"medium.txt"), "Should contain medium.txt");
    assert!(
        !names.contains(&"small.txt"),
        "Should NOT contain small.txt"
    );
    assert!(
        !names.contains(&"large.txt"),
        "Should NOT contain large.txt"
    );

    // クエリ: 10 :<= size: :< 1001
    // 全てヒットすべき
    let result_all = fm.search("10 :<= size: :< 1001", Default::default())?;
    let all_names: Vec<_> =
        result_all.results.iter().map(|r| r.name.as_str()).collect();
    assert!(all_names.contains(&"small.txt"));
    assert!(all_names.contains(&"medium.txt"));
    assert!(all_names.contains(&"large.txt"));

    // 1. 逆方向の連鎖比較: 500 :>= size: :> 100
    // medium.txt (200B) のみがヒットすべき
    let result_rev = fm.search("500 :>= size: :> 100", Default::default())?;
    let rev_names: Vec<_> =
        result_rev.results.iter().map(|r| r.name.as_str()).collect();
    assert!(rev_names.contains(&"medium.txt"));
    assert!(!rev_names.contains(&"small.txt"));
    assert!(!rev_names.contains(&"large.txt"));

    // 2. 集約内でのネスト: sum((100 :< size: :<= 500) & size:)
    // medium.txt (200B) の合計なので 200.0 が返るべき
    let result_agg =
        fm.search("sum((100 :< size: :<= 500) & size:)", Default::default())?;
    assert_eq!(
        result_agg.scalar,
        Some(200.0),
        "Sum of medium file size should be 200"
    );

    Ok(())
}
