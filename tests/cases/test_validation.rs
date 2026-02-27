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
        err_msg.contains("not allowed"),
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
        err_msg.contains("not allowed"),
        "Error message should indicate invalid arithmetic: {}",
        err_msg
    );

    Ok(())
}

// ========== 集合演算スカラーオペランド検証テスト ==========

#[test]
fn test_set_operation_with_aggregation_left_fail() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: count(path:) & type:file
    // 左オペランドが集約関数（スカラー値）
    let result = fm.search("count(path:) & type:file", Default::default());

    assert!(
        result.is_err(),
        "Set operation with scalar aggregation on left should fail"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains(
            "Set operations between sets and scalars are not implemented"
        ),
        "Error message should indicate invalid set operation: {}",
        err_msg
    );
    assert!(
        err_msg.contains("Did you mean?"),
        "Error message should include suggestion: {}",
        err_msg
    );

    Ok(())
}

#[test]
fn test_set_operation_with_aggregation_right_fail() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: type:file & sum(size:)
    // 右オペランドが集約関数（スカラー値）
    let result = fm.search("type:file & sum(size:)", Default::default());

    assert!(
        result.is_err(),
        "Set operation with scalar aggregation on right should fail"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains(
            "Set operations between sets and scalars are not implemented"
        ),
        "Error message should indicate invalid set operation: {}",
        err_msg
    );
    assert!(
        err_msg.contains("Did you mean?"),
        "Error message should include suggestion: {}",
        err_msg
    );

    Ok(())
}

#[test]
fn test_set_operation_with_scalar_comparison_fail() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: (1 > 0) & type:file
    // 左オペランドがスカラー比較（真偽値）
    let result = fm.search("(1 > 0) & type:file", Default::default());

    assert!(
        result.is_err(),
        "Set operation with scalar comparison should fail"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains(
            "Set operations between sets and scalars are not implemented"
        ),
        "Error message should indicate invalid set operation: {}",
        err_msg
    );
    // スカラー比較の場合は提案が含まれないことを確認
    assert!(
        !err_msg.contains("Did you mean?"),
        "Error message should not include suggestion for scalar comparison: {}",
        err_msg
    );

    Ok(())
}

#[test]
fn test_set_operation_difference_with_scalar_fail() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: type:file - sum(size:)
    // 右オペランドが集約関数（スカラー値）
    let result = fm.search("type:file - sum(size:)", Default::default());

    assert!(
        result.is_err(),
        "Difference operation with scalar aggregation should fail"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains(
            "Set operations between sets and scalars are not implemented"
        ),
        "Error message should indicate invalid set operation: {}",
        err_msg
    );

    Ok(())
}

#[test]
fn test_valid_set_operations_still_work() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テスト用のファイルを作成
    std::fs::create_dir_all(root.join("test_dir"))?;
    std::fs::write(root.join("test_file.txt"), "test content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 正常な集合演算: type:file & path:
    let result = fm.search("type:file & path:", Default::default());
    assert!(
        result.is_ok(),
        "Valid set operation (type & projection) should succeed"
    );

    // 正常な集合演算: ラベル比較（集合） & 集合
    // size: :> 0 はラベル比較として集合を返す
    let result = fm.search("(size: :> 0) & type:file", Default::default());
    assert!(
        result.is_ok(),
        "Valid set operation (label comparison & type) should succeed"
    );

    // 正常な集合演算: type:file | type:directory
    let result = fm.search("type:file | type:directory", Default::default());
    assert!(
        result.is_ok(),
        "Valid set operation (type | type) should succeed"
    );

    Ok(())
}

#[test]
fn test_set_operation_with_both_scalars_fail() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: sum(size:) & count(path:)
    // 両方のオペランドがスカラー値
    let result = fm.search("sum(size:) & count(path:)", Default::default());

    assert!(
        result.is_err(),
        "Set operation with both scalars should fail"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Set operations between scalars are not implemented"),
        "Error message should indicate scalar-to-scalar set operation: {}",
        err_msg
    );
    assert!(
        !err_msg.contains("Did you mean?"),
        "Error message should not include suggestion for scalar-to-scalar: {}",
        err_msg
    );

    Ok(())
}

#[test]
fn test_aggregator_empty_args_errors() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(), avg(), max(), min() は引数が必要
    let queries = vec!["sum()", "avg()", "max()", "min()"];

    for q in queries {
        let result = fm.search(q, Default::default());
        assert!(
            result.is_err(),
            "Aggregator '{}' without arguments should fail",
            q
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("requires an argument"),
            "Error message should indicate missing argument: {}",
            err_msg
        );
    }

    Ok(())
}

#[test]
fn test_parse_nest_in_comparison_left() {
    let query = "count(parentdir: &: count(extension:rs) :> 5)";
    let result = ttfm::parse(query);
    assert!(
        result.is_ok(),
        "Nested label comparison inside aggregation should parse successfully: {:?}",
        result.err()
    );

    let node = result.unwrap();
    if let ttfm::QueryNode::Aggregation(ttfm::query::AggregationNode::Count(
        inner,
    )) = node
    {
        if let ttfm::QueryNode::Comparison(cmp) = *inner {
            if let ttfm::query::Operand::Query(inner_query) = cmp.first {
                assert!(
                    matches!(*inner_query, ttfm::QueryNode::Nest(_)),
                    "Comparison left side should be a Nest node"
                );
            } else {
                panic!("Expected Query(Nest) as comparison first operand, got: {:?}", cmp.first);
            }
        } else {
            panic!("Expected Comparison inside count, got {:?}", inner);
        }
    } else {
        panic!("Expected Count Aggregation, got {:?}", node);
    }
}
