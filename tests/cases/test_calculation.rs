/// 算術演算 (Calculation) 機能の統合テスト
use super::default_scope;
use super::inject_path_scope;
use tempfile::tempdir;
use ttfm::FileManager;

define_cases! {
    calc_literal_simple: {
        setup: |dir| {
            std::fs::write(dir.join("file1.txt"), b"12345")?; // 5 bytes
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(1 + 2) :< size:",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should have at least one result");
            assert!(
                res.results.iter().any(|item| item.name.contains("file1.txt")),
                "Results should contain file1.txt"
            );
            Ok(())
        },
    },
    calc_with_tag: {
        setup: |dir| {
            std::fs::write(dir.join("large.txt"), vec![0u8; 1000])?;
            std::fs::write(dir.join("small.txt"), vec![0u8; 500])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(size: + 100) :> 1000",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert!(
                res.results.iter().any(|item| item.name.contains("large.txt")),
                "Results should contain large.txt"
            );
            Ok(())
        },
    },
    calc_tag_comparison: {
        setup: |dir| {
            std::fs::write(dir.join("huge.txt"), vec![0u8; 2000])?;
            std::fs::write(dir.join("medium.txt"), vec![0u8; 1000])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(1000 + 500) :< size:",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert!(
                res.results.iter().any(|item| item.name.contains("huge.txt")),
                "Results should contain huge.txt"
            );
            assert!(
                !res.results.iter().any(|item| item.name.contains("medium.txt")),
                "Results should not contain medium.txt (1000 < 1500)"
            );
            Ok(())
        },
    },
    calc_nested: {
        setup: |dir| {
            std::fs::write(dir.join("big.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("small.txt"), vec![0u8; 5])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "((1 + 2) * 3) :< size:",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert!(
                res.results.iter().any(|item| item.name.contains("big.txt")),
                "Results should contain big.txt"
            );
            assert!(
                !res.results.iter().any(|item| item.name.contains("small.txt")),
                "Results should not contain small.txt (5 < 9)"
            );
            Ok(())
        },
    },
    calc_size_unit: {
        setup: |dir| {
            std::fs::write(dir.join("large.dat"), vec![0u8; 1_048_776])?;
            std::fs::write(dir.join("medium.dat"), vec![0u8; 1_048_576])?;
            std::fs::write(dir.join("small.dat"), vec![0u8; 1_048_476])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(1MB + 100B) :< size:",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert!(
                res.results.iter().any(|item| item.name.contains("large.dat")),
                "Results should contain large.dat"
            );
            assert!(
                !res.results.iter().any(|item| item.name.contains("medium.dat")),
                "Should not contain medium.dat"
            );
            assert!(
                !res.results.iter().any(|item| item.name.contains("small.dat")),
                "Should not contain small.dat"
            );
            Ok(())
        },
    },
    calc_operator_add: {
        setup: |dir| {
            std::fs::write(dir.join("f10.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("f20.txt"), vec![0u8; 20])?;
            std::fs::write(dir.join("f50.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(5 + 10) :< size:",
        assert: |res, _dir| {
            assert!(res.results.iter().any(|i| i.name.contains("f20.txt")), "f20.txt (20 > 15) should match");
            assert!(!res.results.iter().any(|i| i.name.contains("f10.txt")), "f10.txt (10 < 15) should not match");
            Ok(())
        },
    },
    calc_operator_sub: {
        setup: |dir| {
            std::fs::write(dir.join("f10.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("f20.txt"), vec![0u8; 20])?;
            std::fs::write(dir.join("f50.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(30 - 10) :< size:",
        assert: |res, _dir| {
            assert!(res.results.iter().any(|i| i.name.contains("f50.txt")), "f50.txt (50 > 20) should match");
            assert!(!res.results.iter().any(|i| i.name.contains("f20.txt")), "f20.txt (20 = 20) should not match");
            Ok(())
        },
    },
    calc_operator_mul: {
        setup: |dir| {
            std::fs::write(dir.join("f10.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("f20.txt"), vec![0u8; 20])?;
            std::fs::write(dir.join("f50.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(5 * 3) :< size:",
        assert: |res, _dir| {
            assert!(res.results.iter().any(|i| i.name.contains("f20.txt")), "Multiplication: f20.txt (20 > 15) should match");
            Ok(())
        },
    },
    calc_operator_div: {
        setup: |dir| {
            std::fs::write(dir.join("f10.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("f20.txt"), vec![0u8; 20])?;
            std::fs::write(dir.join("f50.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(100 / 5) :< size:",
        assert: |res, _dir| {
            assert!(res.results.iter().any(|i| i.name.contains("f50.txt")), "Division: f50.txt (50 > 20) should match");
            Ok(())
        },
    },
    calc_operator_mod: {
        setup: |dir| {
            std::fs::write(dir.join("f10.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("f20.txt"), vec![0u8; 20])?;
            std::fs::write(dir.join("f50.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(25 % 20) :< size:",
        assert: |res, _dir| {
            assert!(res.results.iter().any(|i| i.name.contains("f10.txt")), "Modulo: f10.txt (10 > 5) should match");
            Ok(())
        },
    },
    calc_agg_simple: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 300])?;
            std::fs::write(dir.join("b.txt"), vec![0u8; 300])?;
            std::fs::write(dir.join("c.txt"), vec![0u8; 400])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "(sum(size:) + 100) > 1000",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should have results");
            Ok(())
        },
    },
    calc_agg_complex: {
        setup: |dir| {
            std::fs::write(dir.join("x.txt"), vec![0u8; 100])?;
            std::fs::write(dir.join("y.txt"), vec![0u8; 150])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size:) > (100 * 2)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should have results");
            Ok(())
        },
    },
    calc_bare_sub: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 200])?;
            std::fs::write(dir.join("b.txt"), vec![0u8; 300])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size: - 100)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should return a scalar result");
            let val: f64 = res.results[0].name.parse().unwrap_or(f64::NAN);
            assert!(!val.is_nan(), "Result should be a number, got: {}", res.results[0].name);
            Ok(())
        },
    },
    calc_bare_mul_cmp: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 400])?;
            std::fs::write(dir.join("b.txt"), vec![0u8; 600])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size: * 2) > 1000",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].name, "TRUE");
            Ok(())
        },
    },
    calc_bare_multiop: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 200])?;
            std::fs::write(dir.join("b.txt"), vec![0u8; 300])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size: + 100 - 50)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should return a scalar result");
            let val: f64 = res.results[0].name.parse().unwrap_or(f64::NAN);
            assert!(!val.is_nan(), "Result should be a number, got: {}", res.results[0].name);
            Ok(())
        },
    },
    calc_vs_calculation: {
        setup: |dir| {
            std::fs::write(dir.join("large.txt"), vec![0u8; 200])?;
            std::fs::write(dir.join("small.txt"), vec![0u8; 100])?;
            std::fs::write(dir.join("tiny.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(size: - 100) :> (size: * 0.1)",
        assert: |res, _dir| {
            let names: Vec<&str> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|n| n.contains("large.txt")), "large.txt (200B) should match: got {:?}", names);
            assert!(!names.iter().any(|n| n.contains("small.txt")), "small.txt (100B) should not match: got {:?}", names);
            assert!(!names.iter().any(|n| n.contains("tiny.txt")), "tiny.txt (50B) should not match: got {:?}", names);
            Ok(())
        },
    },
}

/// bare_calculation — ベースライン: 明示的括弧版 sum((size: - 100)) と同じ結果
#[test]
fn test_aggregation_bare_calc_explicit_paren_baseline() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.txt"), vec![0u8; 200])?;
    std::fs::write(root.join("b.txt"), vec![0u8; 300])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res_explicit = fm.search("sum((size: - 100))", Default::default())?;
    assert!(
        !res_explicit.results.is_empty(),
        "Explicit paren should work"
    );

    let res_bare = fm.search("sum(size: - 100)", Default::default())?;
    assert!(!res_bare.results.is_empty(), "Bare calc should work");

    assert_eq!(
        res_explicit.results[0].name, res_bare.results[0].name,
        "bare_calculation and explicit paren should produce the same result"
    );

    Ok(())
}
