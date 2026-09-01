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
            assert_eq!(res.results[0].raw_repr(), "123B");
            assert!(res.results[0].get_all_values("bitical_type").contains(&"integer".to_string()));
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
            assert_eq!(res.results[0].raw_repr(), "150B");
            assert!(res.results[0].get_all_values("bitical_type").contains(&"double".to_string()));
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
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            assert!(res.results[0].get_all_values("bitical_type").contains(&"boolean".to_string()));
            let vals = res.results[0].get_all_values("value");
            assert!(vals.iter().any(|v| v.to_lowercase() == "true"));
            Ok(())
        },
    },
}
