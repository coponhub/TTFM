use anyhow::Result;
use super::default_scope;
use tempfile::tempdir;
use ttfm::FileManager;

define_cases! {
    label_calc_arith_projection: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: default_scope,
        query: "(size: / 1024)",
        assert: |_res, _dir| Ok(()),
    },
    label_calc_arith_complex: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: default_scope,
        query: "extension:rs & (size: * 2)",
        assert: |_res, _dir| Ok(()),
    },
    label_calc_arith_units: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), "some content")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(size: / 2) :> 100MB",
        assert: |_res, _dir| Ok(()),
    },
    label_calc_arith_units_reverse: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), "some content")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "100MB :< (size: / 2)",
        assert: |_res, _dir| Ok(()),
    },
}

/// 複合比較クエリ (成功/エラー混在のため移行不可)
#[test]
fn test_complex_comparisons() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    let query_agg_agg = "sum(size:) > count(extension:rs)";
    assert!(
        fm.search(query_agg_agg, Default::default()).is_ok(),
        "Agg vs Agg should be valid"
    );

    let query_agg_calc = "(sum(size:) / 1024) > 100";
    assert!(
        fm.search(query_agg_calc, Default::default()).is_ok(),
        "Agg calculation vs Literal should be valid"
    );

    let query_proj_calc = "size: :> (1024 * 1024)";
    assert!(
        fm.search(query_proj_calc, Default::default()).is_ok(),
        "Proj vs Calculation with label op should be valid"
    );

    let query_forbidden = "size: > 100";
    let res_forbidden = fm.search(query_forbidden, Default::default());
    assert!(
        res_forbidden.is_err(),
        "size: > 100 should be a syntax error according to design"
    );

    let query_agg_proj = "max(size:) == size:";
    let res_agg_proj = fm.search(query_agg_proj, Default::default());
    assert!(res_agg_proj.is_err(), "max(size:) == size: should be a syntax error if both sides must be scalar");
}

/// 算術射影の構文確認 (複数クエリのため部分的に残留)
#[test]
fn test_arithmetic_projection_syntax() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    let result = fm.search("(size: / 1024)", Default::default());
    assert!(
        result.is_ok(),
        "Failed to parse arithmetic projection: {:?}",
        result.err()
    );

    let result2 = fm.search("extension:rs & (size: * 2)", Default::default());
    assert!(
        result2.is_ok(),
        "Failed to parse complex query: {:?}",
        result2.err()
    );

    Ok(())
}
