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

use std::fs::File;
use std::os::unix::fs::symlink;
use tempfile::tempdir;
use ttfm::search;

#[test]
#[cfg(unix)]
fn test_metadata_error_recovery_integration() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("db");

    // 1. 正常なファイルと、エラーになるリンクを作成
    let normal_file = dir.path().join("normal.txt");
    File::create(&normal_file).unwrap();

    let loop_link = dir.path().join("loop_link");
    // 自分自身を指すループリンク (ELOOPエラーを誘発)
    symlink(&loop_link, &loop_link).expect("Failed to create loop link");

    // 2. インデックス作成
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry)
        .initialize_tables()
        .unwrap();
    let (store, registry) = (db_dir_store, db_dir_registry);
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    // 3. エラー値がセットされたアイテムを検索して検証
    // 数値型のエラー値 (-1) で検索
    let results =
        search::search(&store, &registry, "size:-1", Default::default())
            .expect("Search for size:-1 should succeed");

    // 検証: loop_link がエラー値で登録されてヒットするはず
    assert_eq!(
        results.results.len(),
        1,
        "Should find exactly one file with metadata error"
    );
    assert!(results.results[0]
        .primary_value()
        .unwrap()
        .contains("loop_link"));
}
