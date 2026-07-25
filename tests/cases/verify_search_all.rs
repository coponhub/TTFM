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

use std::fs::File;
use tempfile::tempdir;
use ttfm::search;
use ttfm::SearchOptions;

#[test]
fn test_search_all_no_paging() -> anyhow::Result<()> {
    // Override TTFM_HOME to point to temp dir
    let dir = tempdir()?;
    let root = dir.path();
    unsafe {
        std::env::set_var("TTFM_HOME", root.join(".ttfm"));
    }

    let db_dir = root.join(".ttfm/db");

    // Create 25 files
    for i in 0..25 {
        File::create(root.join(format!("file_{:03}.txt", i)))?;
    }

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let (store, registry) = (db_dir_store, db_dir_registry);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    // Search all (n=None) -> Should retrieve all 25 items without paging
    let options = SearchOptions {
        n: None,
        ..Default::default()
    };
    // Query must match all files
    let res = search::search_nowarn(&store, &registry, "extension:txt", options)?;

    assert_eq!(res.results.len(), 25, "Should retrieve all 25 items");
    assert!(!res.has_more, "Should not have more results when n is None");

    Ok(())
}
