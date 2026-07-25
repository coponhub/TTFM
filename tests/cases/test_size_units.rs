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
use std::fs::File;
use std::io::Write;
use tempfile::tempdir;
use ttfm::search;

define_cases! {
    size_large_gt_1pb: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: default_scope,
        query: "size: :> 1PB",
        assert: |_res, _dir| Ok(()),
    },
    size_large_eq_1tb: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: default_scope,
        query: "size: := 1TB",
        assert: |_res, _dir| Ok(()),
    },
}

#[test]
fn test_size_unit_queries() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let mut f1 = File::create(root.join("half_mb.bin"))?;
    f1.write_all(&vec![0u8; 512 * 1024])?;

    let mut f2 = File::create(root.join("one_and_half_mb.bin"))?;
    f2.write_all(&vec![0u8; (1.5 * 1024.0 * 1024.0) as usize])?;

    let mut f3 = File::create(root.join("ten_mb.bin"))?;
    f3.write_all(&vec![0u8; 10 * 1024 * 1024])?;

    File::create(root.join("empty.txt"))?;

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

    let cases = vec![
        ("size: :>= 512KB & is_dir:false", 3),
        ("size: :< 1MB & is_dir:false", 2),
        ("size: := 512KiB", 1),
        ("size: :<= 1.5MB & is_dir:false", 3),
        ("size: :> 1.5MiB & is_dir:false", 1),
        ("size: :> 600KB & size: :< 11MB & is_dir:false", 2),
        ("size: :>= 1MB & size: :<= 1.5MB & is_dir:false", 1),
        ("size: :^= 0B & is_dir:false", 3),
        ("size: := 10m & is_dir:false", 1),
        ("size: := 512k & is_dir:false", 1),
    ];

    for (query, expected) in cases {
        let results =
            search::search_nowarn(&store, &registry, query, Default::default())?;
        assert_eq!(
            results.results.len(),
            expected,
            "Query '{}' failed. Expected {}, got {}",
            query,
            expected,
            results.results.len()
        );
    }

    Ok(())
}
