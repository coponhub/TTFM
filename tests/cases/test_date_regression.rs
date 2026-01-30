use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_slash_separated_date_query() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 1. テストファイル作成
    let file_path = root.join("date_test.txt");
    std::fs::write(&file_path, "date test")?;

    // 2. 過去の日付（2024/01/01）に設定
    let target_date = "2024/01/01";
    let status = std::process::Command::new("touch")
        .args(["-d", "2024-01-01 12:00:00", file_path.to_str().unwrap()])
        .status()?;
    assert!(status.success());

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 3. 引用符なしのスラッシュ区切り日付で検索
    // mtime:2024/01/01
    println!("Testing unquoted slash date: mtime:{}", target_date);
    let query = format!("mtime:{}", target_date);
    let res = fm.search(&query, Default::default())?;

    assert!(
        !res.results.is_empty(),
        "Should match file with unquoted slash date"
    );
    assert_eq!(res.results[0].name, "date_test.txt");

    // 4. ハイフン区切りでも確認
    let query_hyphen = "mtime:2024-01-01";
    let res_hyphen = fm.search(query_hyphen, Default::default())?;
    assert!(
        !res_hyphen.results.is_empty(),
        "Should match file with hyphen date"
    );

    // 5. 日時の集約 (max, min)
    // max(mtime:) は最新のファイルのタイムスタンプを返すはず
    let res_max = fm.search("max(mtime:)", Default::default())?;
    assert!(res_max.scalar.unwrap() > 0.0);

    let res_min = fm.search("min(mtime:)", Default::default())?;
    assert!(res_min.scalar.unwrap() > 0.0);
    assert!(res_max.scalar.unwrap() >= res_min.scalar.unwrap());

    Ok(())
}
