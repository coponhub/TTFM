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

use anyhow::Result;
use tempfile::tempdir;
use ttfm::search;

#[test]
fn test_strict_grammar_scalar_comparison_error() -> Result<()> {
    // Tests that "size: > 100" (Projection + Scalar Operator) returns a FRIENDLY error message
    // even though it is technically a parse error in strict grammar.
    // (This works via error post-processing)

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let (store, registry) = (db_dir_store, db_dir_registry);
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    // This implies "Projection(size:) > Scalar(100)" which is invalid logic but valid loose grammar.
    // In strict grammar, this is a Parse Error.
    // Our post-processor should catch it.
    let res = search::search_nowarn(
        &store,
        &registry,
        "size: > 100",
        Default::default(),
    );

    if res.is_ok() {
        // Fail the test if it unexpectedly succeeds (Loose grammar state)
        panic!("Expected error, but search succeeded. Grammar is too loose.");
    }

    let err_msg = res.unwrap_err().to_string();
    println!("Actual error: {}", err_msg);

    // Check if the friendly message is present
    assert!(
        err_msg.contains("Scalar comparison cannot be applied to a Projection"),
        "Expected friendly error message, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("Did you mean: 'size: :> 100'"),
        "Expected suggestion in error message, got: {}",
        err_msg
    );
    // User requested to keep the pretty error formatting (checking for arrow)
    assert!(
        err_msg.contains("-->"),
        "Expected pretty printed error location (-->), got: {}",
        err_msg
    );

    Ok(())
}

#[test]
fn test_strict_grammar_space_requirement() -> Result<()> {
    // Tests that "1 >1" (missing space) is a Parse Error

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let (store, registry) = (db_dir_store, db_dir_registry);

    let res =
        search::search_nowarn(&store, &registry, "1 >1", Default::default());

    if res.is_ok() {
        panic!("Expected error for '1 >1', but it succeeded. Grammar is too loose.");
    }

    assert!(res.is_err());
    // Should be a normal parse error, NOT the friendly one
    let err_msg = res.unwrap_err().to_string();
    println!("Actual error: {}", err_msg);
    assert!(
        err_msg.contains("Parse error")
            || err_msg.contains("Unsupported comparison pattern")
            || err_msg.contains("-->"),
        "Expected Parse error or Unsupported comparison pattern, got: {}",
        err_msg
    );

    Ok(())
}

#[test]
fn test_strict_grammar_invalid_stuck_op() -> Result<()> {
    // Tests that "size:^=100" (invalid stuck operator) is a Parse Error
    // (^= is removed from stuck ops in cycle 1)

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let (store, registry) = (db_dir_store, db_dir_registry);

    let res = search::search_nowarn(
        &store,
        &registry,
        "size:^=100",
        Default::default(),
    );
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    println!("Actual error: {}", err_msg);
    // Pest standard error format contains "-->" pointing to line:col
    assert!(
        err_msg.contains("-->") || err_msg.contains("Parse error"),
        "Expected standard parse error format"
    );

    Ok(())
}

#[test]
fn test_scalar_comparison_rejects_projection_calculation() -> Result<()> {
    // Tests that "(mtime: / 100) < 100" (scalar comparison with projection calculation) is rejected
    // This is now a parse error because scalar_operand only allows scalar_calculation (no type_ref)
    // While "(mtime: / 100) :< 100" (label comparison) should work fine.

    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let (store, registry) = (db_dir_store, db_dir_registry);
    ttfm::indexing::Indexer::new(&store, &registry).run_single(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    // Scalar comparison with projection in calculation should fail
    let res = search::search_nowarn(
        &store,
        &registry,
        "(mtime: / 100) < 100",
        Default::default(),
    );
    assert!(
        res.is_err(),
        "Expected error for '(mtime: / 100) < 100' (projection in scalar comparison)"
    );
    let err_msg = res.unwrap_err().to_string();
    println!("Scalar comparison error: {}", err_msg);
    // Should produce a parse error (not pass through)
    assert!(
        err_msg.contains("-->") || err_msg.contains("Parse error"),
        "Expected parse error for projection in scalar comparison, got: {}",
        err_msg
    );

    // Label comparison with projection calculation should work
    let res2 = search::search_nowarn(
        &store,
        &registry,
        "(size: / 1024) :> 100",
        Default::default(),
    );
    assert!(
        res2.is_ok(),
        "Expected '(size: / 1024) :> 100' to succeed as label comparison, got: {:?}",
        res2.err()
    );

    // Bare arithmetic with aggregation
    let res3 = search::search_nowarn(
        &store,
        &registry,
        "count(extension:rs) + count(extension:c)",
        Default::default(),
    );
    assert!(
        res3.is_ok(),
        "Expected 'count(extension:rs) + count(extension:c)' to succeed, got: {:?}",
        res3.err()
    );

    Ok(())
}
