use super::inject_path_scope;

define_cases! {
    volatile_integer: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 123])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(name:a.txt & size:)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].name, "123");
            assert!(res.results[0].get_all_values("type").contains(&"integer".to_string()));
            assert!(res.results[0].get_all_values("value").contains(&"123".to_string()));
            Ok(())
        },
    },
    volatile_double: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 100])?;
            std::fs::write(dir.join("b.txt"), vec![0u8; 200])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "avg((name:a.txt | name:b.txt) & size:)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert!(res.results[0].name.contains("150"));
            assert!(res.results[0].get_all_values("type").contains(&"double".to_string()));
            assert!(res.results[0].get_all_values("value").iter().any(|v| v.contains("150")));
            Ok(())
        },
    },
    volatile_boolean: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(name:a.txt & size:) == 100",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].name, "TRUE");
            assert!(res.results[0].get_all_values("type").contains(&"boolean".to_string()));
            let vals = res.results[0].get_all_values("value");
            assert!(vals.iter().any(|v| v.to_lowercase() == "true"));
            Ok(())
        },
    },
}
