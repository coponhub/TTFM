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

use crate::cases::*;

define_cases! {
    eval_single_tag_dereference: {
        setup: |dir| {
            std::fs::write(dir.join("main.rs"), "fn main() {}")?;
            std::fs::write(dir.join("lib.rs"), "pub fn lib() {}")?;
            Ok(())
        },
        tags: &[("main.rs", "project:core")],
        modify: Some(|store, registry, _dir| {
            ttfm::edit::edit(
                store,
                registry,
                "tag:\"project:core\"",
                Some("milestone:m1"),
                ttfm::edit::QueryType::Tag,
                None,
                ttfm::edit::WriteOptions::default(),
                &mut Vec::new(),
            )?;
            Ok(())
        }),
        format_query: default_scope,
        query: "q(milestone:m1) & extension:rs",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert!(res.results[0].raw_repr().contains("main.rs"));
            Ok(())
        },
    },
    eval_type_wildcard_dereference: {
        setup: |dir| {
            std::fs::write(dir.join("main.rs"), "fn main() {}")?;
            std::fs::write(dir.join("readme.md"), "# Readme")?;
            Ok(())
        },
        modify: Some(|store, registry, _dir| {
            ttfm::edit::write::write(
                store,
                registry,
                vec![ttfm::edit::write::WriteAction::Add {
                    item: ttfm::types::ItemId::Volatile(0),
                    tags: vec![
                        ttfm::edit::write::TagOp::Append(ttfm::types::TypedTag::new(ttfm::types::SType::ItemKind, "type")),
                        ttfm::edit::write::TagOp::Append(ttfm::types::TypedTag::new(ttfm::types::SType::Content, "extension")),
                        ttfm::edit::write::TagOp::Append(ttfm::types::TypedTag::new("status", "active")),
                    ],
                }],
                None,
            )?;
            Ok(())
        }),
        format_query: default_scope,
        query: "q(status:active)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 2);
            Ok(())
        },
    },
    eval_stored_file_self_reference: {
        setup: |dir| {
            std::fs::write(dir.join("main.rs"), "fn main() {}")?;
            Ok(())
        },
        tags: &[("main.rs", "pinned:true")],
        format_query: default_scope,
        query: "q(pinned:true)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert!(res.results[0].raw_repr().contains("main.rs"));
            Ok(())
        },
    },
    eval_in_aggregation_and_nest_rhs: {
        setup: |dir| {
            std::fs::write(dir.join("main.rs"), "fn main() {}")?;
            Ok(())
        },
        tags: &[("main.rs", "project:core")],
        modify: Some(|store, registry, _dir| {
            ttfm::edit::edit(
                store,
                registry,
                "tag:\"project:core\"",
                Some("milestone:m1"),
                ttfm::edit::QueryType::Tag,
                None,
                ttfm::edit::WriteOptions::default(),
                &mut Vec::new(),
            )?;
            Ok(())
        }),
        format_query: default_scope,
        query: "extension: &: (q(milestone:m1))",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            Ok(())
        },
    },
    eval_glob_character_escaping: {
        setup: |dir| {
            std::fs::write(dir.join("main.rs"), "fn main() {}")?;
            Ok(())
        },
        modify: Some(|store, registry, _dir| {
            ttfm::edit::edit(
                store,
                registry,
                "tag:\"pattern:*.rs\"",
                Some("flag:test"),
                ttfm::edit::QueryType::Tag,
                None,
                ttfm::edit::WriteOptions::default(),
                &mut Vec::new(),
            )?;
            Ok(())
        }),
        format_query: default_scope,
        query: "q(flag:test)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 0);
            Ok(())
        },
    },
    eval_empty_match: {
        setup: |dir| {
            std::fs::write(dir.join("main.rs"), "fn main() {}")?;
            Ok(())
        },
        format_query: default_scope,
        query: "q(non_existent:tag) & extension:rs",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 0);
            Ok(())
        },
    },
    eval_nested_eval_recursion: {
        setup: |dir| {
            std::fs::write(dir.join("main.rs"), "fn main() {}")?;
            Ok(())
        },
        tags: &[("main.rs", "project:ttfm")],
        modify: Some(|store, registry, _dir| {
            ttfm::edit::edit(
                store,
                registry,
                "tag:\"project:ttfm\"",
                Some("milestone:m1"),
                ttfm::edit::QueryType::Tag,
                None,
                ttfm::edit::WriteOptions::default(),
                &mut Vec::new(),
            )?;
            ttfm::edit::edit(
                store,
                registry,
                "tag:\"milestone:m1\"",
                Some("category:core"),
                ttfm::edit::QueryType::Tag,
                None,
                ttfm::edit::WriteOptions::default(),
                &mut Vec::new(),
            )?;
            Ok(())
        }),
        format_query: default_scope,
        query: "q(q(category:core))",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert!(res.results[0].raw_repr().contains("main.rs"));
            Ok(())
        },
    },
}

#[test]
fn test_q_large_expansion_in_list_no_ulimit_crash() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join("db");
    std::fs::create_dir_all(&db_dir)?;
    let store = ttfm::db::Store::open(&db_dir)?;
    let registry = ttfm::tag::TagRegistry::with_standard();
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    for i in 0..2000 {
        std::fs::File::create(root.join(format!("file_{:04}.txt", i)))?;
    }
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;
    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "q(extension:txt) & size:<10000",
        ttfm::SearchOptions::default(),
    )?;
    assert_eq!(res.results.len(), 2000);
    Ok(())
}

#[test]
fn test_q_lightweight_eval_expansion() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join("db");
    std::fs::create_dir_all(&db_dir)?;
    let store = ttfm::db::Store::open(&db_dir)?;
    let registry = ttfm::tag::TagRegistry::with_standard();
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;

    std::fs::write(root.join("a.rs"), "fn main() {}")?;
    std::fs::write(root.join("b.txt"), "hello")?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "extension:rs & q(name:*.rs)",
        ttfm::SearchOptions::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(
        res.results[0].get_tag_value("filename").as_deref(),
        Some("a.rs")
    );

    Ok(())
}
