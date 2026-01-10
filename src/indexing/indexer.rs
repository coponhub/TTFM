use crate::taggers::{TagValue, ColumnDef};
use crate::db::{Tbl, Col, DuckDbFunc, TargetTable, Store};
use crate::{FunctionRegistry};
use crate::util::{self, ExecuteSql, ParquetExt, IdenExt, SelectExt};
use anyhow::Result;
use duckdb::{Connection};
use sea_query::{
    Query, Expr, PostgresQueryBuilder, 
    Func, Table
};
use std::path::{Path, PathBuf};

use super::scan;
use super::diff;
use super::triage;
use super::merge::{self, MergeQueryParts};

// ========================================================
// Shared Data Structures
// ========================================================

pub struct TaggingResult {
    pub entity_row: DynamicRow,
    pub location_row: DynamicRow,
    pub tags: Vec<TagRow>,
}

pub struct DynamicRow {
    pub id: i64,
    pub values: Vec<TagValue>,
}

#[derive(Debug, PartialEq)]
pub struct TagRow {
    pub item_id: i64,
    pub tag_type: String,
    pub label: String,
}

// ========================================================
// Main Indexer (The Orchestrator)
// ========================================================

pub struct Indexer<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) registry: &'a FunctionRegistry,
    pub(crate) store: Store,
}

impl<'a> Indexer<'a> {
    pub fn new(
        conn: &'a Connection,
        registry: &'a FunctionRegistry,
        db_dir: PathBuf,
    ) -> Self {
        Self {
            conn,
            registry,
            store: Store::new(db_dir),
        }
    }

    /// インデックス作成の全体ワークフローを実行します。
    pub fn run<P, F>(
        &self,
        root_path: P,
        on_progress: Option<&F>,
        dry_run: bool,
    ) -> Result<usize>
    where
        P: AsRef<Path>,
        F: Fn(usize) + Sync + Send,
    {
        // 1. Scan Phase
        let count = scan::run_scan(
            self.conn,
            &self.store.db_dir,
            &self.store.temp_scan_path(),
            root_path.as_ref(),
            on_progress,
            dry_run,
        )?;

        if dry_run {
            return Ok(count);
        }

        // 2. Diff Phase
        let diff = diff::run_diff(self.conn, &self.store)?;

        // 3. Triage Phase
        let (results, moved) = triage::run_triage(
            self.registry,
            diff.to_tag,
            diff.moved,
            || self.max_file_id()
        )?;

        // 4. Merge Phase
        merge::run_merge(
            self.conn,
            self.registry,
            &self.store,
            results,
            moved,
            diff.deleted_ids,
            diff.unchanged_ids,
            &self.store.temp_scan_path(),
            |data| self.update_system_items(data)
        )?;

        Ok(count)
    }

    /// データベーステーブルとビューの初期化を行います。
    pub fn initialize_tables(&self) -> Result<()> {
        let all_cols = self.registry.get_all_columns();
        use strum::IntoEnumIterator;

        for target in TargetTable::iter() {
            let path = self.store.path_for_target(target);
            self.ensure_empty_parquet_if_missing(&path, target, &all_cols)?;
        }

        crate::oneview::OneView::recreate(self.conn, &all_cols, &self.store.db_dir)?;
        
        self.update_system_items(None)?;
        Ok(())
    }

    fn ensure_empty_parquet_if_missing(
        &self,
        path: &Path,
        target: TargetTable,
        columns: &[ColumnDef],
    ) -> Result<()> {
        if path.exists() {
            return Ok(());
        }
        let table = Tbl::Master;
        crate::db::Schema::build_table(target, table, columns)
            .execute(self.conn)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        table.write_parquet(self.conn, path)?;
        Table::drop().table(table).execute(self.conn).ok();
        Ok(())
    }

    /// システム定義アイテム（拡張子、タグ型など）をデータベースに登録・更新します。
    pub fn update_system_items(&self, data_candidates: Option<sea_query::SelectStatement>) -> Result<()> {
        let items_path = self.store.path_for_target(TargetTable::ItemEntities);
        let system_tags_path = self.store.path_for_target(TargetTable::SystemTags);
        let items_str = items_path.to_string_lossy();

        let mut all_candidates = MergeQueryParts::registry_variants(self.registry);
        if let Some(data) = data_candidates {
            all_candidates.union(sea_query::UnionType::Distinct, data);
        }

        MergeQueryParts::filter_new(all_candidates, &items_str)
            .create_temp_table_as(self.conn, Tbl::Item)?;

        if self.count_table(Tbl::Item)? == 0 {
            Tbl::Item.drop_table(self.conn)?;
            return Ok(());
        }

        let start_id = self.next_item_id(&items_str)?;
        let tmp_items = items_path.with_extension("parquet.tmp");
        let tmp_stags = system_tags_path.with_extension("parquet.tmp");

        MergeQueryParts::assign_ids(start_id)
            .create_temp_table_as(self.conn, Tbl::IdItem)?;

        util::parquet_query(&items_str)
            .union(
                sea_query::UnionType::All,
                Query::select()
                    .columns([Col::ItemId, Col::Rank, Col::ItemKind, Col::Content])
                    .from(Tbl::IdItem)
                    .to_owned(),
            )
            .save_parquet(self.conn, &tmp_items)?;

        util::parquet_query(&system_tags_path.to_string_lossy())
            .union(sea_query::UnionType::All, MergeQueryParts::metadata_tags())
            .save_parquet(self.conn, &tmp_stags)?;

        self.finalize_updates(
            &items_path,
            &system_tags_path,
            &tmp_items,
            &tmp_stags,
        )
    }

    // --- Shared Helpers ---

    /// ファイルエンティティの現在の最大ID（正の整数）を取得します。
    pub(crate) fn max_file_id(&self) -> Result<i64> {
        let ents_path = self.store.path_for_target(TargetTable::FileEntities);
        if !ents_path.exists() {
            return Ok(0);
        }
        let ents_str = ents_path.to_string_lossy();
        let query = Query::select()
            .expr(Func::cust(DuckDbFunc::Coalesce).args([
                Expr::col(Col::ItemId).max().into(),
                Expr::val(0).into(),
            ]))
            .from_subquery(util::parquet_query(&ents_str), Tbl::FileEntities)
            .to_string(PostgresQueryBuilder);

        self.conn
            .query_row(&query, [], |r| r.get(0))
            .map_err(Into::into)
    }

    /// 次の負のアイテムIDを取得します（システムアイテム用）。
    pub(crate) fn next_item_id(&self, items_path: &str) -> Result<i64> {
        let query_min = Query::select()
            .expr(
                Func::cust(DuckDbFunc::Coalesce)
                    .args([Expr::col(Col::ItemId).min().into(), Expr::val(0).into()]),
            )
            .from_subquery(util::parquet_query(items_path), Tbl::ItemEntities)
            .to_string(PostgresQueryBuilder);

        let min_id: i64 = self.conn.query_row(&query_min, [], |r| r.get(0))?;
        Ok(if min_id > -1 { -1 } else { min_id - 1 })
    }

    /// テーブルのレコード数を取得します（共有ロジック）。
    pub(crate) fn count_table(&self, table: impl sea_query::Iden + Clone + 'static) -> Result<i64> {
        let sql = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from(table)
            .to_string(PostgresQueryBuilder);
        self.conn.query_row(&sql, [], |r| r.get(0)).map_err(Into::into)
    }

    fn finalize_updates(
        &self,
        items_path: &Path,
        stags_path: &Path,
        tmp_items: &Path,
        tmp_stags: &Path,
    ) -> Result<()> {
        std::fs::rename(tmp_items, items_path)?;
        std::fs::rename(tmp_stags, stags_path)?;
        Tbl::Item.drop_table(self.conn)?;
        Tbl::IdItem.drop_table(self.conn)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use crate::FunctionRegistry;

    #[test]
    fn test_initialize_tables() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let conn = Connection::open_in_memory().unwrap();
        let registry = FunctionRegistry::with_standard();
        let indexer = Indexer::new(&conn, &registry, db_dir.clone());
        indexer.initialize_tables().unwrap();
        assert!(db_dir.join("file_entities.parquet").exists());
    }

    #[test]
    fn test_indexer_next_item_id_logic() {
        fn calc_next(min_id: i64) -> i64 {
            if min_id > -1 { -1 } else { min_id - 1 }
        }
        assert_eq!(calc_next(0), -1);
        assert_eq!(calc_next(-1), -2);
    }
}