// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
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

/// 算術演算 (Calculation) 機能の統合テスト
use super::default_scope;
use super::inject_path_scope;
use tempfile::tempdir;
use ttfm::search;

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
                res.results.iter().any(|item| item.raw_repr().contains("file1.txt")),
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
                res.results.iter().any(|item| item.raw_repr().contains("large.txt")),
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
                res.results.iter().any(|item| item.raw_repr().contains("huge.txt")),
                "Results should contain huge.txt"
            );
            assert!(
                !res.results.iter().any(|item| item.raw_repr().contains("medium.txt")),
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
                res.results.iter().any(|item| item.raw_repr().contains("big.txt")),
                "Results should contain big.txt"
            );
            assert!(
                !res.results.iter().any(|item| item.raw_repr().contains("small.txt")),
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
                res.results.iter().any(|item| item.raw_repr().contains("large.dat")),
                "Results should contain large.dat"
            );
            assert!(
                !res.results.iter().any(|item| item.raw_repr().contains("medium.dat")),
                "Should not contain medium.dat"
            );
            assert!(
                !res.results.iter().any(|item| item.raw_repr().contains("small.dat")),
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
            assert!(res.results.iter().any(|i| i.raw_repr().contains("f20.txt")), "f20.txt (20 > 15) should match");
            assert!(!res.results.iter().any(|i| i.raw_repr().contains("f10.txt")), "f10.txt (10 < 15) should not match");
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
            assert!(res.results.iter().any(|i| i.raw_repr().contains("f50.txt")), "f50.txt (50 > 20) should match");
            assert!(!res.results.iter().any(|i| i.raw_repr().contains("f20.txt")), "f20.txt (20 = 20) should not match");
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
            assert!(res.results.iter().any(|i| i.raw_repr().contains("f20.txt")), "Multiplication: f20.txt (20 > 15) should match");
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
            assert!(res.results.iter().any(|i| i.raw_repr().contains("f50.txt")), "Division: f50.txt (50 > 20) should match");
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
            assert!(res.results.iter().any(|i| i.raw_repr().contains("f10.txt")), "Modulo: f10.txt (10 > 5) should match");
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
            // raw_repr() is now size-formatted; use value tag to check numeric result
            let value_strs = res.results[0].get_all_values("value");
            let val: i64 = value_strs[0].parse().unwrap_or(-1);
            assert!(val > 0, "Result should be a positive number, got value: {:?}", value_strs);
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
            assert_eq!(res.results[0].raw_repr(), "TRUE");
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
            // raw_repr() is now size-formatted; use value tag to check numeric result
            let value_strs = res.results[0].get_all_values("value");
            let val: i64 = value_strs[0].parse().unwrap_or(-1);
            assert!(val > 0, "Result should be a positive number, got value: {:?}", value_strs);
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
            let names: Vec<String> = res.results.iter().map(|r| r.raw_repr()).collect();
            assert!(names.contains(&"large.txt".to_string()), "large.txt (200B) should match: got {:?}", names);
            assert!(!names.contains(&"small.txt".to_string()), "small.txt (100B) should not match: got {:?}", names);
            assert!(!names.contains(&"tiny.txt".to_string()), "tiny.txt (50B) should not match: got {:?}", names);
            Ok(())
        },
    },
    // Projection同士の & に Calculation キーが含まれる場合のテスト
    calc_projection_intersect_no_crash: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), b"hello")?;
            std::fs::write(dir.join("b.txt"), b"world!!")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(size: / 2) & parentdir:",
        assert: |res, _dir| {
            // 型が一致しないので空でも有効なプロジェクションでもよい（クラッシュしないことが重要）
            let _ = res;
            Ok(())
        },
    },
    calc_projection_intersect_matching_values: {
        setup: |dir| {
            // size=8: size/2=4, size=2: size*2=4 → ラベル値4が両辺に現れる
            std::fs::write(dir.join("size8.txt"), vec![0u8; 8])?;
            std::fs::write(dir.join("size2.txt"), vec![0u8; 2])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(size: / 2) & (size: * 2)",
        assert: |res, _dir| {
            let reprs: Vec<String> = res.results.iter().map(|r| r.raw_repr()).collect();
            // ラベル値4が積集合に現れるべき (size8: 8/2=4, size2: 2*2=4)
            let item_with_4 = res.results.iter().find(|r| r.raw_repr() == "4")
                .unwrap_or_else(|| panic!("ラベル値4が積集合に現れるべき: {:?}", reprs));
            // size8.txt と size2.txt の両方がそのグループに含まれるべき
            let item_strs: Vec<String> = item_with_4.tags.entries.iter()
                .filter(|e| e.typed_tag.tag_type() == ttfm::types::TagType::from("item"))
                .map(|e| e.typed_tag.as_str())
                .collect();
            assert!(
                item_strs.iter().any(|s| s.contains("size8")),
                "size8.txt が結果グループに含まれるべき: {:?}", item_strs
            );
            assert!(
                item_strs.iter().any(|s| s.contains("size2")),
                "size2.txt が結果グループに含まれるべき: {:?}", item_strs
            );
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

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let (store, registry) = (db_dir_store, db_dir_registry);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res_explicit = search::search_nowarn(
        &store,
        &registry,
        "sum((size: - 100))",
        Default::default(),
    )?;
    assert!(
        !res_explicit.results.is_empty(),
        "Explicit paren should work"
    );

    let res_bare = search::search_nowarn(
        &store,
        &registry,
        "sum(size: - 100)",
        Default::default(),
    )?;
    assert!(!res_bare.results.is_empty(), "Bare calc should work");

    assert_eq!(
        res_explicit.results[0].raw_repr(),
        res_bare.results[0].raw_repr(),
        "bare_calculation and explicit paren should produce the same result"
    );

    Ok(())
}
