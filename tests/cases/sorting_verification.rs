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

use tempfile::tempdir;
use ttfm::db::TargetTable;

#[test]
fn test_parquet_physical_order() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // Create files ensures correct sorting check
    // 1. a.rs -> extension:rs
    // 2. z.txt -> extension:txt
    // type 'extension' is same. label_str 'rs' < 'txt'.
    std::fs::write(root.join("z.txt"), "content").unwrap();
    std::fs::write(root.join("a.rs"), "content").unwrap();

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let db_dir_cache =
        ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, _cache) =
        (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    // Verify base_tags.parquet order
    let path = store.path_for_target(TargetTable::BaseTags);

    // Read raw rows without ORDER BY
    // DuckDB read_parquet typically follows physical order.
    // We extract type and label_str.
    let rows: Vec<(String, Option<String>)> = store
        .conn
        .prepare(&format!(
            "SELECT type, label_str FROM read_parquet('{}')",
            path.to_string_lossy()
        ))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(!rows.is_empty(), "Should have base tags");

    // Check if sorted manually
    let mut is_sorted = true;
    for i in 0..rows.len() - 1 {
        let current = &rows[i];
        let next = &rows[i + 1];
        if current > next {
            is_sorted = false;
            println!("Unsorted at index {}: {:?} > {:?}", i, current, next);
            break;
        }
    }

    assert!(
        is_sorted,
        "Physical order in BaseTags parquet must be sorted: {:?}",
        rows
    );
}
