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

/// `plan_projection_as_nest.md` (Part A) の受け入れテスト。
use super::{default_scope, has_item_tags};

define_cases! {
    wildcard_key_identity_holds_in_results: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "*: &: extension:",
        assert: |res, _dir| {
            assert!(has_item_tags(&res.results));
            assert!(
                res.results.iter().any(|r| r.raw_repr() == "rs"),
                "'*: &: extension:' should behave exactly like 'extension:', \
                 got: {:?}",
                res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
            );
            assert!(res.results.iter().any(|r| r.raw_repr() == "txt"));
            Ok(())
        },
    },
    wildcard_key_alone_is_still_empty: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "*:",
        assert: |res, _dir| {
            // 廃止予定: plan_wildcard_type_key.md 実施後にこの空振り挙動は反転する
            assert_eq!(res.results.len(), 0);
            Ok(())
        },
    },
    wildcard_typed_tag_stays_an_item_filter: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "*:*",
        assert: |res, _dir| {
            assert!(!has_item_tags(&res.results), "*:* should be a flat item list");
            assert_eq!(res.results.len(), 2);
            Ok(())
        },
    },
    nest_rhs_implication_survives: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "")?;
            std::fs::create_dir_all(dir.join("onlydir.zzz"))?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: extension:",
        assert: |res, _dir| {
            assert!(has_item_tags(&res.results));
            assert!(
                res.results.iter().any(|r| r.raw_repr().contains("rs")),
                "the file's extension group must be present: {:?}",
                res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
            );
            assert!(
                !res.results.iter().any(|r| r.raw_repr().contains("zzz")),
                "a directory-only extension must not appear as a nested projection group: {:?}",
                res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
            );
            Ok(())
        },
    },
}
