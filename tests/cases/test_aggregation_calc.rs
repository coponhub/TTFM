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
            let value_strs = res.results[0].get_all_values("value");
            let total: i64 = value_strs[0].parse()?;
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
            assert_eq!(res.results[0].raw_repr(), "9.0KB", "sum(size: - 1000) should be 9.0KB");
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
            assert_eq!(res.results[0].raw_repr(), "9.5KB", "sum(size: - 1000 / 2) should be 9.5KB");
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
            assert_eq!(res.results[0].raw_repr(), "NULL", "sum of unknown tag + 1 should be NULL");
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
            assert_eq!(res.results[0].raw_repr(), "NULL", "complex sum expression should be NULL");
            Ok(())
        },
    },
    // CalculationMatch (calc op literal) as filter inside aggregation
    agg_filter_calc_match: {
        setup: |dir| {
            std::fs::write(dir.join("big.bin"), vec![0u8; 300])?;
            std::fs::write(dir.join("small.bin"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum((size: - 100) :> 100 & size:)",
        assert: |res, _dir| {
            // filter: size - 100 > 100  =>  size > 200
            // big.bin (300 bytes): included; small.bin (50 bytes): excluded
            let value_strs = res.results[0].get_all_values("value");
            let total: i64 = value_strs[0].parse()?;
            assert_eq!(total, 300, "Only big.bin should be included in sum");
            Ok(())
        },
    },
    // TagCalculationMatch (tag op calc) as filter inside aggregation
    // (50 + 50) folds to Literal(100) => Match, not TagCalculationMatch.
    // To get TagCalculationMatch we need a calc with a tag on the rhs:
    // size: :> (size: * 0 + 100) -- rhs has size: so stays Calculation.
    agg_filter_tag_calc_match: {
        setup: |dir| {
            std::fs::write(dir.join("big.bin"), vec![0u8; 300])?;
            std::fs::write(dir.join("small.bin"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size: :> (size: * 0 + 100) & size:)",
        assert: |res, _dir| {
            // rhs calc: size * 0 + 100 = 100 (per item), so filter: size > 100
            // big.bin (300 bytes): included; small.bin (50 bytes): excluded
            let value_strs = res.results[0].get_all_values("value");
            let total: i64 = value_strs[0].parse()?;
            assert_eq!(total, 300, "Only big.bin should be included in sum");
            Ok(())
        },
    },
    // CalculationMatch with nested aggregation in lhs calc as filter
    // filter: size - sum(rs_sizes) > 100KB  => sum(rs_sizes) is a scalar subquery
    agg_filter_calc_with_nested_agg: {
        setup: |dir| {
            // big.txt: 300KB; rs_files total: 50KB => 300KB - 50KB = 250KB > 100KB => included
            std::fs::write(dir.join("big.txt"), vec![0u8; 300 * 1024])?;
            std::fs::write(dir.join("lib.rs"), vec![0u8; 30 * 1024])?;
            std::fs::write(dir.join("main.rs"), vec![0u8; 20 * 1024])?;
            // small.txt: 80KB; 80KB - 50KB = 30KB > 100KB => excluded
            std::fs::write(dir.join("small.txt"), vec![0u8; 80 * 1024])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(((size: - sum(extension:rs & size:)) :> 100KB) & size:)",
        assert: |res, _dir| {
            // sum(rs sizes) = 30KB + 20KB = 50KB = 51200 bytes
            // big.txt: 300KB - 50KB = 250KB > 100KB => included (300KB = 307200)
            // small.txt: 80KB - 50KB = 30KB > 100KB => excluded
            let value_strs = res.results[0].get_all_values("value");
            let total: i64 = value_strs[0].parse()?;
            assert_eq!(total, 300 * 1024, "Only big.txt should be included in sum");
            Ok(())
        },
    },
    // CalculationCalculationMatch (calc op calc) as filter inside aggregation
    agg_filter_calc_calc_match: {
        setup: |dir| {
            std::fs::write(dir.join("big.bin"), vec![0u8; 300])?;
            std::fs::write(dir.join("small.bin"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum((size: * 2) :> (size: + 100) & size:)",
        assert: |res, _dir| {
            // filter: size * 2 > size + 100  =>  size > 100
            // big.bin (300 bytes): 600 > 400 => included; small.bin (50 bytes): 100 > 150 => excluded
            let value_strs = res.results[0].get_all_values("value");
            let total: i64 = value_strs[0].parse()?;
            assert_eq!(total, 300, "Only big.bin should be included in sum");
            Ok(())
        },
    },
}
