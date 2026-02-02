/// 型バリデーションの統合テスト
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_calculation_invalid_type_fail() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストファイル作成
    std::fs::write(root.join("test.txt"), b"test content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: (path: + 10) :> 100
    // path: は文字列型なので、+ 10（数値演算）は論理展開フェーズで失敗すべき
    let result = fm.search("(path: + 10) :> 100", Default::default());

    assert!(
        result.is_err(),
        "Non-numeric arithmetic should fail during logical resolution"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains(
            "Arithmetic operations are only possible for numeric types"
        ),
        "Error message should indicate invalid arithmetic: {}",
        err_msg
    );

    Ok(())
}

#[test]
fn test_calculation_literal_string_fail() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: ('str' + 10) :> 100
    // 文字列リテラルとの演算も失敗すべき
    let result = fm.search("('str' + 10) :> 100", Default::default());

    assert!(
        result.is_err(),
        "String literal arithmetic should fail during logical resolution"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains(
            "Arithmetic operations are only possible for numeric types"
        ),
        "Error message should indicate invalid arithmetic: {}",
        err_msg
    );

    Ok(())
}
