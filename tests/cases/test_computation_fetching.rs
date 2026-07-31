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
            assert_eq!(res.results[0].raw_repr(), "1.07KB");
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
