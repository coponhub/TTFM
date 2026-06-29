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

use std::fs;
use tempfile::tempdir;
use ttfm::types::ItemId;
use ttfm::{search, tagging};

#[test]
fn test_rank_sorting_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 1. ファイルを3つ作成
    fs::write(root.join("low.txt"), "low").unwrap();
    fs::write(root.join("high.txt"), "high").unwrap();
    fs::write(root.join("mid.txt"), "mid").unwrap();

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let db_dir_cache =
        ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) =
        (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    // 2. クエリでランクを設定
    // high.txt を 100 に
    let res_high = search::search(
        &store,
        &registry,
        &cache,
        "filename:high.txt",
        Default::default(),
    )
    .unwrap();
    ttfm::rank::update_ranks(&store, &registry, &res_high.results, 100)
        .unwrap();

    // mid.txt を 50 に
    let res_mid = search::search(
        &store,
        &registry,
        &cache,
        "filename:mid.txt",
        Default::default(),
    )
    .unwrap();
    ttfm::rank::update_ranks(&store, &registry, &res_mid.results, 50).unwrap();

    // 3. 検索して順序を確認
    let results = search::search(
        &store,
        &registry,
        &cache,
        "extension:txt",
        Default::default(),
    )
    .unwrap();
    assert_eq!(results.results.len(), 3);

    // 順序: high (100) -> mid (50) -> low (0)
    assert!(results.results[0]
        .primary_value()
        .unwrap()
        .contains("high.txt"));
    assert!(results.results[1]
        .primary_value()
        .unwrap()
        .contains("mid.txt"));
    assert!(results.results[2]
        .primary_value()
        .unwrap()
        .contains("low.txt"));
}

#[test]
fn test_rank_batch_update() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    fs::write(root.join("a.txt"), "a").unwrap();
    fs::write(root.join("b.txt"), "b").unwrap();
    fs::write(root.join("c.rs"), "c").unwrap();

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let db_dir_cache =
        ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) =
        (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    // 1. *.txt のランクを一括で 10 に設定
    let results = search::search(
        &store,
        &registry,
        &cache,
        "extension:txt",
        Default::default(),
    )
    .unwrap();
    assert_eq!(results.results.len(), 2);
    ttfm::rank::update_ranks(&store, &registry, &results.results, 10).unwrap();

    // 2. 結果を確認
    let res = search::search(
        &store,
        &registry,
        &cache,
        "extension:txt | extension:rs",
        Default::default(),
    )
    .unwrap();
    // ランク順に a.txt(10), b.txt(10), c.rs(0) のはず
    assert_eq!(res.results.len(), 3);
    assert!(res.results[0].primary_value().unwrap().contains(".txt"));
    assert!(res.results[1].primary_value().unwrap().contains(".txt"));
    assert!(res.results[2].primary_value().unwrap().contains(".rs"));
}

#[test]
fn test_rank_set_by_id_low_level() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let db_dir_cache =
        ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) =
        (db_dir_store, db_dir_registry, db_dir_cache);

    fs::create_dir_all(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    let id = tagging::add_item(&store, &registry, "note", "test note").unwrap();
    ttfm::rank::set_rank_by_id(&store, &registry, id, false, 500).unwrap();

    let results = search::search(
        &store,
        &registry,
        &cache,
        "item_kind:note",
        Default::default(),
    )
    .unwrap();
    assert_eq!(results.results[0].id, ItemId::from(id));
    // ランクに基づいたソートが効いているか（他にアイテムがあればより明確）
}
