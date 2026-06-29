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
use ttfm::search;

#[test]
fn test_binder_error() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let db_dir = dir.path().join(".ttfm/db");

    // Create test files
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.rs"), "a")?;
    std::fs::write(src_dir.join("b.txt"), "b")?;

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()?;
    let db_dir_cache =
        ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) =
        (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        dir.path(),
        None::<&fn(usize)>,
        false,
    )?;

    let q = r#"((parentdir: &: count(extension:rs)) / (parentdir: &: count())) :> 1"#;
    match search::search(&store, &registry, &cache, q, Default::default()) {
        Ok(res) => eprintln!("SUCCESS: {:?}", res),
        Err(e) => {
            eprintln!("ERROR: {}", e);
            panic!("Search failed");
        }
    }
    Ok(())
}
