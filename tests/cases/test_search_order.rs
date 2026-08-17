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

//! SearchOptions.order（検索結果の並び順の明示指定）の統合テスト。
//! 任意の型（TagType）をキーにでき、複数キーの組み合わせにも対応する。

use tempfile::TempDir;
use ttfm::{
    db::Store,
    indexing::Indexer,
    tag::TagRegistry,
    types::{Order, SType},
    SearchOptions,
};

/// 拡張子・サイズの異なるファイル群を索引した環境を作る。
/// サイズ: a.rs=100, b.rs=200, m.txt=150, y.txt=50, z.txt=300
fn setup() -> (Store, TagRegistry, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    for (name, size) in [
        ("a.rs", 100),
        ("b.rs", 200),
        ("m.txt", 150),
        ("y.txt", 50),
        ("z.txt", 300),
    ] {
        std::fs::write(root.join(name), "x".repeat(size)).unwrap();
    }

    let db_dir = base.join("db");
    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run_single(&root, None::<&fn(usize)>, false)
        .unwrap();
    (store, registry, dir)
}

fn search_with_order(
    store: &Store,
    registry: &TagRegistry,
    query: &str,
    order: Vec<Order>,
) -> Vec<String> {
    let results = ttfm::search::search_nowarn(
        store,
        registry,
        query,
        SearchOptions {
            order,
            ..SearchOptions::default()
        },
    )
    .unwrap();
    results.results.iter().map(|r| r.raw_repr()).collect()
}

// 明示した order は既定の並び（rank 降順）より優先される（定義アイテム経路）。
#[test]
fn order_by_name_asc_overrides_default_on_type_definitions() {
    let (store, registry, _dir) = setup();

    let names = search_with_order(
        &store,
        &registry,
        "type:*",
        vec![Order::asc(SType::Name)],
    );

    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        names, sorted,
        "type:* with order [name asc] must be sorted by name"
    );
}

// 任意の型（size）の値でアイテムをソートできる。
#[test]
fn order_by_size_desc_on_items() {
    let (store, registry, _dir) = setup();

    let names = search_with_order(
        &store,
        &registry,
        "extension:txt",
        vec![Order::desc(SType::Size)],
    );

    assert_eq!(
        names,
        vec!["z.txt", "m.txt", "y.txt"],
        "txt files must be sorted by size descending"
    );
}

// 複数キーの組み合わせ（extension 昇順 → size 降順）でソートできる。
#[test]
fn order_by_multiple_keys_on_items() {
    let (store, registry, _dir) = setup();

    let names = search_with_order(
        &store,
        &registry,
        "extension:*",
        vec![Order::asc(SType::Extension), Order::desc(SType::Size)],
    );

    assert_eq!(
        names,
        vec!["b.rs", "a.rs", "z.txt", "m.txt", "y.txt"],
        "items must be sorted by extension asc, then size desc"
    );
}
