// Copyright (C) 2026 Kensuke Aoyagi
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

/// NULL伝播の統合テスト
use super::inject_path_scope;

define_cases! {
    null_propagation_empty_data: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "content")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "avg(extension:nonexistent & size:) == avg(extension:nonexistent & size:)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].raw_repr(), "NULL");
            assert!(res.results[0].get_all_values("bitical_type").contains(&"boolean".to_string()));
            Ok(())
        },
    },
    null_propagation_data_true: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "avg(size:) == avg(size:)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            Ok(())
        },
    },
    null_propagation_data_false: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size:) > 1000",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].raw_repr(), "FALSE");
            Ok(())
        },
    },
    null_propagation_single_aggregation_empty: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: inject_path_scope,
        query: "max(extension:nonexistent & size:)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].raw_repr(), "NULL");
            assert!(res.results[0].get_all_values("bitical_type").contains(&"numeric".to_string()));
            assert!(res.results[0].get_all_values("value").contains(&"NULL".to_string()));
            Ok(())
        },
    },
}
