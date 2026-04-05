use super::inject_path_scope;

define_cases! {
    agg_calc_base_sum: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), vec![0u8; 10 * 1024])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:txt & (size:))",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            let total: i64 = res.results[0].name.parse()?;
            assert_eq!(total, 10 * 1024);
            Ok(())
        },
    },
    agg_calc_minus_1000: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), vec![0u8; 10 * 1024])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:txt & ((size: - 1000)))",
        assert: |res, _dir| {
            assert_eq!(res.results[0].name, "9240", "sum(size: - 1000) should be 9240");
            Ok(())
        },
    },
    agg_calc_complex: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), vec![0u8; 10 * 1024])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:txt & ((size: - (1000 / 2))))",
        assert: |res, _dir| {
            assert_eq!(res.results[0].name, "9740", "sum(size: - 1000 / 2) should be 9740");
            Ok(())
        },
    },
    agg_calc_unknown_null: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), "content")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(unknown_tag: + 1)",
        assert: |res, _dir| {
            assert_eq!(res.results[0].name, "NULL", "sum of unknown tag + 1 should be NULL");
            assert!(
                res.results[0].get_all_values("type").contains(&"numeric".to_string()),
                "type should be 'numeric' for NULL aggregation result"
            );
            Ok(())
        },
    },
    agg_calc_complex_expr_null: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), "content")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum((non_existant_tag: :> size:) & size:)",
        assert: |res, _dir| {
            assert_eq!(res.results[0].name, "NULL", "complex sum expression should be NULL");
            Ok(())
        },
    },
}
