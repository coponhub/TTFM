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

//! `identifier::next` の DB 越し採番挙動の統合テスト。
//! （純関数 space/within/display/parse は src/db/identifier.rs の単体テスト側）

use tempfile::tempdir;
use ttfm::db::identifier::next;
use ttfm::db::{Store, TargetTable};
use ttfm::types::Origin;

/// 区画幅 B = 2^58。
const B: i64 = 1 << 58;

/// 指定 item_id 群を parquet に仕込んだ Store を作る。
/// TempDir も返す（drop で db_dir が消えるため保持が必要）。
fn store_with(rows: &[(TargetTable, &[i64])]) -> (Store, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("db");
    std::fs::create_dir_all(&db_dir).unwrap();
    let store = Store::open(&db_dir).unwrap();
    for (target, ids) in rows {
        let path = store.path_for_target(*target);
        store
            .conn
            .execute("CREATE TABLE tmp_ids (item_id BIGINT)", [])
            .unwrap();
        for id in *ids {
            store
                .conn
                .execute(&format!("INSERT INTO tmp_ids VALUES ({id})"), [])
                .unwrap();
        }
        store
            .conn
            .execute(
                &format!(
                    "COPY tmp_ids TO '{}' (FORMAT PARQUET)",
                    path.to_string_lossy()
                ),
                [],
            )
            .unwrap();
        store.conn.execute("DROP TABLE tmp_ids", []).unwrap();
    }
    (store, dir)
}

#[test]
fn next_empty_space_starts_at_lo() {
    let (store, _dir) = store_with(&[]);
    // parquet が無い → User 区画は lo=0 から連番。
    assert_eq!(next(&store, Origin::User, 3).unwrap(), vec![0, 1, 2]);
}

#[test]
fn next_user_continues_above_existing_max() {
    // User 区画 [0,B) に max=10 → 次は 11,12,13。
    let (store, _dir) = store_with(&[(TargetTable::ItemReferences, &[5, 10])]);
    assert_eq!(next(&store, Origin::User, 3).unwrap(), vec![11, 12, 13]);
}

#[test]
fn next_file_reads_file_references() {
    // File 区画 [8B, ...) — file_references の max=8B+5 → 次は 8B+6, 8B+7。
    let (store, _dir) =
        store_with(&[(TargetTable::FileReferences, &[8 * B + 5])]);
    assert_eq!(
        next(&store, Origin::File, 2).unwrap(),
        vec![8 * B + 6, 8 * B + 7]
    );
}

#[test]
fn next_builtin_reads_item_references() {
    // Builtin 区画 [-B, 0) — item_references の Builtin 行 max=-B+5 → 次は -B+6。
    let (store, _dir) = store_with(&[(TargetTable::ItemReferences, &[-B + 5])]);
    assert_eq!(next(&store, Origin::Builtin, 1).unwrap(), vec![-B + 6]);
}

#[test]
fn next_user_ignores_builtin_rows_in_item_references() {
    // item_references に Builtin(-B+5) / User(7) 混在。User 採番は User 区画のみ見る。
    let (store, _dir) =
        store_with(&[(TargetTable::ItemReferences, &[7, -B + 5])]);
    assert_eq!(next(&store, Origin::User, 1).unwrap(), vec![8]);
}
