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

    // This parses successfully but fails logical validation
    let result = fm.search(
        "((parentdir: &: count()) / (extension: &: count())) :> 1",
        Default::default(),
    );

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
