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

use super::append_to_target;
use super::indexer::{ScanEntryLoader, TempScanEntry};
use crate::db::{Col, DuckDbFunc, Store, TargetTable, Tbl};
use crate::indexing::ScanEntry;
use crate::types::ItemId;
use crate::util::{self, ParquetExt};
use anyhow::Result;
use duckdb::Connection;
use path_slash::PathExt as _;
use sea_query::{
    Condition, Expr, Func, JoinType, PostgresQueryBuilder, Query,
    SelectStatement,
};
use std::path::{Path, PathBuf};

// ========================================================
// Diff Phase Orchestrator
// ========================================================

pub(crate) fn run_diff(
    conn: &Connection,
    store: &Store,
    roots: &[PathBuf],
) -> Result<IndexDiff> {
    let scan_path = store.temp_scan_path();
    rediscover(conn, store, &scan_path)?;

    let auditor = DiffAuditor::new(store, &scan_path);

    if auditor.is_initial() {
        return Ok(IndexDiff {
            to_process: auditor.query_all_scanned().fetch_with_ids(conn)?,
            ..Default::default()
        });
    }

    // 1. 今回スキャンされたエントリに対し、既存の Inode を持っていればその ID を特定
    let to_process = auditor.query_with_existing_ids().fetch_with_ids(conn)?;

    // 2. 削除対象の特定（生存リストにも、今回のスキャン結果にも載っていない既存 ID）
    let deleted_ids = auditor.query_deleted(roots).fetch_ids(conn)?;

    Ok(IndexDiff {
        to_process,
        deleted_ids,
    })
}

/// スキャン結果に removed_files と同じ basename_scan_hash（basename・mtime・
/// size・inode）が現れたら、その行を file_references へ復元し、removed_files
/// から取り除く。file_id は今回スキャンで割り当てられた新しい値を使う。OS が
/// inode を再利用しうるため、removed_files の古い file_id をそのまま使うと
/// 無関係な新規ファイルが誤って昔のタグを引き継いでしまう。
fn rediscover(
    conn: &Connection,
    store: &Store,
    scan_path: &Path,
) -> Result<()> {
    let removed_path = store
        .path_for_target(TargetTable::RemovedFiles)
        .to_string_lossy()
        .into_owned();

    if removed_files_is_empty(conn, &removed_path)? {
        return Ok(());
    }

    let hits = Query::select()
        .distinct()
        .column((Tbl::RemovedFiles, Col::ItemId))
        .column((Tbl::RemovedFiles, Col::Rank))
        .column((Tbl::Scan, Col::FileId))
        .columns([
            (Tbl::RemovedFiles, Col::IsDir),
            (Tbl::RemovedFiles, Col::Size),
            (Tbl::RemovedFiles, Col::Mtime),
        ])
        .from_subquery(util::parquet_query(&removed_path), Tbl::RemovedFiles)
        .join_subquery(
            JoinType::InnerJoin,
            util::parquet_query(&scan_path.to_string_lossy()),
            Tbl::Scan,
            Expr::col((Tbl::RemovedFiles, Col::BasenameScanHash))
                .eq(Expr::col((Tbl::Scan, Col::BasenameScanHash))),
        )
        .to_owned();

    append_to_target(conn, store, TargetTable::FileReferences, hits.clone())?;

    let restored = Query::select()
        .column(Col::ItemId)
        .from_subquery(hits, crate::db::Pronoun::Sub)
        .to_owned();

    Query::select()
        .column(sea_query::Asterisk)
        .from_subquery(util::parquet_query(&removed_path), Tbl::RemovedFiles)
        .and_where(Expr::col(Col::ItemId).not_in_subquery(restored))
        .to_owned()
        .save_parquet(conn, &store.path_for_target(TargetTable::RemovedFiles))
}

fn removed_files_is_empty(
    conn: &Connection,
    removed_path: &str,
) -> Result<bool> {
    let sql = Query::select()
        .expr(Expr::cust("COUNT(*)"))
        .from_subquery(util::parquet_query(removed_path), Tbl::RemovedFiles)
        .to_string(PostgresQueryBuilder);
    let count: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
    Ok(count == 0)
}

/// 指定された roots のいずれか（自身、またはその配下）に path が含まれるか判定する。
pub(crate) fn in_scope(col: Col, roots: &[PathBuf]) -> Condition {
    roots.iter().fold(Condition::any(), |cond, r| {
        let base = format!("{}/", r.to_slash_lossy());
        cond.add(Expr::col(col).eq(r.to_slash_lossy().to_string())).add(
            sea_query::SimpleExpr::from(
                Func::cust(DuckDbFunc::StartsWith)
                    .args([Expr::col(col).into(), Expr::val(base).into()]),
            ),
        )
    })
}

// ========================================================
// 1. Diff Auditor
// ========================================================

pub(crate) struct DiffAuditor {
    scan: String,
    live: String,
    ents: String,
    locs: String,
}

impl DiffAuditor {
    pub(crate) fn new(store: &Store, scan_path: &Path) -> Self {
        let path = |t| store.path_for_target(t).to_string_lossy().into_owned();
        Self {
            scan: scan_path.to_string_lossy().into_owned(),
            live: store.temp_live_path().to_string_lossy().into_owned(),
            ents: path(TargetTable::FileReferences),
            locs: path(TargetTable::Locations),
        }
    }

    pub(crate) fn is_initial(&self) -> bool {
        !Path::new(&self.ents).exists()
    }

    /// 今回スキャンされた全データを取得（ID未定として扱う）
    pub(crate) fn query_all_scanned(&self) -> SelectStatement {
        Query::select()
            .expr_as(Expr::cust("NULL"), Col::ItemId)
            .columns(
                TempScanEntry::columns_with_type()
                    .into_iter()
                    .map(|(c, _)| c),
            )
            .from_subquery(util::parquet_query(&self.scan), Tbl::Scan)
            .to_owned()
    }

    /// スキャン結果と既存 DB を Inode で JOIN し、既存 ID を特定するクエリ
    pub(crate) fn query_with_existing_ids(&self) -> SelectStatement {
        let col_file_id = util::col_to_iden(ScanEntry::schema()[1].name);

        // Inode ごとに最小 ID を 1つ選ぶユニーク名簿
        let distinct_ents = Query::select()
            .expr(crate::util::CustomExpr::distinct_on_all(Col::FileId))
            .from_subquery(util::parquet_query(&self.ents), Tbl::FileReferences)
            .order_by(Col::FileId, sea_query::Order::Asc)
            .order_by(Col::ItemId, sea_query::Order::Asc)
            .to_owned();

        Query::select()
            .column((Tbl::FileReferences, Col::ItemId))
            .columns(
                TempScanEntry::columns_with_type()
                    .into_iter()
                    .map(|(c, _)| (Tbl::Scan, c)),
            )
            .from_subquery(util::parquet_query(&self.scan), Tbl::Scan)
            .join_subquery(
                JoinType::LeftJoin,
                distinct_ents,
                Tbl::FileReferences,
                Expr::col((Tbl::Scan, col_file_id.clone()))
                    .eq(Expr::col((Tbl::FileReferences, col_file_id))),
            )
            .to_owned()
    }

    /// 削除判定：生存リスト(live)にも、変更(scan)にも、roots 範囲外にも
    /// location を持たない既存 ID
    pub(crate) fn query_deleted(&self, roots: &[PathBuf]) -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId).from_subquery(
            util::parquet_query(&self.ents),
            Tbl::FileReferences,
        );

        let mut live_q = Query::select();
        live_q
            .column(Col::ItemId)
            .from_subquery(util::parquet_query(&self.live), Tbl::Live);
        q.union(sea_query::UnionType::Except, live_q);

        let col_file_id = util::col_to_iden(ScanEntry::schema()[1].name);
        let mut scan_q = Query::select();
        scan_q
            .column((Tbl::FileReferences, Col::ItemId))
            .from_subquery(util::parquet_query(&self.ents), Tbl::FileReferences)
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(&self.scan),
                Tbl::Scan,
                Expr::col((Tbl::FileReferences, col_file_id.clone()))
                    .eq(Expr::col((Tbl::Scan, col_file_id))),
            );
        q.union(sea_query::UnionType::Except, scan_q);

        let mut outside_q = Query::select();
        outside_q
            .column(Col::ItemId)
            .from_subquery(util::parquet_query(&self.locs), Tbl::Locations)
            .cond_where(in_scope(Col::Path, roots).not());
        q.union(sea_query::UnionType::Except, outside_q);

        q
    }
}

// ========================================================
// 3. Execution Extensions
// ========================================================

pub(crate) trait SelectFetchExt {
    fn fetch_with_ids(
        &self,
        conn: &Connection,
    ) -> Result<Vec<(Option<ItemId>, TempScanEntry)>>;
    fn fetch_ids(&self, conn: &Connection) -> Result<Vec<ItemId>>;
}

impl SelectFetchExt for SelectStatement {
    fn fetch_with_ids(
        &self,
        conn: &Connection,
    ) -> Result<Vec<(Option<ItemId>, TempScanEntry)>> {
        let loader = ScanEntryLoader::new();
        let sql = self.to_string(PostgresQueryBuilder);
        conn.prepare(&sql)?
            .query_map([], |row| {
                let id: Option<ItemId> = row.get(0)?;
                let entry =
                    TempScanEntry::from_row_with_offset(row, 1, &loader)?;
                Ok((id, entry))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn fetch_ids(&self, conn: &Connection) -> Result<Vec<ItemId>> {
        let sql = self.to_string(PostgresQueryBuilder);
        conn.prepare(&sql)?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

// ========================================================
// 4. Data Structures
// ========================================================

#[derive(Default)]
pub(crate) struct IndexDiff {
    pub to_process: Vec<(Option<ItemId>, TempScanEntry)>,
    pub deleted_ids: Vec<ItemId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_diff_auditor_initial() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let scan_path = store.temp_scan_path();
        let auditor = DiffAuditor::new(&store, &scan_path);
        assert!(auditor.is_initial());
    }

    #[test]
    fn removed_files_is_empty_when_table_has_no_rows() -> Result<()> {
        let dir = tempdir()?;
        let registry = crate::tag::TagRegistry::with_standard();
        let store = Store::open(dir.path().join("db"))?;
        crate::indexing::Indexer::new(&store, &registry).initialize_tables()?;

        let removed_path = store
            .path_for_target(TargetTable::RemovedFiles)
            .to_string_lossy()
            .into_owned();
        assert!(removed_files_is_empty(&store.conn, &removed_path)?);
        Ok(())
    }
}
