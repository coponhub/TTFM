use anyhow::Result;
use tempfile::tempdir;
use ttfm::FileManager;

// ──────────────────────────────────────────────
// define_cases! 移行済みケース
// ──────────────────────────────────────────────

define_cases! {
    proj_proj_comparison_spaced: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: super::default_scope,
        query: "width: :> height:",
        assert: |_res, _dir| Ok(()),
    },
    proj_proj_comparison_stuck: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: super::default_scope,
        query: "width:>height:",
        assert: |_res, _dir| Ok(()),
    },
    calc_reverse_consistency_gt: {
        setup: |dir| {
            std::fs::write(dir.join("test.bin"), vec![0u8; 10 * 1024 * 1024])?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: "size: :> (size: - 1M)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Query 'size: :> (size: - 1M)' should NOT be empty");
            Ok(())
        },
    },
    calc_reverse_consistency_lt: {
        setup: |dir| {
            std::fs::write(dir.join("test.bin"), vec![0u8; 10 * 1024 * 1024])?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: "(size: - 1M) :< size:",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Query '(size: - 1M) :< size:' should NOT be empty");
            Ok(())
        },
    },
    complex_tag_calc_comparison: {
        setup: |dir| {
            std::fs::write(dir.join("wide_image.jpg"), "")?;
            std::fs::write(dir.join("tall_image.jpg"), "")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            let wide_path = format!("name:wide_image.jpg & path:{}/*", dir.to_string_lossy());
            let res_wide = fm.search(&wide_path, Default::default())?;
            anyhow::ensure!(!res_wide.results.is_empty(), "wide_image.jpg not found");
            let wide_id = res_wide.results[0].id.to_string();

            let tall_path = format!("name:tall_image.jpg & path:{}/*", dir.to_string_lossy());
            let res_tall = fm.search(&tall_path, Default::default())?;
            anyhow::ensure!(!res_tall.results.is_empty(), "tall_image.jpg not found");
            let tall_id = res_tall.results[0].id.to_string();

            fm.tag_item(&wide_id, "width:1000")?;
            fm.tag_item(&wide_id, "height:400")?;
            fm.tag_item(&tall_id, "width:1000")?;
            fm.tag_item(&tall_id, "height:600")?;
            Ok(())
        }),
        format_query: super::default_scope,
        query: "width: :> (height: * 2)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1, "Should find exactly 1 item");
            assert!(res.results[0].name.contains("wide"), "Should find wide_image, got: {}", res.results[0].name);
            Ok(())
        },
    },
}

// ──────────────────────────────────────────────
// エラー系テスト (define_cases! 移行不可)
// ──────────────────────────────────────────────

#[test]
fn test_reverse_pattern_scalar_gt_projection() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let res = fm.search("100 > size:", Default::default());
    assert!(res.is_err());

    let err_msg = res.unwrap_err().to_string();
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
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let res = fm.search("sum(size:) :> 100", Default::default());
    assert!(res.is_err());

    let err_msg = res.unwrap_err().to_string();
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
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let res = fm.search("1 :> 100", Default::default());
    assert!(res.is_err());

    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("Label Comparison cannot be applied to Scalar/Value"),
        "Expected unified error message, got: {}",
        err_msg
    );
    assert!(err_msg.contains("-->"), "Expected pretty printing");

    let res = fm.search("100 :< size:", Default::default());
    assert!(
        res.is_ok(),
        "Valid query '100 :< size:' should be allowed. Error: {:?}",
        res.err()
    );

    Ok(())
}

#[test]
fn test_double_colon_suggestion_fix() -> Result<()> {
    let dir = tempdir()?;
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let res = fm.search("size: > path:", Default::default());
    assert!(res.is_err());

    let err_msg = res.unwrap_err().to_string();
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
