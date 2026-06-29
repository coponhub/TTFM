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

/// 集約機能 (Aggregation) の統合テスト
use super::inject_path_scope;
use path_slash::PathExt;
use tempfile::tempdir;
use ttfm::search;

define_cases! {
    count_items_txt: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.rs"), "c")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(extension:txt)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].raw_repr(), "2");
            Ok(())
        },
    },
    count_unique_labels: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            std::fs::write(dir.join("c.rs"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(extension:)",
        assert: |res, _dir| {
            assert_eq!(res.results[0].raw_repr(), "2");
            Ok(())
        },
    },
    sum_projection_txt: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 100])?;
            std::fs::write(dir.join("large.txt"), vec![0u8; 1000])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:txt & size:)",
        assert: |res, _dir| {
            assert_eq!(res.results[0].raw_repr(), "1.1KB");
            Ok(())
        },
    },
    agg_comparison_true: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 100])?;
            std::fs::write(dir.join("large.txt"), vec![0u8; 1000])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size:) > 500",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            assert!(res.results[0].id.is_volatile());
            assert_eq!(res.results[0].item_kind, ttfm::ItemKind::Volatile);
            Ok(())
        },
    },
    agg_comparison_false: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size:) > 1000",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            assert_eq!(res.results[0].raw_repr(), "FALSE");
            assert!(res.results[0].id.is_volatile());
            assert_eq!(res.results[0].item_kind, ttfm::ItemKind::Volatile);
            Ok(())
        },
    },
    count_type_projection: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), "content")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(type:)",
        assert: |res, _dir| {
            let val: f64 = res.results[0].raw_repr().parse()?;
            assert!(val > 1.0, "Expected multiple types, got {}", val);
            Ok(())
        },
    },
    count_directory_projection: {
        setup: |dir| {
            std::fs::create_dir(dir.join("subdir"))?;
            std::fs::write(dir.join("test.txt"), "content")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(directory:)",
        assert: |res, _dir| {
            let val: f64 = res.results[0].raw_repr().parse()?;
            assert!(val >= 1.0, "Expected at least 1 directory, got {}", val);
            Ok(())
        },
    },
    max_on_empty: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: inject_path_scope,
        query: "max(nonexistent_tag:)",
        assert: |res, _dir| {
            let r = res.results.first().expect("Expected a result for MAX aggregation");
            let types = r.get_all_values("type");
            assert!(types.contains(&"numeric".to_string()), "Expected type:numeric, got {:?}", types);
            Ok(())
        },
    },
    agg_agg_reflexive: {
        setup: |dir| {
            std::fs::write(dir.join("empty.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(size:0) == sum(size:0)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Result should not be empty");
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            assert!(res.results[0].id.is_volatile());
            Ok(())
        },
    },
    string_agg_extension: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "")?;
            std::fs::write(dir.join("b.rs"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            let val = &res.results[0].raw_repr();
            assert!(val.contains("txt") && val.contains("rs") && val.contains(", "), "Unexpected sum(extension:): {}", val);
            Ok(())
        },
    },
    string_agg_with_filter: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "")?;
            std::fs::write(dir.join("b.rs"), "")?;
            std::fs::write(dir.join("c.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:rs & extension:)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Result should not be empty");
            let name = &res.results[0].raw_repr();
            assert!(name.contains("rs"), "Result should contain 'rs', but got: {}", name);
            assert!(name.contains(","), "Result should be a joined string, but got: {}", name);
            Ok(())
        },
    },
    string_agg_arithmetic_addition: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:rs & extension:)",
        assert: |res, _dir| {
            // Just verify rs files can be aggregated (arithmetic test is in standalone)
            assert!(!res.results.is_empty(), "Result should not be empty");
            Ok(())
        },
    },
    count_empty_gt: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            std::fs::write(dir.join("c.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count() > 2",
        assert: |res, _dir| {
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            Ok(())
        },
    },
    count_wildcard_gt: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            std::fs::write(dir.join("c.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(*:*) > 2",
        assert: |res, _dir| {
            assert_eq!(res.results[0].raw_repr(), "TRUE");
            Ok(())
        },
    },
    count_empty_arithmetic: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count() + 1",
        assert: |res, _dir| {
            assert!(res.results[0].raw_repr().parse::<i64>().is_ok(), "Result should be a valid integer");
            Ok(())
        },
    },
    count_empty_vs_scalar: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count() == count(type:)",
        assert: |res, _dir| {
            assert!(res.results[0].raw_repr() == "TRUE" || res.results[0].raw_repr() == "FALSE");
            Ok(())
        },
    },
}

/// 集約比較 (!=): count(extension:txt) ^ 0/2
#[test]
fn test_aggregation_comparison_ne() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.txt"), "a")?;
    std::fs::write(root.join("b.txt"), "b")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res1 = search::search(
        &store,
        &registry,
        &cache,
        "count(extension:txt) ^ 0",
        Default::default(),
    )?;
    assert_eq!(
        res1.total_count,
        Some(1),
        "Should match root directory (calc is true)"
    );

    let res2 = search::search(
        &store,
        &registry,
        &cache,
        "count(extension:txt) ^ 2",
        Default::default(),
    )?;
    assert_eq!(res2.results[0].raw_repr(), "FALSE");

    Ok(())
}

/// その他のシステムカラムの集約テスト
#[test]
fn test_system_columns_aggregation() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("test.txt"), "content")?;
    std::fs::create_dir(root.join("sub"))?;
    std::fs::write(root.join("sub/test2.txt"), "content2")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = search::search(
        &store,
        &registry,
        &cache,
        "count(item_id:)",
        Default::default(),
    )?;
    let val: f64 = res.results[0].raw_repr().parse().unwrap();
    assert!(val >= 3.0, "count(item_id) failed: {}", val);

    let res = search::search(
        &store,
        &registry,
        &cache,
        "count(item_kind:)",
        Default::default(),
    )?;
    let val: f64 = res.results[0].raw_repr().parse().unwrap();
    assert!(val >= 1.0, "count(item_kind) failed: {}", val);

    let res = search::search(
        &store,
        &registry,
        &cache,
        "count(rank:)",
        Default::default(),
    )?;
    let val: f64 = res.results[0].raw_repr().parse().unwrap();
    assert!(val >= 1.0, "count(rank) failed: {}", val);

    let res = search::search(
        &store,
        &registry,
        &cache,
        "count(origin:)",
        Default::default(),
    )?;
    let val: f64 = res.results[0].raw_repr().parse().unwrap();
    assert!(val >= 1.0, "count(origin) failed: {}", val);

    let res = search::search(
        &store,
        &registry,
        &cache,
        "count(path:)",
        Default::default(),
    )?;
    let val: f64 = res.results[0].raw_repr().parse().unwrap();
    assert!(val >= 3.0, "count(path) failed: {}", val);

    let res = search::search(
        &store,
        &registry,
        &cache,
        "count(parentdir:)",
        Default::default(),
    )?;
    let val: f64 = res.results[0].raw_repr().parse().unwrap();
    assert!(val >= 1.0, "count(parentdir) failed: {}", val);

    let res = search::search(
        &store,
        &registry,
        &cache,
        "count(filename:)",
        Default::default(),
    )?;
    let val: f64 = res.results[0].raw_repr().parse().unwrap();
    assert!(val >= 1.0, "count(filename) failed: {}", val);

    Ok(())
}

/// max(mtime:) と日付文字列の比較
#[test]
fn test_max_mtime_date_comparison() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("old.txt"), "old")?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    std::fs::write(root.join("new.txt"), "new")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res2 = search::search(
        &store,
        &registry,
        &cache,
        "max(mtime:) < 2027-01-01",
        Default::default(),
    )?;
    assert_eq!(res2.results.len(), 1);
    assert_eq!(res2.results[0].raw_repr(), "TRUE");

    Ok(())
}

/// max(filter & mtime:) と日付文字列の比較（AND条件内のProjection）
#[test]
fn test_max_mtime_with_filter_date_comparison() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("test.txt"), "content")?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    std::fs::write(root.join("test.rs"), "code")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = search::search(
        &store,
        &registry,
        &cache,
        "max(extension:txt & mtime:) < 2027-02-01",
        Default::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].raw_repr(), "TRUE");

    Ok(())
}

/// max(mtime:) == YYYY (Equal Comparison with Date Expansion)
#[test]
fn test_aggregation_comparison_date_equal() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("test.txt"), "content")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = search::search(
        &store,
        &registry,
        &cache,
        "max(mtime:) == 2026",
        Default::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].raw_repr(), "TRUE");
    assert!(res.results[0].id.is_volatile());

    let res_false = search::search(
        &store,
        &registry,
        &cache,
        "max(mtime:) == 2025",
        Default::default(),
    )?;
    assert_eq!(res_false.results.len(), 1);
    assert_eq!(res_false.results[0].raw_repr(), "FALSE");
    assert!(res_false.results[0].id.is_volatile());

    Ok(())
}

struct TestContext {
    _dir: tempfile::TempDir,
    db_dir: std::path::PathBuf,
    root: std::path::PathBuf,
}

impl TestContext {
    fn new() -> Self {
        let _dir = tempdir().unwrap();
        let root = _dir.path().to_path_buf();
        let db_dir = _dir.path().join("db");
        std::fs::create_dir(&db_dir).unwrap();
        Self { _dir, db_dir, root }
    }

    fn create_file_with_mtime(&self, name: &str, mtime_iso: &str) {
        let path = self.root.join(name);
        std::fs::File::create(&path).unwrap();
        let dt = chrono::DateTime::parse_from_rfc3339(mtime_iso).unwrap();
        let mtime = filetime::FileTime::from_unix_time(dt.timestamp(), 0);
        filetime::set_file_mtime(&path, mtime).unwrap();
    }

    fn search(&self, query: &str) -> ttfm::response::SearchResponse {
        let registry = ttfm::tag::TagRegistry::with_standard();
        let store = ttfm::db::Store::open(&self.db_dir).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();
        let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
        ttfm::indexing::Indexer::new(&store, &registry)
            .run(&self.root, None::<&fn(usize)>, false)
            .unwrap();
        search::search(
            &store,
            &registry,
            &cache,
            query,
            ttfm::SearchOptions::default(),
        )
        .unwrap()
    }
}

#[test]
fn test_max_mtime_with_year_filter() {
    let context = TestContext::new();
    context.create_file_with_mtime("a.rs", "2025-06-15T12:00:00Z");
    context.create_file_with_mtime("b.rs", "2024-12-31T23:59:59Z");
    context.create_file_with_mtime("c.txt", "2025-01-01T00:00:00Z");

    let res = context.search("max(extension:rs & mtime:2025 & mtime:)");
    assert!(!res.results.is_empty());
    let value_strs = res.results[0].get_all_values("value");
    let scalar: f64 = value_strs[0].parse().unwrap();
    assert!(scalar > 1700000000.0);
}

/// string_agg_arithmetic_addition — 文字列集計結果同士の加算
#[test]
fn test_string_agg_arithmetic_addition() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.rs"), "")?;
    std::fs::write(root.join("b.txt"), "")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res = search::search(&store, &registry, &cache,
        "sum(extension:rs & extension:) + ' - ' + sum(extension:txt & extension:)",
        Default::default(),
    )?;

    assert!(!res.results.is_empty(), "Result should not be empty");
    let name = &res.results[0].raw_repr();
    assert!(
        name.contains("rs"),
        "Result should contain 'rs', but got: {}",
        name
    );
    assert!(
        name.contains("txt"),
        "Result should contain 'txt', but got: {}",
        name
    );
    assert!(
        name.contains(" - "),
        "Result should contain separator ' - ', but got: {}",
        name
    );

    Ok(())
}

/// count() と count(*:*) の一致確認、count() - count(*:* - parentdir:"...") の検証
#[test]
fn test_count_empty_args() -> anyhow::Result<()> {
    let temp_parent = tempdir()?;
    let root = temp_parent.path().join("root");
    let db_dir = temp_parent.path().join("db");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&db_dir)?;

    std::fs::write(root.join("a.rs"), "")?;
    std::fs::write(root.join("b.rs"), "")?;
    std::fs::write(root.join("c.txt"), "")?;
    std::fs::create_dir(root.join("subdir1"))?;
    std::fs::create_dir(root.join("subdir2"))?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        &root,
        None::<&fn(usize)>,
        false,
    )?;

    let res_any_top = search::search(
        &store,
        &registry,
        &cache,
        "count()",
        Default::default(),
    )?;
    let res_wild_top = search::search(
        &store,
        &registry,
        &cache,
        "count(*:*)",
        Default::default(),
    )?;
    assert_eq!(
        res_any_top.results[0].raw_repr(),
        res_wild_top.results[0].raw_repr()
    );

    let root_str = root.to_slash_lossy();
    let query_indirect =
        format!("count() - count(*:* - parentdir:\"{}\")", root_str);
    let res_indirect = search::search(
        &store,
        &registry,
        &cache,
        &query_indirect,
        Default::default(),
    )?;
    assert_eq!(res_indirect.results[0].raw_repr(), "5");

    Ok(())
}
