use super::inject_path_scope;

define_cases! {
    computation_scalar_sum: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 100])?;
            std::fs::write(dir.join("large.txt"), vec![0u8; 1000])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:txt & size:)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].raw_repr(), "1.1KB");
            Ok(())
        },
    },
    computation_bool_simple: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size:) > 50",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            Ok(())
        },
    },
    computation_non_id_1: {
        setup: |dir| {
            std::fs::write(dir.join("other.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("target.rs"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        // Tests that boolean result is correct even when matched item ID is not 1
        format_query: inject_path_scope,
        query: "sum(extension:rs & size:) > 0",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].raw_repr(), "TRUE", "Should be TRUE even if matched ID is not 1");
            Ok(())
        },
    },
    computation_count_exact: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("b.txt"), vec![0u8; 10])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(extension:txt) == 2",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            Ok(())
        },
    },
    computation_reflexive: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "avg(size:) == avg(size:)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            Ok(())
        },
    },
}
