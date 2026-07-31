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

use super::default_scope;

fn quote_files(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(dir.join("a b.txt"), "")?;
    std::fs::write(dir.join("axb.txt"), "")?;
    std::fs::write(dir.join("a*b.txt"), "")?;
    Ok(())
}

fn names(res: &ttfm::SearchResponse) -> Vec<String> {
    res.results.iter().map(|r| r.raw_repr()).collect()
}

define_cases! {
    quoted_without_metachar_stays_exact: {
        setup: quote_files,
        format_query: default_scope,
        query: "filename:\"a b.txt\"",
        assert: |res, _dir| {
            assert_eq!(
                names(res),
                vec!["a b.txt".to_string()],
                "metachar-free quoted string must still match exactly: {:?}",
                names(res)
            );
            Ok(())
        },
    },
    quoted_metachar_matches_as_glob: {
        setup: quote_files,
        format_query: default_scope,
        query: "filename:\"a*b.txt\"",
        assert: |res, _dir| {
            let mut got = names(res);
            got.sort();
            assert_eq!(
                got,
                vec!["a b.txt".to_string(), "a*b.txt".to_string(), "axb.txt".to_string()],
                "glob inside quotes must match as a pattern: {:?}",
                got
            );
            Ok(())
        },
    },
    quoted_escaped_metachar_matches_literal_only: {
        setup: quote_files,
        format_query: default_scope,
        query: "filename:\"a\\*b.txt\"",
        assert: |res, _dir| {
            assert_eq!(
                names(res),
                vec!["a*b.txt".to_string()],
                "escaped star inside quotes must match a literal star only: {:?}",
                names(res)
            );
            Ok(())
        },
    },
    quoted_number_is_interpreted_in_type_context: {
        setup: quote_files,
        format_query: default_scope,
        query: "rank:\"0\" & is_dir:false",
        assert: |res, _dir| {
            assert_eq!(
                res.results.len(),
                3,
                "type context interprets the value regardless of quoting: {:?}",
                names(res)
            );
            Ok(())
        },
    },
}

#[test]
fn test_rank_unparseable_value_returns_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    std::fs::write(root.join("a.txt"), "content")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "rank:abc",
        Default::default(),
    );

    assert!(
        res.is_err(),
        "rank:abc is not interpretable as a rank and must error, not return 0 results: {:?}",
        res.map(|r| r.results.len())
    );

    Ok(())
}
