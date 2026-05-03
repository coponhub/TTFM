use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_literal_arithmetic() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let data_dir = root.join("data");
    std::fs::create_dir(&data_dir)?;
    // Create a dummy file so we have items to match against
    std::fs::write(data_dir.join("test.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&data_dir, None::<&fn(usize)>, false)?;

    // 1. Tag + Literal Arithmetic
    // size: + 1
    // The file size is 7 bytes ("content"). 7 + 1 = 8.
    // Grammar requires parens for top-level calculation: "(size: + 1)"
    let res_tag = fm.search("(size: + 1)", Default::default())?;
    assert!(!res_tag.results.is_empty(), "size: + 1 should match");
    // Result name should be the calculated value
    assert_eq!(res_tag.results[0].name, "8", "size: + 1 should be 8");

    // 2. Pure Literal Arithmetic
    // (1 + 2) - Parentheses are likely required for pure calculation to distinguish from other patterns?
    // Or maybe just "1 + 2" should work. The parser error suggests it expects structure.
    // Try "(1 + 2)" as per my implementation plan note.
    let res = fm.search("(1 + 2)", Default::default())?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "3");

    // 3. String Arithmetic
    // ('a' + 'b') -> "a, b"
    let res = fm.search("('a' + 'b')", Default::default())?;
    assert_eq!(res.results[0].name, "a, b");

    // ('a' * 'b') -> "ab"
    let res = fm.search("('a' * 'b')", Default::default())?;
    assert_eq!(res.results[0].name, "ab");

    Ok(())
}

#[test]
fn test_literal_comparison() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;

    // 1. Integer Comparison (True)
    let res = fm.search("10 > 2", Default::default())?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "TRUE");

    // 2. Integer Comparison (False)
    let res = fm.search("1 > 2", Default::default())?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "FALSE");

    // 3. String Comparison
    let res = fm.search("'b' > 'a'", Default::default())?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "TRUE");

    Ok(())
}

#[test]
fn test_literal_set_operation_error() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;

    // リテラル同士の集合演算はパーサーレベルで拒否される
    let res = fm.search("1 & 2", Default::default());
    assert!(
        res.is_err(),
        "Set operation between literals should be an error"
    );

    Ok(())
}

#[test]
fn test_literal_string_error_cases() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = ttfm::FileManager::new_with_db_dir(&db_dir)?;

    let cases = vec![
        // String同士の無効な算術演算
        ("('a' - 'b')", "Unsupported arithmetic"),
        ("('a' / 'b')", "Unsupported arithmetic"),
        // StringとString以外の型の算術演算
        ("('a' + 1)", "String and non-String"),
        ("(1 + 'a')", "String and non-String"),
    ];

    for (query, expected_err_part) in cases {
        let result = fm.search(query, Default::default());
        assert!(result.is_err(), "Query '{}' should fail", query);
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains(expected_err_part),
            "Query '{}' error message '{}' should contain '{}'",
            query,
            err_msg,
            expected_err_part
        );
    }
    Ok(())
}
