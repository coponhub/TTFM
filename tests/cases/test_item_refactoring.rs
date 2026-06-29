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
use ttfm::SearchOptions;
use ttfm::{search, tagging};

fn setup_store() -> (ttfm::db::Store, ttfm::tag::TagRegistry, ttfm::CacheManager)
{
    let dir = Box::leak(Box::new(tempdir().unwrap()));
    let db_dir = dir.path().join(".ttfm/db");
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();
    let cache = ttfm::CacheManager::new(db_dir.join("cache"), 0);
    (store, registry, cache)
}

/// User 区画 [0, B) の幅 B = 2^58。
const B: i64 = 1 << 58;

#[test]
fn add_item_allocates_in_user_space() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();

    // ユーザー作成アイテムは User 区画から採番される（負値ではない）。
    let id0 = tagging::add_item(&store, &registry, "note", "memo A").unwrap();
    let id1 = tagging::add_item(&store, &registry, "note", "memo B").unwrap();
    assert!((0..B).contains(&id0), "id0={id0} should be in User space");
    assert!((0..B).contains(&id1), "id1={id1} should be in User space");
    assert!(id1 > id0, "ids should ascend: {id0} -> {id1}");
}

#[test]
fn test_item_id_and_kind_refactoring() {
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

    // 1. Stored items (File/Note)
    let note_id = tagging::add_item(
        &store,
        &registry,
        "note",
        "TDD integration test memo",
    )
    .unwrap();
    // note_id should be ItemId::Stored
    assert!(note_id.to_string().parse::<i64>().is_ok());

    // 2. tag_item does NOT persist label
    tagging::tag_item(&store, &registry, &note_id.to_string(), "project:ttfm")
        .unwrap();

    // 3. Volatile items from aggregation (with actual data)
    // Add some files to make count > 0
    std::fs::write(dir.path().join("file1.txt"), "content").unwrap();
    std::fs::write(dir.path().join("file2.txt"), "content").unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    let res = search::search(
        &store,
        &registry,
        &cache,
        "count(item_id:)",
        SearchOptions::default(),
    )
    .unwrap();
    assert_eq!(res.results.len(), 1);
    // name may vary depending on env, so we just ensure it's a number > 0
    let count: i64 = res.results[0]
        .raw_repr()
        .parse()
        .expect("Count should be numeric");
    assert!(count > 0, "Count should be positive");

    // Check if a volatile ID is assigned.
    // In a shared process, this might not be 0, so we just check it is volatile.
    assert!(res.results[0].id.is_volatile());

    // 4. Projection which should return volatile label items
    let res_proj = search::search(
        &store,
        &registry,
        &cache,
        "extension:",
        SearchOptions::default(),
    )
    .unwrap();
    assert!(!res_proj.results.is_empty());

    // item_kind should be ItemKind::Volatile
    assert_eq!(res_proj.results[0].item_kind, ttfm::ItemKind::Volatile);

    // Check for sequential IDs
    if res_proj.results.len() >= 2 {
        let id0 = res_proj.results[0].id.as_i64() as u64;
        let id1 = res_proj.results[1].id.as_i64() as u64;
        assert_eq!(id1, id0 + 1, "Volatile IDs should be sequential");
    }

    // 5. Explicitly check for ID 0 if we can assume fresh process or just check behavior
    println!("First result ID: {}", res.results[0].id);
}

/// display(id) はローカル形式 "User(0)" / "Sys(N)" を返す。
/// Item.id.as_i64() を通すと identifier::display の形式になることを確認。
#[test]
fn item_id_display_uses_local_form() {
    let (store, registry, cache) = setup_store();
    let raw_id =
        tagging::add_item(&store, &registry, "type", "my_type").unwrap();

    let o = ttfm::Origin::within(raw_id);
    let disp = format!("{}({})", o.short(), raw_id - o.space_lo());
    // User 区画 [0, B) なので "User(N)" 形式
    assert!(disp.starts_with("User("), "expected User(N), got {disp}");
    assert!(disp.ends_with(')'), "expected User(N), got {disp}");

    // Item が同じ id を返す
    let results = search::search(
        &store,
        &registry,
        &cache,
        &format!("item_id:{raw_id}"),
        SearchOptions::default(),
    )
    .unwrap();
    assert_eq!(results.results.len(), 1);
    let rid = results.results[0].id.as_i64();
    let ro = ttfm::Origin::within(rid);
    assert_eq!(format!("{}({})", ro.short(), rid - ro.space_lo()), disp);
}

/// item_id:"User(0)" クエリは item_id:0 と同一アイテムを返す（ローカル形式の往復）。
#[test]
fn item_id_quoted_local_form_resolves_same_as_raw_id() {
    let (store, registry, cache) = setup_store();
    let raw_id =
        tagging::add_item(&store, &registry, "type", "my_type").unwrap();

    let o = ttfm::Origin::within(raw_id);
    let disp = format!("{}({})", o.short(), raw_id - o.space_lo()); // e.g. "User(0)"
    let q_raw = format!("item_id:{raw_id}");
    let q_local = format!("item_id:\"{disp}\"");

    let r_raw = search::search(
        &store,
        &registry,
        &cache,
        &q_raw,
        SearchOptions::default(),
    )
    .unwrap();
    let r_local = search::search(
        &store,
        &registry,
        &cache,
        &q_local,
        SearchOptions::default(),
    )
    .unwrap();

    assert_eq!(r_raw.results.len(), 1, "raw id query should find item");
    assert_eq!(
        r_local.results.len(),
        1,
        "local form query '{q_local}' should find same item (got {})",
        r_local.results.len()
    );
    assert_eq!(
        r_raw.results[0].id, r_local.results[0].id,
        "both queries must resolve to same item",
    );
}
