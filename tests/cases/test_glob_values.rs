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

fn three_files(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(dir.join("a.txt"), "")?;
    std::fs::write(dir.join("b.txt"), "")?;
    std::fs::write(dir.join("c.txt"), "")?;
    Ok(())
}

fn names(res: &ttfm::SearchResponse) -> Vec<String> {
    res.results.iter().map(|r| r.raw_repr()).collect()
}

define_cases! {
    bare_glob_user_tag_excludes_untagged: {
        setup: three_files,
        tags: &[("a.txt", "cat:one"), ("b.txt", "cat:two")],
        format_query: default_scope,
        query: "cat:*",
        assert: |res, _dir| {
            assert_eq!(
                res.results.len(),
                2,
                "cat:* はタグを持つファイルのみ（c.txt は含まれない）: {:?}",
                names(res)
            );
            Ok(())
        },
    },
    bare_glob_user_tag_negation_matches_nothing: {
        setup: three_files,
        tags: &[("a.txt", "cat:one")],
        format_query: default_scope,
        query: "cat: :^ *",
        assert: |res, _dir| {
            assert!(
                res.results.is_empty(),
                "全ての値が * に一致するので不一致は0件: {:?}",
                names(res)
            );
            Ok(())
        },
    },
    untyped_partial_digit_glob_stays_string_glob_not_error: {
        setup: three_files,
        tags: &[("a.txt", "cat:1x")],
        format_query: default_scope,
        query: "cat:1*",
        assert: |res, _dir| {
            // `cat:` has no TagFn (no OperandFormat dispatch), so '1*' is never
            // routed through size/mtime interpretation and stays a plain string glob.
            assert_eq!(
                res.results.len(),
                1,
                "cat:1* should match as a plain string glob, not error: {:?}",
                names(res)
            );
            Ok(())
        },
    },
    bare_glob_size_matches_all_files: {
        setup: three_files,
        format_query: default_scope,
        query: "size:* & is_dir:false",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 3, "size:* は全ファイル: {:?}", names(res));
            Ok(())
        },
    },
    bare_glob_rank_matches_all_files: {
        setup: three_files,
        format_query: default_scope,
        query: "rank:* & is_dir:false",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 3, "rank:* は全ファイル: {:?}", names(res));
            Ok(())
        },
    },
    bare_glob_item_id_matches_all_files: {
        setup: three_files,
        format_query: default_scope,
        query: "item_id:* & is_dir:false",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 3, "item_id:* は全ファイル: {:?}", names(res));
            Ok(())
        },
    },
    bare_glob_mtime_matches_all_files: {
        setup: three_files,
        format_query: default_scope,
        query: "mtime:* & is_dir:false",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 3, "mtime:* should match all files: {:?}", names(res));
            Ok(())
        },
    },
    nest_count_user_tag_glob_matches: {
        setup: |dir| {
            std::fs::create_dir_all(dir.join("d1"))?;
            std::fs::write(dir.join("d1/x.rs"), "")?;
            Ok(())
        },
        tags: &[("d1/x.rs", "cat:one")],
        format_query: default_scope,
        query: "parentdir: &: count(cat:o*) > 0",
        assert: |res, _dir| {
            assert_eq!(
                res.results.len(),
                1,
                "Nest 内 count でも glob が効く: {:?}",
                names(res)
            );
            Ok(())
        },
    },
    nest_count_merged_glob_matches: {
        setup: |dir| {
            std::fs::create_dir_all(dir.join("d3"))?;
            std::fs::write(dir.join("d3/z.rs"), "")?;
            Ok(())
        },
        tags: &[("d3/z.rs", "cat:one"), ("d3/z.rs", "dog:x")],
        format_query: default_scope,
        query: "parentdir: &: (count(cat:o*) > 0) & parentdir: &: (count(dog:x) > 0)",
        assert: |res, _dir| {
            assert_eq!(
                res.results.len(),
                1,
                "同一キーに合流した Nest count でも glob が効く: {:?}",
                names(res)
            );
            Ok(())
        },
    },
    calculation_wrapped_slot_glob_order: {
        setup: |dir| {
            let touch = |name: &str, datetime: &str| -> anyhow::Result<()> {
                let file_path = dir.join(name);
                std::fs::write(&file_path, "")?;
                let status = std::process::Command::new("touch")
                    .args(["-d", datetime, file_path.to_str().unwrap()])
                    .status()?;
                anyhow::ensure!(status.success(), "touch command failed");
                Ok(())
            };
            touch("late_feb.txt", "2026-02-28 23:00:00")?;
            touch("mid_feb.txt", "2026-02-15 10:00:00")?;
            Ok(())
        },
        format_query: default_scope,
        query: "(mtime: + 3600) :> *-02-*",
        assert: |res, _dir| {
            let names: std::collections::BTreeSet<String> =
                res.results.iter().map(|r| r.raw_repr()).collect();
            let expected: std::collections::BTreeSet<String> =
                ["late_feb.txt"].into_iter().map(String::from).collect();
            assert_eq!(
                names, expected,
                "算術ラップ後に3月へ繰り上がるものだけが一致する（文脈D）: {:?}",
                names
            );
            Ok(())
        },
    },
    nest_aggregation_slot_glob_unscoped: {
        setup: |dir| {
            std::fs::create_dir_all(dir.join("d2"))?;
            let file_path = dir.join("d2/y.txt");
            std::fs::write(&file_path, "")?;
            let status = std::process::Command::new("touch")
                .args(["-d", "2026-02-10 08:00:00", file_path.to_str().unwrap()])
                .status()?;
            anyhow::ensure!(status.success(), "touch command failed");
            Ok(())
        },
        format_query: |q, _| q.to_string(),
        query: "parentdir: &: (max(mtime:) == *-02-*)",
        assert: |res, _dir| {
            assert!(
                names(res).iter().any(|n| n == "y.txt"),
                "スコープ無しの Nest 内集約比較でもスロット glob が効く（文脈F・MergedNestMatch 経路）: {:?}",
                names(res)
            );
            Ok(())
        },
    },
    nest_aggregation_slot_glob_scoped: {
        setup: |dir| {
            std::fs::create_dir_all(dir.join("d4"))?;
            let file_path = dir.join("d4/y.txt");
            std::fs::write(&file_path, "")?;
            let status = std::process::Command::new("touch")
                .args(["-d", "2026-02-10 08:00:00", file_path.to_str().unwrap()])
                .status()?;
            anyhow::ensure!(status.success(), "touch command failed");
            Ok(())
        },
        format_query: default_scope,
        query: "parentdir: &: (max(mtime:) == *-02-*)",
        assert: |res, _dir| {
            assert_eq!(
                res.results.len(),
                1,
                "path: スコープ付きの Nest 内集約比較でもスロット glob が効く（文脈F・NestMatch 経路）: {:?}",
                names(res)
            );
            Ok(())
        },
    },
}

#[test]
fn test_aggregation_comparison_slot_glob_true() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let file_path = root.join("test.txt");
    std::fs::write(&file_path, "content")?;
    let status = std::process::Command::new("touch")
        .args(["-d", "2026-02-15 10:00:00", file_path.to_str().unwrap()])
        .status()?;
    anyhow::ensure!(status.success(), "touch command failed");

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "max(extension:txt & mtime:) == *-02-*",
        Default::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(
        res.results[0].raw_repr(),
        "TRUE",
        "2月の mtime を持つ場合は真になる（文脈C）"
    );

    Ok(())
}

#[test]
fn test_aggregation_comparison_slot_glob_false() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let file_path = root.join("test.txt");
    std::fs::write(&file_path, "content")?;
    let status = std::process::Command::new("touch")
        .args(["-d", "2026-05-15 10:00:00", file_path.to_str().unwrap()])
        .status()?;
    anyhow::ensure!(status.success(), "touch command failed");

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "max(extension:txt & mtime:) == *-02-*",
        Default::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(
        res.results[0].raw_repr(),
        "FALSE",
        "2月以外の mtime しか無い場合は偽になる（文脈C）"
    );

    Ok(())
}

#[test]
fn test_aggregation_comparison_string_rhs_not_collapsed_to_zero(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    std::fs::write(root.join("alpha.txt"), "content")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "max(name:) == alpha.txt",
        Default::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(
        res.results[0].raw_repr(),
        "TRUE",
        "文字列 RHS が as_i64() で 0 に潰れて誤って FALSE にならない（G16）"
    );

    Ok(())
}

#[test]
fn test_size_partial_digit_glob_returns_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    std::fs::write(root.join("a.txt"), "content")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "size:1* & is_dir:false",
        Default::default(),
    );

    assert!(
        res.is_err(),
        "size:1* is an unparseable partial digit glob and must error, not return 0 results: {:?}",
        res.map(|r| r.results.len())
    );

    Ok(())
}

#[test]
fn test_mtime_partial_digit_glob_returns_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    std::fs::write(root.join("a.txt"), "content")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "mtime:20* & is_dir:false",
        Default::default(),
    );

    assert!(
        res.is_err(),
        "mtime:20* is an unparseable partial digit glob and must error, not return 0 results: {:?}",
        res.map(|r| r.results.len())
    );

    Ok(())
}

#[test]
fn test_item_id_unknown_origin_glob_returns_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    std::fs::write(root.join("a.txt"), "content")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = ttfm::search::search_nowarn(
        &store,
        &registry,
        "item_id:\"Xyz(*)\"",
        Default::default(),
    );

    assert!(
        res.is_err(),
        "item_id:Xyz(*) names an unknown origin and must error, not return 0 results: {:?}",
        res.map(|r| r.results.len())
    );

    Ok(())
}

