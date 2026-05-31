// Copyright (C) 2026 coponhub
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

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
            let names: Vec<_> = res.results.iter().map(|r| r.raw_repr()).collect();
            assert!(names.contains(&"medium.txt".to_string()), "medium.txt should match");
            assert!(!names.contains(&"small.txt".to_string()), "small.txt should NOT match");
            assert!(!names.contains(&"large.txt".to_string()), "large.txt should NOT match");
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
            let names: Vec<_> = res.results.iter().map(|r| r.raw_repr()).collect();
            assert!(names.contains(&"small.txt".to_string()));
            assert!(names.contains(&"medium.txt".to_string()));
            assert!(names.contains(&"large.txt".to_string()));
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
            let names: Vec<_> = res.results.iter().map(|r| r.raw_repr()).collect();
            assert!(names.contains(&"medium.txt".to_string()));
            assert!(!names.contains(&"small.txt".to_string()));
            assert!(!names.contains(&"large.txt".to_string()));
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
            assert_eq!(res.results[0].raw_repr(), "200B", "Sum of medium file size should be 200B");
            Ok(())
        },
    },
}
