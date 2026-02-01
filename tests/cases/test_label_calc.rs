use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_arithmetic_projection_syntax() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. Simple arithmetic projection
    // This previously caused a syntax error
    let query = "(size: / 1024)";
    let result = fm.search(query, Default::default());

    match &result {
        Ok(res) => {
            println!("Success! Parsed arithmetic projection.");
            // Optional: verify internal structure if needed, but parsing success is the main goal here.
            // Check if type_for_projection is correctly set or derived
            println!("Projections: {:?}", res.type_for_projection);
        }
        Err(e) => {
            panic!("Failed to parse arithmetic projection: {}", e);
        }
    }

    // 2. Complex query with set operation
    let query2 = "extension:rs & (size: * 2)";
    let result2 = fm.search(query2, Default::default());
    match &result2 {
        Ok(_) => println!("Success! Parsed complex arithmetic projection."),
        Err(e) => panic!("Failed to parse complex query: {}", e),
    }
}

#[test]
fn test_arithmetic_comparison_with_units() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // Create a dummy file to ensure DB has data
    std::fs::write(root.join("test.txt"), "some content").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // Index the directory
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 3. Arithmetic comparison with units
    // (size: / 2) > 100MB
    // This currently fails with "Could not convert string '100MB' to DOUBLE"
    let query = "(size: / 2) > 100MB";
    let result = fm.search(query, Default::default());

    match &result {
        Ok(_) => {
            println!("Success! Unit-aware arithmetic comparison worked.");
        }
        Err(e) => {
            panic!("Failed to parse arithmetic comparison: {}", e);
        }
    }

    // 4. Reverse pattern: Literal < Calculation
    // 100MB < (size: / 2)
    let query_rev = "100MB < (size: / 2)";
    let result_rev = fm.search(query_rev, Default::default());
    match &result_rev {
        Ok(_) => println!("Success! Reverse pattern worked."),
        Err(e) => panic!("Failed to parse reverse pattern: {}", e),
    }
}

#[test]
fn test_complex_comparisons() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // Index something empty to avoid index check failures if any
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 5. Agg vs Agg (Scalar Comparison)
    let query_agg_agg = "sum(size:) > count(extension:rs)";
    assert!(
        fm.search(query_agg_agg, Default::default()).is_ok(),
        "Agg vs Agg should be valid"
    );

    // 6. Agg Calculation vs Literal (Scalar Comparison)
    let query_agg_calc = "(sum(size:) / 1024) > 100";
    assert!(
        fm.search(query_agg_calc, Default::default()).is_ok(),
        "Agg calculation vs Literal should be valid"
    );

    // 7. Proj vs Calculation (General Label Comparison - MUST have colon)
    let query_proj_calc = "size: :> (1024 * 1024)";
    assert!(
        fm.search(query_proj_calc, Default::default()).is_ok(),
        "Proj vs Calculation with label op should be valid"
    );

    // 8. FORBIDDEN: Proj vs Literal (Scalar Comparison - NO colon)
    // DESIGN: size: > 100 is Syntax Error
    let query_forbidden = "size: > 100";
    let res_forbidden = fm.search(query_forbidden, Default::default());
    assert!(
        res_forbidden.is_err(),
        "size: > 100 should be a syntax error according to design"
    );

    // 9. FORBIDDEN: Agg vs Proj (Scalar Comparison - NO colon)
    // DESIGN: max(size:) == size: is likely Syntax Error if scalar op is restricted
    let query_agg_proj = "max(size:) == size:";
    let res_agg_proj = fm.search(query_agg_proj, Default::default());
    assert!(res_agg_proj.is_err(), "max(size:) == size: should be a syntax error if both sides must be scalar");
}
