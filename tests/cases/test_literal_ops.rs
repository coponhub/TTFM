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

use tempfile::tempdir;
use ttfm::search;

#[test]
fn test_literal_arithmetic() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let data_dir = root.join("data");
    std::fs::create_dir(&data_dir)?;
    // Create a dummy file so we have items to match against
    std::fs::write(data_dir.join("test.txt"), "content")?;

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let db_dir_cache =
        ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) =
        (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        &data_dir,
        None::<&fn(usize)>,
        false,
    )?;

    // 1. Tag + Literal Arithmetic
    // size: + 1
    // The file size is 7 bytes ("content"). 7 + 1 = 8.
    // Grammar requires parens for top-level calculation: "(size: + 1)"
    let res_tag = search::search(
        &store,
        &registry,
        &cache,
        "(size: + 1)",
        Default::default(),
    )?;
    assert!(!res_tag.results.is_empty(), "size: + 1 should match");
    // Result name should be the calculated value
    assert_eq!(res_tag.results[0].raw_repr(), "8", "size: + 1 should be 8");

    // 2. Pure Literal Arithmetic
    // (1 + 2) - Parentheses are likely required for pure calculation to distinguish from other patterns?
    // Or maybe just "1 + 2" should work. The parser error suggests it expects structure.
    // Try "(1 + 2)" as per my implementation plan note.
    let res = search::search(
        &store,
        &registry,
        &cache,
        "(1 + 2)",
        Default::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].raw_repr(), "3");

    // 3. String Arithmetic
    // ('a' + 'b') -> "a, b"
    let res = search::search(
        &store,
        &registry,
        &cache,
        "('a' + 'b')",
        Default::default(),
    )?;
    assert_eq!(res.results[0].raw_repr(), "a, b");

    // ('a' * 'b') -> "ab"
    let res = search::search(
        &store,
        &registry,
        &cache,
        "('a' * 'b')",
        Default::default(),
    )?;
    assert_eq!(res.results[0].raw_repr(), "ab");

    Ok(())
}

#[test]
fn test_literal_comparison() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let db_dir_cache =
        ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) =
        (db_dir_store, db_dir_registry, db_dir_cache);

    // 1. Integer Comparison (True)
    let res = search::search(
        &store,
        &registry,
        &cache,
        "10 > 2",
        Default::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].raw_repr(), "TRUE");

    // 2. Integer Comparison (False)
    let res =
        search::search(&store, &registry, &cache, "1 > 2", Default::default())?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].raw_repr(), "FALSE");

    // 3. String Comparison
    let res = search::search(
        &store,
        &registry,
        &cache,
        "'b' > 'a'",
        Default::default(),
    )?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].raw_repr(), "TRUE");

    Ok(())
}

#[test]
fn test_literal_set_operation_error() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let db_dir_cache =
        ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) =
        (db_dir_store, db_dir_registry, db_dir_cache);

    // リテラル同士の集合演算はパーサーレベルで拒否される
    let res =
        search::search(&store, &registry, &cache, "1 & 2", Default::default());
    assert!(
        res.is_err(),
        "Set operation between literals should be an error"
    );

    Ok(())
}

#[test]
fn test_literal_string_error_cases() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);

    let cases = vec![
        // String同士の無効な算術演算
        ("('a' - 'b')", "Unsupported arithmetic"),
        ("('a' / 'b')", "Unsupported arithmetic"),
        // StringとString以外の型の算術演算
        ("('a' + 1)", "String and non-String"),
        ("(1 + 'a')", "String and non-String"),
    ];

    for (query, expected_err_part) in cases {
        let result = search::search(
            &store,
            &registry,
            &cache,
            query,
            Default::default(),
        );
        assert!(result.is_err(), "Query '{}' should fail", query);
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains(expected_err_part),
            "Query '{}' error message '{}' should contain '{}'",
            query,
            err_msg,
            expected_err_part
        );
    }
    Ok(())
}
