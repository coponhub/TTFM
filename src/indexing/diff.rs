use crate::functions::{ScanEntry, ScanRole};
use crate::db::{Tbl, Col, TargetTable, Store};
use crate::util::{self};
use anyhow::Result;
use duckdb::{Connection};
use sea_query::{
    Query, Expr, Condition, JoinType, PostgresQueryBuilder, 
    SelectStatement
};
use std::path::{Path};

// ========================================================
// Diff Phase Orchestrator
// ========================================================

pub(crate) fn run_diff(conn: &Connection, store: &Store) -> Result<IndexDiff> {
    let diff = DiffAuditor::new(store);

    if diff.is_initial() {
        return Ok(IndexDiff {
            to_tag: diff.query_all().fetch_entries(conn)?,
            ..Default::default()
        });
    }

    Ok(IndexDiff {
        to_tag: diff.query_to_tag().fetch_entries(conn)?,
        moved: diff.query_moved().fetch_moved(conn)?,
        deleted_ids: diff.query_deleted().fetch_ids(conn)?,
        unchanged_ids: diff.query_unchanged().fetch_ids(conn)?,
    })
}

// ========================================================
// 1. Diff Auditor
// ========================================================

pub(crate) struct DiffAuditor {
    scan: String,
    ents: String,
    locs: String,
}

impl DiffAuditor {
    pub(crate) fn new(store: &Store) -> Self {
        let path = |t| store.path_for_target(t).to_string_lossy().into_owned();
        Self {
            scan: store.temp_scan_path().to_string_lossy().into_owned(),
            ents: path(TargetTable::FileEntities),
            locs: path(TargetTable::Locations),
        }
    }

    pub(crate) fn is_initial(&self) -> bool {
        !Path::new(&self.ents).exists()
    }

    pub(crate) fn query_all(&self) -> SelectStatement {
        Query::select()
            .columns(ScanEntry::column_idens())
            .from_subquery(util::parquet_query(&self.scan), Tbl::Scan)
            .to_owned()
    }

    pub(crate) fn query_to_tag(&self) -> SelectStatement {
        AuditQueryParts::to_tag(&self.scan, &self.ents)
    }

    pub(crate) fn query_moved(&self) -> SelectStatement {
        AuditQueryParts::moved(&self.scan, &self.ents, &self.locs)
    }

    pub(crate) fn query_deleted(&self) -> SelectStatement {
        AuditQueryParts::deleted(&self.scan, &self.ents)
    }

    pub(crate) fn query_unchanged(&self) -> SelectStatement {
        AuditQueryParts::unchanged(&self.scan, &self.ents, &self.locs)
    }
}

// ========================================================
// 2. Query Builder for Auditing
// ========================================================

struct AuditQueryParts;

impl AuditQueryParts {
    fn join_by_role(left: Tbl, right: Tbl, role: ScanRole) -> Condition {
        let col_eq = |col: sea_query::DynIden| {
            Expr::col((left, col.clone())).eq(Expr::col((right, col)))
        };

        ScanEntry::schema()
            .iter()
            .filter(|cd| cd.role == role)
            .map(|cd| col_eq(util::col_to_iden(cd.name)))
            .fold(Condition::all(), Condition::add)
    }

    fn join_ent_scan(role: ScanRole) -> Condition {
        Self::join_by_role(Tbl::FileEntities, Tbl::Scan, role)
    }

    fn to_tag(scan_path: &str, entities_path: &str) -> SelectStatement {
        let sub_exists = Query::select()
            .expr(Expr::val(1))
            .from_subquery(util::parquet_query(entities_path), Tbl::FileEntities)
            .cond_where(Self::join_ent_scan(ScanRole::ScanId))
            .cond_where(Self::join_ent_scan(ScanRole::Integrity))
            .to_owned();

        let columns = ScanEntry::schema()
            .iter()
            .map(|c| {
                let col = util::col_to_iden(c.name);
                (Tbl::Scan, col)
            })
            .collect::<Vec<_>>();

        Query::select()
            .columns(columns)
            .from_subquery(util::parquet_query(scan_path), Tbl::Scan)
            .and_where(Expr::exists(sub_exists).not())
            .to_owned()
    }

    fn moved(
        scan_path: &str,
        entities_path: &str,
        loc_path: &str,
    ) -> SelectStatement {
        let path_name = ScanEntry::schema()[0].name;
        let col_path = util::col_to_iden(path_name);
        Query::select()
            .column((Tbl::FileEntities, Col::ItemId))
            .column((Tbl::Scan, col_path.clone()))
            .from_subquery(util::parquet_query(entities_path), Tbl::FileEntities)
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(scan_path),
                Tbl::Scan,
                Self::join_ent_scan(ScanRole::ScanId),
            )
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(loc_path),
                Tbl::Locations,
                Expr::col((Tbl::FileEntities, Col::ItemId))
                    .eq(Expr::col((Tbl::Locations, Col::ItemId))),
            )
            .and_where(
                Expr::col((Tbl::Locations, Col::Path)).ne(Expr::col((Tbl::Scan, col_path))),
            )
            .cond_where(Self::join_ent_scan(ScanRole::Integrity))
            .to_owned()
    }

    fn deleted(scan_path: &str, entities_path: &str) -> SelectStatement {
        let path_name = ScanEntry::schema()[0].name;
        let col_path = util::col_to_iden(path_name);
        let join_cond = Condition::all()
            .add(Self::join_ent_scan(ScanRole::ScanId))
            .add(Self::join_ent_scan(ScanRole::Integrity));

        Query::select()
            .column((Tbl::FileEntities, Col::ItemId))
            .from_subquery(util::parquet_query(entities_path), Tbl::FileEntities)
            .join_subquery(
                JoinType::LeftJoin,
                util::parquet_query(scan_path),
                Tbl::Scan,
                join_cond,
            )
            .and_where(Expr::col((Tbl::Scan, col_path)).is_null())
            .to_owned()
    }

    fn unchanged(
        scan_path: &str,
        entities_path: &str,
        loc_path: &str,
    ) -> SelectStatement {
        let path_name = ScanEntry::schema()[0].name;
        let col_path = util::col_to_iden(path_name);
        Query::select()
            .column((Tbl::FileEntities, Col::ItemId))
            .from_subquery(util::parquet_query(entities_path), Tbl::FileEntities)
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(scan_path),
                Tbl::Scan,
                Self::join_ent_scan(ScanRole::ScanId),
            )
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(loc_path),
                Tbl::Locations,
                Expr::col((Tbl::FileEntities, Col::ItemId))
                    .eq(Expr::col((Tbl::Locations, Col::ItemId))),
            )
            .and_where(
                Expr::col((Tbl::Locations, Col::Path)).eq(Expr::col((Tbl::Scan, col_path))),
            )
            .cond_where(Self::join_ent_scan(ScanRole::Integrity))
            .to_owned()
    }
}

// ========================================================
// 3. Execution Extensions
// ========================================================

pub(crate) trait SelectFetchExt {
    fn fetch_entries(&self, conn: &Connection) -> Result<Vec<ScanEntry>>;
    fn fetch_ids(&self, conn: &Connection) -> Result<Vec<i64>>;
    fn fetch_moved(&self, conn: &Connection) -> Result<Vec<(i64, String)>>;
}

fn fetch_rows<T, F>(
    stmt: &SelectStatement,
    conn: &Connection,
    mapper: F,
) -> Result<Vec<T>>
where
    F: FnMut(&duckdb::Row<'_>) -> duckdb::Result<T>,
{
    let sql = stmt.to_string(PostgresQueryBuilder);
    conn.prepare(&sql)?
        .query_map([], mapper)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

impl SelectFetchExt for SelectStatement {
    fn fetch_entries(&self, conn: &Connection) -> Result<Vec<ScanEntry>> {
        fetch_rows(self, conn, |row| ScanEntry::from_row(row))
    }

    fn fetch_ids(&self, conn: &Connection) -> Result<Vec<i64>> {
        fetch_rows(self, conn, |row| row.get(0))
    }

    fn fetch_moved(&self, conn: &Connection) -> Result<Vec<(i64, String)>> {
        fetch_rows(self, conn, |row| Ok((row.get(0)?, row.get(1)?)))
    }
}

// ========================================================
// 4. Data Structures
// ========================================================

#[derive(Default)]
pub(crate) struct IndexDiff {
    pub to_tag: Vec<ScanEntry>,
    pub moved: Vec<(i64, String)>,
    pub deleted_ids: Vec<i64>,
    pub unchanged_ids: Vec<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_diff_auditor_logic() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join("db");
        let store = Store::new(db_dir);

        // 1. 初回スキャンのテスト
        let diff = DiffAuditor::new(&store);
        assert!(diff.is_initial());
        let sql = diff.query_all().to_string(PostgresQueryBuilder);
        assert!(sql.contains("read_parquet"));

        // 2. 既存DBがある場合のテスト
        std::fs::create_dir_all(&store.db_dir).unwrap();
        std::fs::write(store.path_for_target(TargetTable::FileEntities), "").unwrap();
        let diff2 = DiffAuditor::new(&store);
        assert!(!diff2.is_initial());
    }

    #[test]
    fn test_diff_auditor_sql_generation() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let auditor = DiffAuditor::new(&store);

        let sql_to_tag = auditor.query_to_tag().to_string(PostgresQueryBuilder);
        assert!(sql_to_tag.contains("NOT EXISTS"));

        let sql_moved = auditor.query_moved().to_string(PostgresQueryBuilder);
        assert!(sql_moved.contains("JOIN"));
        assert!(sql_moved.contains("<>"));

        let sql_deleted = auditor.query_deleted().to_string(PostgresQueryBuilder);
        assert!(sql_deleted.contains("LEFT JOIN"));
        assert!(sql_deleted.contains("IS NULL"));
    }
}