/// 連鎖比較の統合テスト
use super::{default_scope, inject_path_scope};

define_cases! {
    chain_comparison_medium_only: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 50])?;
            std::fs::write(dir.join("medium.txt"), vec![0u8; 200])?;
            std::fs::write(dir.join("large.txt"), vec![0u8; 1000])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "100 :< size: :<= 500",
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.contains(&"medium.txt"), "medium.txt should match");
            assert!(!names.contains(&"small.txt"), "small.txt should NOT match");
            assert!(!names.contains(&"large.txt"), "large.txt should NOT match");
            Ok(())
        },
    },
    chain_comparison_all_sizes: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 50])?;
            std::fs::write(dir.join("medium.txt"), vec![0u8; 200])?;
            std::fs::write(dir.join("large.txt"), vec![0u8; 1000])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "10 :<= size: :< 1001",
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.contains(&"small.txt"));
            assert!(names.contains(&"medium.txt"));
            assert!(names.contains(&"large.txt"));
            Ok(())
        },
    },
    chain_comparison_reverse: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 50])?;
            std::fs::write(dir.join("medium.txt"), vec![0u8; 200])?;
            std::fs::write(dir.join("large.txt"), vec![0u8; 1000])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "500 :>= size: :> 100",
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.contains(&"medium.txt"));
            assert!(!names.contains(&"small.txt"));
            assert!(!names.contains(&"large.txt"));
            Ok(())
        },
    },
    chain_comparison_agg_sum: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 50])?;
            std::fs::write(dir.join("medium.txt"), vec![0u8; 200])?;
            std::fs::write(dir.join("large.txt"), vec![0u8; 1000])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum((100 :< size: :<= 500) & size:)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].name, "200", "Sum of medium file size should be 200");
            Ok(())
        },
    },
}
