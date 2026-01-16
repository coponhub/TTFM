use super::indexer::{ScanEntryLoader, TempScanEntry};
use crate::db::{Col, Store, TargetTable, Tbl};
use crate::functions::ScanEntry;
use crate::types::ItemId;
use crate::util::{self};
use anyhow::Result;
use duckdb::Connection;
use sea_query::{Expr, JoinType, PostgresQueryBuilder, Query, SelectStatement};
use std::path::Path;

// ========================================================
// Diff Phase Orchestrator
// ========================================================

pub(crate) fn run_diff(conn: &Connection, store: &Store) -> Result<IndexDiff> {
    let auditor = DiffAuditor::new(store);

    if auditor.is_initial() {
        return Ok(IndexDiff {
            to_process: auditor.query_all_scanned().fetch_with_ids(conn)?,
            ..Default::default()
        });
    }

    // 1. 今回スキャンされたエントリに対し、既存の Inode を持っていればその ID を特定
    let to_process = auditor.query_with_existing_ids().fetch_with_ids(conn)?;

    // 2. 削除対象の特定（生存リストにも、今回のスキャン結果にも載っていない既存 ID）
    let deleted_ids = auditor.query_deleted().fetch_ids(conn)?;

    Ok(IndexDiff {
        to_process,
        deleted_ids,
    })
}

// ========================================================
// 1. Diff Auditor
// ========================================================

pub(crate) struct DiffAuditor {
    scan: String,
    live: String,
    ents: String,
}

impl DiffAuditor {
    pub(crate) fn new(store: &Store) -> Self {
        let path = |t| store.path_for_target(t).to_string_lossy().into_owned();
        Self {
            scan: store.temp_scan_path().to_string_lossy().into_owned(),
            live: store.temp_live_path().to_string_lossy().into_owned(),
            ents: path(TargetTable::FileEntities),
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
            .from_subquery(util::parquet_query(&self.ents), Tbl::FileEntities)
            .order_by(Col::FileId, sea_query::Order::Asc)
            .order_by(Col::ItemId, sea_query::Order::Asc)
            .to_owned();

        Query::select()
            .column((Tbl::FileEntities, Col::ItemId))
            .columns(
                TempScanEntry::columns_with_type()
                    .into_iter()
                    .map(|(c, _)| (Tbl::Scan, c)),
            )
            .from_subquery(util::parquet_query(&self.scan), Tbl::Scan)
            .join_subquery(
                JoinType::LeftJoin,
                distinct_ents,
                Tbl::FileEntities,
                Expr::col((Tbl::Scan, col_file_id.clone()))
                    .eq(Expr::col((Tbl::FileEntities, col_file_id))),
            )
            .to_owned()
    }

    /// 削除判定：生存リスト(live)にも、変更(scan)にも載っていない既存 ID
    pub(crate) fn query_deleted(&self) -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId)
            .from_subquery(util::parquet_query(&self.ents), Tbl::FileEntities);

        let mut live_q = Query::select();
        live_q
            .column(Col::ItemId)
            .from_subquery(util::parquet_query(&self.live), Tbl::Live);
        q.union(sea_query::UnionType::Except, live_q);

        let col_file_id = util::col_to_iden(ScanEntry::schema()[1].name);
        let mut scan_q = Query::select();
        scan_q
            .column((Tbl::FileEntities, Col::ItemId))
            .from_subquery(util::parquet_query(&self.ents), Tbl::FileEntities)
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(&self.scan),
                Tbl::Scan,
                Expr::col((Tbl::FileEntities, col_file_id.clone()))
                    .eq(Expr::col((Tbl::Scan, col_file_id))),
            );
        q.union(sea_query::UnionType::Except, scan_q);

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
        let store = Store::new(dir.path().to_path_buf());
        let auditor = DiffAuditor::new(&store);
        assert!(auditor.is_initial());
    }
}
