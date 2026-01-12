use crate::taggers::{ColumnDef, TagValue};
use crate::db::{Tbl, Col, SqlType, TargetTable, Store};
use crate::{FunctionRegistry};
use crate::util::{self, ExecuteSql, ParquetExt, IdenExt};
use crate::indexing::indexer::{TaggingResult, DynamicRow};
use anyhow::Result;
use duckdb::{Connection, ToSql};
use sea_query::{
    Query, Expr, Condition, JoinType, 
    Iden, SelectStatement, CaseStatement
};
use std::path::{Path};

// ========================================================
// Merge Phase Orchestrator
// ========================================================

pub(crate) fn run_merge(
    conn: &Connection,
    registry: &FunctionRegistry,
    store: &Store,
    results: Vec<TaggingResult>,
    moved: Vec<DynamicRow>,
    deleted_ids: Vec<i64>,
    temp_scan_path: &Path,
    temp_live_path: &Path,
    update_sys_fn: impl Fn(Option<SelectStatement>) -> Result<()>,
) -> Result<()> {
    // 各テーブルの取り込みと同期を実行
    
    // A. 実体テーブル (不変): 削除は行わず、新規登録のみ。
    let ent = FileEntityMerger { conn, registry, store }
        .prepare()?
        .ingest(&results)?
        .sync()?;
    
    // B. 場所テーブル (可変): 属性（size/mtime/hash）を含めて上書き更新。
    let loc = LocationMerger { conn, registry, store }
        .prepare()?
        .ingest(&results, &moved)?
        .sync(temp_live_path)?;
    
    // C. タグテーブル (可変): 削除 ID の分を掃除。
    let tag = BaseTagMerger { conn, registry, store }
        .prepare()?
        .ingest(&results)?
        .sync(&deleted_ids)?;

    if !results.is_empty() || !moved.is_empty() {
        let tags = MergeQueryParts::diff_tags(&registry.get_all_columns());
        let candidates_data = MergeQueryParts::expand_variants(tags);
        update_sys_fn(Some(candidates_data))?;
    }

    ent.cleanup()?;
    loc.cleanup()?;
    tag.cleanup()?;

    // クリーンアップ
    std::fs::remove_file(temp_scan_path).ok();
    std::fs::remove_file(temp_live_path).ok();
    Ok(())
}

// ========================================================
// 1. Merger Contexts
// ========================================================

pub(crate) struct FileEntityMerger<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) registry: &'a FunctionRegistry,
    pub(crate) store: &'a Store,
}

impl<'a> FileEntityMerger<'a> {
    pub(crate) fn prepare(self) -> Result<Self> {
        let all_cols = self.registry.get_all_columns();
        let mut create_stmt = crate::db::Schema::build_table(
            TargetTable::FileEntities,
            Tbl::FileEntitiesDiff,
            &all_cols,
        );
        create_stmt.temporary().execute(self.conn)?;
        Ok(self)
    }

    pub(crate) fn ingest(self, results: &[TaggingResult]) -> Result<Self> {
        if results.is_empty() {
            return Ok(self);
        }
        let table_name = Tbl::FileEntitiesDiff.to_string().replace('"', "");
        let mut app = self.conn.appender(&table_name)?;
        
        for res in results {
            let mut er = vec![&res.entity_row.id as &dyn ToSql];
            er.extend(res.entity_row.values.iter().map(|v| v as &dyn ToSql));
            app.append_row(er.as_slice())?;
        }
        Ok(self)
    }

    pub(crate) fn sync(self) -> Result<Self> {
        // file_entities は item_id をキーにしてマージ
        merge_and_save(
            self.conn,
            &self.store.path_for_target(TargetTable::FileEntities),
            Tbl::FileEntitiesDiff,
            None,
            Col::ItemId,
        )?;
        Ok(self)
    }

    pub(crate) fn cleanup(self) -> Result<()> {
        Tbl::FileEntitiesDiff.drop_table(self.conn).ok();
        Ok(())
    }
}

pub(crate) struct LocationMerger<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) registry: &'a FunctionRegistry,
    pub(crate) store: &'a Store,
}

impl<'a> LocationMerger<'a> {
    pub(crate) fn prepare(self) -> Result<Self> {
        let all_cols = self.registry.get_all_columns();
        let mut create_stmt = crate::db::Schema::build_table(
                TargetTable::Locations,
                Tbl::LocationsDiff,
                &all_cols,
            );
        create_stmt.temporary().execute(self.conn)?;
        Ok(self)
    }

    pub(crate) fn ingest(self, results: &[TaggingResult], moved: &[DynamicRow]) -> Result<Self> {
        if results.is_empty() && moved.is_empty() {
            return Ok(self);
        }
        let table_name = Tbl::LocationsDiff.to_string().replace('"', "");
        let mut app = self.conn.appender(&table_name)?;
        
        for res in results {
            let mut lr = vec![&res.location_row.id as &dyn ToSql];
            lr.extend(res.location_row.values.iter().map(|v| v as &dyn ToSql));
            lr.push(&res.scan_hash as &dyn ToSql);
            app.append_row(lr.as_slice())?;
        }
        for row in moved {
            let mut lr = vec![&row.id as &dyn ToSql];
            lr.extend(row.values.iter().map(|v| v as &dyn ToSql));
            lr.push(&TagValue::Null as &dyn ToSql);
            app.append_row(lr.as_slice())?;
        }
        Ok(self)
    }

    pub(crate) fn sync(self, live_path: &Path) -> Result<Self> {
        let path_str = live_path.to_string_lossy();
        let live_query = Query::select()
            .column(Col::ItemId)
            .from_subquery(util::parquet_query(&path_str), Tbl::Live)
            .to_owned();

        // locations は path をキーにして上書き判定を行う（ハードリンク対応）
        merge_and_save(
            self.conn,
            &self.store.path_for_target(TargetTable::Locations),
            Tbl::LocationsDiff,
            Some(Condition::all().add(Expr::col(Col::ItemId).in_subquery(live_query))),
            Col::Path,
        )?;
        Ok(self)
    }

    pub(crate) fn cleanup(self) -> Result<()> {
        Tbl::LocationsDiff.drop_table(self.conn).ok();
        Ok(())
    }
}

pub(crate) struct BaseTagMerger<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) registry: &'a FunctionRegistry,
    pub(crate) store: &'a Store,
}

impl<'a> BaseTagMerger<'a> {
    pub(crate) fn prepare(self) -> Result<Self> {
        let all_cols = self.registry.get_all_columns();
        crate::db::Schema::build_table(
                TargetTable::BaseTags,
                Tbl::BaseTagsDiff,
                &all_cols,
            )
            .temporary()
            .execute(self.conn)?;
        Ok(self)
    }

    pub(crate) fn ingest(self, results: &[TaggingResult]) -> Result<Self> {
        if results.is_empty() {
            return Ok(self);
        }
        let table_name = Tbl::BaseTagsDiff.to_string().replace('"', "");
        let mut app = self.conn.appender(&table_name)?;
        
        for res in results {
            for t in &res.tags {
                app.append_row([
                    &t.item_id as &dyn ToSql,
                    &t.tag_type,
                    &t.label,
                ])?;
            }
        }
        Ok(self)
    }

    pub(crate) fn sync(self, deleted_ids: &[i64]) -> Result<Self> {
        // base_tags は item_id をキーにしてマージ
        merge_and_save(
            self.conn,
            &self.store.path_for_target(TargetTable::BaseTags),
            Tbl::BaseTagsDiff,
            (!deleted_ids.is_empty()).then(|| {
                Condition::all()
                    .add(Expr::col(Col::ItemId).is_not_in(deleted_ids.to_vec()))
            }),
            Col::ItemId,
        )?;
        Ok(self)
    }

    pub(crate) fn cleanup(self) -> Result<()> {
        Tbl::BaseTagsDiff.drop_table(self.conn).ok();
        Ok(())
    }
}

// ========================================================
// 2. Query Builder for Merging
// ========================================================

pub(crate) struct MergeQueryParts;

impl MergeQueryParts {
    pub(crate) fn diff_tags(all_cols: &[ColumnDef]) -> SelectStatement {
        let mut source_q = Query::select();
        source_q
            .expr_as(Expr::col(Col::Type), Col::Type)
            .expr_as(Expr::col(Col::Label), Col::Label)
            .from(Tbl::BaseTagsDiff);

        for col in all_cols.iter().filter(|c| {
            c.target_table == TargetTable::Locations
        }) {
            let mut sub = Query::select();
            let col_iden = util::col_to_iden(&col.name);
            sub.expr_as(Expr::val(col.name.to_string()), Col::Type)
                .expr_as(
                    Expr::col(col_iden.clone()).cast_as(SqlType::VARCHAR),
                    Col::Label,
                )
                .from(Tbl::LocationsDiff)
                .and_where(Expr::col(col_iden).is_not_null());
            source_q.union(sea_query::UnionType::Distinct, sub.to_owned());
        }
        source_q
    }

    pub(crate) fn expand_variants(tags: SelectStatement) -> SelectStatement {
        let mut cand_q = Query::select();
        cand_q
            .expr_as(Expr::val("type"), Col::ItemKind)
            .expr_as(Expr::col(Col::Type), Col::Content)
            .expr_as(Expr::val(0), Col::Rank)
            .column(Col::Type)
            .expr_as(Expr::cust("NULL"), Col::Label)
            .from_subquery(tags.clone(), Tbl::Diff);

        let mut label_q = Query::select();
        label_q
            .expr_as(Expr::val("label"), Col::ItemKind)
            .expr_as(Expr::col(Col::Label), Col::Content)
            .expr_as(Expr::val(0), Col::Rank)
            .expr_as(Expr::cust("NULL"), Col::Type)
            .column(Col::Label)
            .from_subquery(tags.clone(), Tbl::Diff);
        cand_q.union(sea_query::UnionType::Distinct, label_q.to_owned());

        let mut tt_q = Query::select();
        tt_q.expr_as(Expr::val("typedtag"), Col::ItemKind)
            .expr_as(
                Expr::cust_with_exprs("$1 || ':' || $2", [
                    Expr::col(Col::Type).into(),
                    Expr::col(Col::Label).into(),
                ]),
                Col::Content,
            )
            .expr_as(Expr::val(0), Col::Rank)
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(tags, Tbl::Diff);
        cand_q.union(sea_query::UnionType::Distinct, tt_q.to_owned());

        cand_q
    }

    pub(crate) fn registry_variants(registry: &FunctionRegistry) -> SelectStatement {
        let funcs = registry.all_functions();
        if funcs.is_empty() {
            let mut q = Query::select();
            q.expr(Expr::val(1)).and_where(Expr::val(1).eq(0));
            return q;
        }

        let mut query = Query::select();
        let first = &funcs[0];
        query
            .expr_as(Expr::val("type"), Col::ItemKind)
            .expr_as(Expr::val(first.name()), Col::Content)
            .expr_as(Expr::val(first.default_rank()), Col::Rank)
            .expr_as(Expr::val(first.name()), Col::Type)
            .expr_as(Expr::cust("NULL"), Col::Label);

        for func in funcs.iter().skip(1) {
            let mut sub = Query::select();
            sub.expr_as(Expr::val("type"), Col::ItemKind)
                .expr_as(Expr::val(func.name()), Col::Content)
                .expr_as(Expr::val(func.default_rank()), Col::Rank)
                .expr_as(Expr::val(func.name()), Col::Type)
                .expr_as(Expr::cust("NULL"), Col::Label);
            query.union(sea_query::UnionType::Distinct, sub);
        }
        query
    }

    pub(crate) fn filter_new(candidates: SelectStatement, items_path: &str) -> SelectStatement {
        Query::select()
            .column((Tbl::Item, Col::ItemKind))
            .column((Tbl::Item, Col::Content))
            .column((Tbl::Item, Col::Type))
            .column((Tbl::Item, Col::Label))
            .column((Tbl::Item, Col::Rank))
            .distinct()
            .from_subquery(candidates, Tbl::Item)
            .join_subquery(
                JoinType::LeftJoin,
                util::parquet_query(items_path),
                Tbl::ItemEntities,
                Condition::all()
                    .add(
                        Expr::col((Tbl::Item, Col::ItemKind))
                            .eq(Expr::col((Tbl::ItemEntities, Col::ItemKind))),
                    )
                    .add(
                        Expr::col((Tbl::Item, Col::Content))
                            .eq(Expr::col((Tbl::ItemEntities, Col::Content))),
                    ),
            )
            .and_where(Expr::col((Tbl::ItemEntities, Col::ItemId)).is_null())
            .to_owned()
    }

    pub(crate) fn assign_ids(start_id: i64) -> SelectStatement {
        Query::select()
            .expr_as(
                Expr::cust_with_exprs(
                    "$1 - (row_number() OVER (ORDER BY rank DESC, content ASC) - 1)",
                    [Expr::val(start_id).into()],
                ),
                Col::ItemId,
            )
            .column(Col::Rank)
            .column(Col::ItemKind)
            .column(Col::Content)
            .column(Col::Type)
            .column(Col::Label)
            .from(Tbl::Item)
            .to_owned()
    }

    pub(crate) fn metadata_tags() -> SelectStatement {
        let mut meta = Query::select();
        meta.column(Col::ItemId)
            .expr_as(Expr::val("type"), Col::Type)
            .expr_as(
                CaseStatement::new()
                    .case(
                        Expr::col(Col::ItemKind).eq("typedtag"),
                        Expr::col(Col::Type),
                    )
                    .finally(Expr::col(Col::ItemKind)),
                Col::Label,
            )
            .from(Tbl::IdItem);

        let mut label = Query::select();
        label
            .column(Col::ItemId)
            .expr_as(Expr::val("label"), Col::Type)
            .column(Col::Label)
            .from(Tbl::IdItem)
            .and_where(Expr::col(Col::ItemKind).eq("typedtag"));

        meta.union(sea_query::UnionType::All, label).to_owned()
    }
}

// ========================================================
// 3. Internal Utility (Merge context only)
// ========================================================

fn merge_and_save(
    conn: &Connection,
    path: &Path,
    temp_table: impl Iden + Clone + 'static,
    filter: Option<Condition>,
    key_col: Col,
) -> Result<()> {
    let base_query = Query::select()
        .expr(Expr::cust("*"))
        .from(temp_table.clone())
        .to_owned();

    if !path.exists() {
        return base_query.save_parquet(conn, path);
    }

    let path_str = path.to_string_lossy().to_string();
    let mut query = util::parquet_query(&path_str);

    // 【核心】既存データから、今回更新されるレコードをキー（ID またはパス）で除外
    query.and_where(Expr::col(key_col).not_in_subquery(
        Query::select().column(key_col).from(temp_table).to_owned()
    ));

    if let Some(cond) = filter {
        query.cond_where(cond);
    }

    query
        .union(sea_query::UnionType::All, base_query)
        .save_parquet(conn, path)
}

// ========================================================
// Tests
// ========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::PostgresQueryBuilder;

    #[test]
    fn test_query_parts_expand_variants_structure() {
        let tags = Query::select()
            .expr_as(Expr::val("extension"), Col::Type)
            .expr_as(Expr::val("rs"), Col::Label)
            .from(Tbl::Diff)
            .to_owned();
        let sql = MergeQueryParts::expand_variants(tags).to_string(PostgresQueryBuilder);
        assert!(sql.contains("'type'"));
        assert!(sql.contains("'label'"));
        assert!(sql.contains("'typedtag'"));
    }

    #[test]
    fn test_query_parts_metadata_tags_logic() {
        let sql = MergeQueryParts::metadata_tags().to_string(PostgresQueryBuilder);
        assert!(sql.contains("CASE"));
        assert!(sql.contains("'typedtag'"));
    }
}
