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
use tempfile::tempdir;
use ttfm::query::lens_resolver::Resolver;
use ttfm::search;
use ttfm::tag::TagRegistry;
use ttfm::types::SType;
use ttfm::{QueryNode, TypedTag};

define_cases! {
    // ── EAV 列は値の型で選ばれる ───────────────────────

    column_selects_integer_only: {
        setup: |dir| {
            std::fs::write(dir.join("int_row.txt"), "")?;
            std::fs::write(dir.join("str_row.txt"), "")?;
            std::fs::write(dir.join("dbl_row.txt"), "")?;
            Ok(())
        },
        tags: &[
            ("int_row.txt", "cat:42"),
            ("str_row.txt", "cat:\"42\""),
            ("dbl_row.txt", "cat:42.0"),
        ],
        query: "cat: := 42",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1, "cat:42 must not hit the string or double row");
            assert_eq!(res.results[0].raw_repr(), "int_row.txt");
            Ok(())
        },
    },
    column_selects_string_only: {
        setup: |dir| {
            std::fs::write(dir.join("int_row.txt"), "")?;
            std::fs::write(dir.join("str_row.txt"), "")?;
            std::fs::write(dir.join("dbl_row.txt"), "")?;
            Ok(())
        },
        tags: &[
            ("int_row.txt", "cat:42"),
            ("str_row.txt", "cat:\"42\""),
            ("dbl_row.txt", "cat:42.0"),
        ],
        query: "cat: := \"42\"",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1, "cat:\"42\" must not hit the integer or double row");
            assert_eq!(res.results[0].raw_repr(), "str_row.txt");
            Ok(())
        },
    },
    column_selects_double_only: {
        setup: |dir| {
            std::fs::write(dir.join("int_row.txt"), "")?;
            std::fs::write(dir.join("str_row.txt"), "")?;
            std::fs::write(dir.join("dbl_row.txt"), "")?;
            Ok(())
        },
        tags: &[
            ("int_row.txt", "cat:42"),
            ("str_row.txt", "cat:\"42\""),
            ("dbl_row.txt", "cat:42.0"),
        ],
        query: "cat: := 42.0",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1, "cat:42.0 must not hit the integer or string row");
            assert_eq!(res.results[0].raw_repr(), "dbl_row.txt");
            Ok(())
        },
    },
    numeric_ordering_spans_int_and_double: {
        setup: |dir| {
            std::fs::write(dir.join("one.txt"), "")?;
            std::fs::write(dir.join("one_five.txt"), "")?;
            Ok(())
        },
        tags: &[
            ("one.txt", "cat:1"),
            ("one_five.txt", "cat:1.5"),
        ],
        query: "cat: :> 0",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 2, "order comparison must see both the int and double column");
            Ok(())
        },
    },
    unregistered_tag_unit_value_round_trips: {
        setup: |dir| {
            std::fs::write(dir.join("unit_row.txt"), "")?;
            Ok(())
        },
        tags: &[
            ("unit_row.txt", "cat:1MB"),
        ],
        query: "cat:1MB",
        assert: |res, _dir| {
            assert_eq!(
                res.results.len(), 1,
                "a tag with no TagFn stores its value verbatim, so the same \
                 text must find it again"
            );
            assert_eq!(res.results[0].raw_repr(), "unit_row.txt");
            Ok(())
        },
    },
    unregistered_tag_column_follows_value_type: {
        setup: |dir| {
            std::fs::write(dir.join("width_row.txt"), "")?;
            Ok(())
        },
        tags: &[
            ("width_row.txt", "width:640"),
        ],
        query: "width: :> 500",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].raw_repr(), "width_row.txt");
            Ok(())
        },
    },
    order_comparison_with_double_literal_reaches_integer_column: {
        setup: |dir| {
            std::fs::write(dir.join("width_row.txt"), "")?;
            Ok(())
        },
        tags: &[
            ("width_row.txt", "width:501"),
        ],
        query: "width: :> 500.5",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].raw_repr(), "width_row.txt");
            Ok(())
        },
    },

    // ── 含意（extension: は is_dir:false を伴う） ───────────

    implication_tag_form_top_level: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "a")?;
            std::fs::create_dir_all(dir.join("weird.rs"))?;
            std::fs::write(dir.join("weird.rs").join("inner.txt"), "x")?;
            Ok(())
        },
        query: "extension:rs",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1, "a directory named *.rs must not match extension:rs");
            Ok(())
        },
    },
    implication_projection_form_top_level: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "a")?;
            std::fs::create_dir_all(dir.join("onlydir.zzz"))?;
            Ok(())
        },
        query: "extension:",
        assert: |res, _dir| {
            assert!(res.results.iter().any(|r| r.raw_repr() == "rs"));
            assert!(
                !res.results.iter().any(|r| r.raw_repr() == "zzz"),
                "a directory-only extension must not appear as a projection group"
            );
            Ok(())
        },
    },
    implication_tag_form_operand: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "a")?;
            std::fs::create_dir_all(dir.join("weird.rs"))?;
            std::fs::write(dir.join("weird.rs").join("inner.txt"), "x")?;
            Ok(())
        },
        format_query: inject_path_scope,
        query: "count(extension:rs)",
        assert: |res, _dir| {
            assert_eq!(res.results[0].raw_repr(), "1");
            Ok(())
        },
    },
    implication_projection_form_operand: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::create_dir_all(dir.join("weird.rs"))?;
            Ok(())
        },
        format_query: inject_path_scope,
        query: "count(extension:)",
        assert: |res, _dir| {
            assert_eq!(res.results[0].raw_repr(), "2", "distinct extensions among files only: rs, txt");
            Ok(())
        },
    },
    implication_projection_form_nest_rhs: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "a")?;
            std::fs::create_dir_all(dir.join("onlydir.zzz"))?;
            Ok(())
        },
        query: "parentdir: &: extension:",
        assert: |res, _dir| {
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

// ── 値の解釈: size:/mtime: のタグ形（既存テストの穴埋め） ───────

#[test]
fn size_tag_form_unit_matches_exact_file() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("small.bin"), vec![0u8; 100])?;
    std::fs::write(root.join("big.bin"), vec![0u8; 2048])?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = search::search_nowarn(
        &store,
        &registry,
        "size:2k",
        Default::default(),
    )?;
    assert_eq!(
        res.results.len(),
        1,
        "size:2k tag form must match exactly the 2048-byte file"
    );
    assert_eq!(res.results[0].raw_repr(), "big.bin");
    Ok(())
}

#[test]
fn mtime_tag_form_year_excludes_other_years() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let make = |name: &str, iso: &str| -> anyhow::Result<()> {
        let path = root.join(name);
        std::fs::File::create(&path)?;
        let dt = chrono::DateTime::parse_from_rfc3339(iso)?;
        let mtime = filetime::FileTime::from_unix_time(dt.timestamp(), 0);
        filetime::set_file_mtime(&path, mtime)?;
        Ok(())
    };
    make("a2025.txt", "2025-06-15T12:00:00Z")?;
    make("b2024.txt", "2024-06-15T12:00:00Z")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = search::search_nowarn(
        &store,
        &registry,
        "mtime:2025",
        Default::default(),
    )?;
    assert_eq!(
        res.results.len(),
        1,
        "mtime:2025 tag form must exclude the 2024 file"
    );
    assert_eq!(res.results[0].raw_repr(), "a2025.txt");
    Ok(())
}

// ── 値の解釈: 展開後も素の表記が TypedTag から取れること ───────

fn collect_typed_tags(node: &QueryNode) -> Vec<&TypedTag> {
    match node {
        QueryNode::And(children) | QueryNode::Or(children) => {
            children.iter().flat_map(collect_typed_tags).collect()
        }
        QueryNode::Difference(lhs, rhs) => {
            let mut tags = collect_typed_tags(lhs);
            tags.extend(collect_typed_tags(rhs));
            tags
        }
        QueryNode::TypedTag(tt) => vec![tt],
        _ => Vec::new(),
    }
}

#[test]
fn size_tag_form_expand_keeps_raw_unit_form() -> anyhow::Result<()> {
    let resolver =
        Resolver::new_nowarn("size:1k", &TagRegistry::with_standard())?;
    let typed_tags = collect_typed_tags(&resolver.expanded_query);
    let raw = typed_tags
        .into_iter()
        .find(|tt| tt.tag_type() == SType::Size.into())
        .map(|tt| tt.label.as_str());
    assert_eq!(
        raw.as_deref(),
        Some("1k"),
        "size:1k expansion must keep the raw unit form on the TypedTag's Label; \
         normalize_label currently overwrites it with the interpreted byte value \
         (fixed in Step 6)"
    );
    Ok(())
}

// ── 含意（is_dir）: タグ形は注釈であり書き換えではない ───────

#[test]
fn implication_tag_form_expand_is_a_single_annotated_typed_tag(
) -> anyhow::Result<()> {
    for (query, tag_type) in [
        ("extension:rs", SType::Extension.into()),
        ("filename:foo", SType::Filename.into()),
        ("directory:foo", SType::Directory.into()),
    ] {
        let resolver =
            Resolver::new_nowarn(query, &TagRegistry::with_standard())?;
        match &resolver.expanded_query {
            QueryNode::TypedTag(tt) => {
                assert_eq!(
                    tt.tag_type(),
                    tag_type,
                    "{query} must expand to a TypedTag of its own type, not be \
                     rewritten into the implied type"
                );
                assert!(
                    !tt.is_default_node(),
                    "{query} must carry its expansion as an annotation on the \
                     Node; a default node means the implication was dropped"
                );
            }
            other => panic!(
                "{query} must expand to a single annotated TypedTag \
                 (decision 11: tag form is an annotation, not a rewrite \
                 into a top-level And); got {other:?}"
            ),
        }
    }
    Ok(())
}

#[test]
fn mtime_tag_form_expand_keeps_raw_year_form() -> anyhow::Result<()> {
    let resolver =
        Resolver::new_nowarn("mtime:2026", &TagRegistry::with_standard())?;
    let typed_tags = collect_typed_tags(&resolver.expanded_query);
    let raw = typed_tags
        .into_iter()
        .find(|tt| tt.tag_type() == SType::Mtime.into())
        .map(|tt| tt.label.as_str());
    assert_eq!(
        raw.as_deref(),
        Some("2026"),
        "mtime:2026 expansion must keep a TypedTag carrying the raw year form on its Label; \
         expansion currently rewrites the tree into QueryNode::DateTimeRange instead of \
         annotating the TypedTag (fixed in Step 7, per decision 11: TypedTag expansion must \
         be an annotation, not a rewrite)"
    );
    Ok(())
}
