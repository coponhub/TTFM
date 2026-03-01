use anyhow::Result;
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_mismatched_comparison_error_message() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let files_dir = root.join("files");
    std::fs::create_dir(&files_dir)?;

    std::fs::write(files_dir.join("test.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&files_dir, None::<&fn(usize)>, false)?;

    // size: > 100 という形式（本来は :> であるべき）を実行
    let result = fm.search("size: > 100", Default::default());

    assert!(
        result.is_err(),
        "Should fail for mismatched comparison operator"
    );

    let err_msg = format!("{}", result.unwrap_err());

    // 期待される詳細なエラーメッセージが含まれているか確認
    // 注: 現時点では文法エラーなどで失敗するため、このアサーションは失敗する想定 (Red)
    assert!(
        err_msg.contains("Invalid operator '>'"),
        "Error message should point out the invalid operator"
    );
    assert!(
        err_msg.contains("Scalar comparison cannot be applied to a Projection"),
        "Error message should explain why it's invalid"
    );
    assert!(
        err_msg.contains("Did you mean: 'size: :> 100'"),
        "Error message should suggest a correct alternative"
    );

    Ok(())
}

#[test]
fn test_repro_mismatched_group_by_keys_error_msg() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let files_dir = root.join("files");
    std::fs::create_dir(&files_dir)?;

    std::fs::write(files_dir.join("test.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&files_dir, None::<&fn(usize)>, false)?;

    // --- Investigation ---
    // 1.1 正常系: プレーンなプロジェクション同士の演算（size: + mtime:）
    let ok_query1 = "(size: + mtime:) :> 10";
    let ok_result1 = fm.search(ok_query1, Default::default());
    if let Err(e) = &ok_result1 {
        println!("QUERY 1 ERROR: {:?}", e);
    }
    assert!(ok_result1.is_ok(), "Simple projection arithmetic should succeed (Top level)");

    // 1.2 異常系: Nest 内での非集約タグ算術（Level 2 Nest は未実装）
    let err_query2 = "(parentdir: &: (size: + mtime:)) :> 10";
    let err_result2 = fm.search(err_query2, Default::default());
    assert!(
        err_result2.is_err(),
        "Arithmetic over non-aggregated tags within Nest should fail (not yet implemented)"
    );
    let err_msg2 = format!("{}", err_result2.unwrap_err());
    assert!(
        err_msg2.contains("not yet implemented"),
        "Error should mention 'not yet implemented', got: {}",
        err_msg2
    );

    // 2. 異常系: 異なる集計キーを持つクエリ
    let query = "((parentdir: &: count()) / (extension: &: count())) :> 1";
    let result = fm.search(query, Default::default());

    assert!(result.is_err(), "Should fail for mismatched group by keys");

    let err_msg = format!("{}", result.unwrap_err());
    println!("Err msg: {}", err_msg);

    assert!(
        err_msg.contains(
            "Arithmetic operations between different Group By target keys"
        ),
        "Error message should explain the mismatch"
    );
    assert!(
        err_msg.contains("parentdir"),
        "Error message should mention parentdir"
    );
    assert!(
        err_msg.contains("extension"),
        "Error message should mention extension"
    );

    Ok(())
}
