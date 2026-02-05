use anyhow::Result;
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_reverse_pattern_scalar_gt_projection() -> Result<()> {
    // Case: "100 > size:"
    // Intent: Label Comparison (100 :> size:)
    // Actual: Scalar Comparison syntax with Projection target (which is invalid in strict grammar)

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let res = fm.search("100 > size:", Default::default());
    assert!(res.is_err());

    let err_msg = res.unwrap_err().to_string();
    println!("Actual error: {}", err_msg);

    // We expect a friendly message suggesting ":>"
    // "Invalid operator '>': ... Did you mean '100 :> size:'?"
    assert!(
        err_msg.contains("Did you mean"),
        "Expected suggestion, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains(":>"),
        "Expected suggestion to contain ':>', got: {}",
        err_msg
    );
    assert!(err_msg.contains("-->"), "Expected pretty printing");

    Ok(())
}

#[test]
fn test_reverse_pattern_aggregation_label_op() -> Result<()> {
    // Case: "sum(size:) :> 100"
    // Intent: Scalar Comparison (sum(size:) > 100)
    // Actual: Label Op used with Aggregation

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let res = fm.search("sum(size:) :> 100", Default::default());
    assert!(res.is_err());

    let err_msg = res.unwrap_err().to_string();
    println!("Actual error: {}", err_msg);

    // We expect a friendly message suggesting ">"
    // "Invalid operator ':>': ... Did you mean 'sum(size:) > 100'?"
    assert!(
        err_msg.contains("Did you mean"),
        "Expected suggestion, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("> 100"),
        "Expected suggestion to contain '> 100', got: {}",
        err_msg
    );
    assert!(err_msg.contains("-->"), "Expected pretty printing");

    Ok(())
}

#[test]
fn test_unified_error_scalar_label_op() -> Result<()> {
    // Case 1: "1 :> 100" (Scalar :Op Scalar) -> Invalid
    // Intent: Error message regarding misused Label Op on Scalar

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let res = fm.search("1 :> 100", Default::default());
    assert!(res.is_err());

    let err_msg = res.unwrap_err().to_string();
    println!("Actual error for '1 :> 100': {}", err_msg);

    assert!(
        err_msg.contains("Label Comparison cannot be applied to Scalar/Value"),
        "Expected unified error message, got: {}",
        err_msg
    );
    assert!(err_msg.contains("-->"), "Expected pretty printing");

    // Case 2: "100 :< size:" (Scalar :Op Proj) -> Valid (per QUERY.md Spec)
    // Intent: Verify that valid Scalar :Op Proj is Still supported.
    let res = fm.search("100 :< size:", Default::default());
    assert!(
        res.is_ok(),
        "Valid query '100 :< size:' should be allowed. Error: {:?}",
        res.err()
    );

    Ok(())
}

#[test]
fn test_projection_to_projection_comparison() -> Result<()> {
    // Phase 1 TDD: These are expected to FAIL initially.
    // Case: "width: :> height:" or "width:>height:"

    let dir = tempdir()?;
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    // 1. Spaced: width: :> height:
    let res_spaced = fm.search("width: :> height:", Default::default());
    assert!(
        res_spaced.is_ok(),
        "Spaced Proj-Proj comparison 'width: :> height:' should be allowed. Got: {:?}",
        res_spaced.err()
    );

    // 2. Stuck: width:>height:
    let res_stuck = fm.search("width:>height:", Default::default());
    assert!(
        res_stuck.is_ok(),
        "Stuck Proj-Proj comparison 'width:>height:' should be allowed. Got: {:?}",
        res_stuck.err()
    );

    Ok(())
}

#[test]
fn test_double_colon_suggestion_fix() -> Result<()> {
    // Phase 1 TDD: This is expected to FAIL initially (it will suggest 'size: : :>').
    // Case: "size: > path:" (Error at '>')

    let dir = tempdir()?;
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let res = fm.search("size: > path:", Default::default());
    assert!(res.is_err());

    let err_msg = res.unwrap_err().to_string();
    println!("Actual error for 'size: > path:': {}", err_msg);

    // Currently it suggests: "Did you mean: 'size: : :> path:'"
    // We want it to suggest: "Did you mean: 'size: :> path:'"
    assert!(
        !err_msg.contains("size: : :>"),
        "Should not suggest double colon like 'size: : :>'"
    );
    assert!(
        err_msg.contains("size: :> path:"),
        "Should suggest correct syntax 'size: :> path:'"
    );

    Ok(())
}

#[test]
fn test_projection_calculation_reverse_consistency() -> Result<()> {
    // Reported case: "size: :> (size: - 1M)"

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    // Create 10MB file
    std::fs::write(root.join("test.bin"), vec![0u8; 10 * 1024 * 1024])?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 1. "size: :> (size: - 1M)" -> Should be TRUE (size > size - 1MB)
    let res1 = fm.search("size: :> (size: - 1M)", Default::default())?;
    assert!(
        !res1.results.is_empty(),
        "Query 'size: :> (size: - 1M)' should NOT be empty"
    );

    // 2. "(size: - 1M) :< size:" -> Should also be TRUE (size - 1MB < size)
    let res2 = fm.search("(size: - 1M) :< size:", Default::default())?;
    assert!(
        !res2.results.is_empty(),
        "Query '(size: - 1M) :< size:' should NOT be empty"
    );

    Ok(())
}

#[test]
fn test_complex_tag_calculation_comparison_eav() -> Result<()> {
    // Test case for cross-tag calculation: width: :> (height: * 2)
    // This requires ANY_VALUE grouping for RowTag (EAV).

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    // Create two files
    let wide_file = root.join("wide_image.jpg"); // width > height * 2
    let tall_file = root.join("tall_image.jpg"); // width < height * 2
    std::fs::write(&wide_file, "")?;
    std::fs::write(&tall_file, "")?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // Find item_ids
    let res_wide = fm.search("name:wide_image.jpg", Default::default())?;
    let res_tall = fm.search("name:tall_image.jpg", Default::default())?;
    let wide_id = res_wide.results[0].id.to_string();
    let tall_id = res_tall.results[0].id.to_string();

    // wide_image: width=1000, height=400 (1000 > 400*2=800) -> TRUE
    fm.tag_item(&wide_id, "width:1000")?;
    fm.tag_item(&wide_id, "height:400")?;

    // tall_image: width=1000, height=600 (1000 < 600*2=1200) -> FALSE
    fm.tag_item(&tall_id, "width:1000")?;
    fm.tag_item(&tall_id, "height:600")?;

    println!("Executing search: width: :> (height: * 2)");
    let res = fm.search("width: :> (height: * 2)", Default::default())?;

    // Only wide_image should match
    assert_eq!(res.results.len(), 1, "Should find exactly 1 item");
    assert!(
        res.results[0].name.contains("wide"),
        "Should find wide_image, got: {}",
        res.results[0].name
    );

    Ok(())
}
