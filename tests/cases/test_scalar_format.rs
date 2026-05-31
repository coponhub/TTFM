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
    // max(mtime:) の結果が日時フォーマットで表示される
    scalar_format_max_mtime: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "data")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "max(name:a.txt & mtime:)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            let repr = res.results[0].raw_repr();
            assert!(
                repr.contains('-') && repr.contains(':'),
                "max(mtime:) should format as date string, got: {:?}",
                repr
            );
            assert!(
                repr.parse::<i64>().is_err(),
                "max(mtime:) should NOT be a raw integer, got: {:?}",
                repr
            );
            Ok(())
        },
    },

    // count() は型なし → フォーマットされない（数値のまま）
    scalar_format_count_no_format: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "fn main() {}")?;
            std::fs::write(dir.join("b.rs"), "fn foo() {}")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(extension:rs)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            let repr = res.results[0].raw_repr();
            assert!(
                repr.parse::<i64>().is_ok(),
                "count() result should remain a raw number, got: {:?}",
                repr
            );
            Ok(())
        },
    },

    // sum(size: + mtime:) は複数型 → フォーマットされない
    scalar_format_mixed_types_no_format: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(name:a.txt & (size: + mtime:))",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            let repr = res.results[0].raw_repr();
            assert!(
                repr.parse::<i64>().is_ok() || repr == "NULL",
                "sum(size: + mtime:) with 2 types should remain raw number, got: {:?}",
                repr
            );
            Ok(())
        },
    },
}
