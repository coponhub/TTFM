use crate::db::{Col, SqlType, Store, TargetTable, Tbl};
use crate::indexing::indexer::{DynamicRow, TaggingResult};
use crate::taggers::{ColumnDef, TagValue};
use crate::util::{self, ExecuteSql, IdenExt, ParquetExt};
use crate::FunctionRegistry;
use anyhow::Result;
use duckdb::{Connection, ToSql};
use sea_query::{
    Condition, Expr, Func, Iden, JoinType, Query, SelectStatement, SimpleExpr,
};
use std::path::Path;

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
    let ent = FileEntityMerger {
        conn,
        registry,
        store,
    }
    .prepare()?
    .ingest(&results)?
    .sync(&deleted_ids)?;

    // B. 場所テーブル (可変): 属性（size/mtime/hash）を含めて上書き更新。
    let loc = LocationMerger {
        conn,
        registry,
        store,
    }
    .prepare()?
    .ingest(&results, &moved)?
    .sync(temp_live_path)?;

    // C. タグテーブル (可変): 削除 ID の分を掃除。
    let tag = BaseTagMerger {
        conn,
        registry,
        store,
    }
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
            TargetTable::FileReferences,
            Tbl::FileReferencesDiff,
            &all_cols,
        );
        create_stmt.temporary().execute(self.conn)?;
        Ok(self)
    }

    pub(crate) fn ingest(self, results: &[TaggingResult]) -> Result<Self> {
        if results.is_empty() {
            return Ok(self);
        }
        let table_name = Tbl::FileReferencesDiff.to_string().replace('"', "");
        let mut app = self.conn.appender(&table_name)?;

        for res in results {
            let mut er = vec![&res.entity_row.id as &dyn ToSql];
            er.extend(res.entity_row.values.iter().map(|v| v as &dyn ToSql));
            app.append_row(er.as_slice())?;
        }
        Ok(self)
    }

    pub(crate) fn sync(self, deleted_ids: &[i64]) -> Result<Self> {
        // file_entities は item_id をキーにしてマージ
        merge_and_save(
            self.conn,
            &self.store.path_for_target(TargetTable::FileReferences),
            Tbl::FileReferencesDiff,
            (!deleted_ids.is_empty()).then(|| {
                Condition::all()
                    .add(Expr::col(Col::ItemId).is_not_in(deleted_ids.to_vec()))
            }),
            Col::ItemId,
        )?;
        Ok(self)
    }

    pub(crate) fn cleanup(self) -> Result<()> {
        Tbl::FileReferencesDiff.drop_table(self.conn).ok();
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

    pub(crate) fn ingest(
        self,
        results: &[TaggingResult],
        moved: &[DynamicRow],
    ) -> Result<Self> {
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
            Some(
                Condition::all()
                    .add(Expr::col(Col::ItemId).in_subquery(live_query)),
            ),
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
                    &t.label_str,
                    &t.label_int,
                    &t.label_double,
                    &t.label_bool,
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

// 唯一の定義箇所。ここが Single Source of Truth となります。
crate::define_item_schema! {
    kind    => ItemKind,
    content => Content,
    name    => Name,
    rank    => Rank,
    type_   => Type,
    label   => Label,
}

impl ItemRow {
    pub(crate) fn new_type(content: SimpleExpr, rank: i64) -> Self {
        Self {
            kind: Expr::val("type").into(),
            content: content.clone(),
            name: content.clone(),
            rank: Expr::val(rank).into(),
            type_: content,
            label: util::null_as(SqlType::VARCHAR),
        }
    }

    pub(crate) fn new_label(content: SimpleExpr) -> Self {
        Self {
            kind: Expr::val("label").into(),
            content: content.clone(),
            name: content.clone(),
            rank: Expr::val(0).into(),
            type_: util::null_as(SqlType::VARCHAR),
            label: content,
        }
    }

    pub(crate) fn new_typedtag(
        type_expr: SimpleExpr,
        label_expr: SimpleExpr,
    ) -> Self {
        let content = Func::cust(crate::db::DuckDbFunc::Concat).args([
            Expr::col(Col::Type).into(),
            Expr::val(":").into(),
            label_expr.clone(),
        ]);
        Self {
            kind: Expr::val("typedtag").into(),
            content: content.clone().into(),
            name: content.into(),
            rank: Expr::val(0).into(),
            type_: type_expr,
            label: label_expr,
        }
    }

    pub(crate) fn variant_col(kind: &str, col: Col) -> Self {
        match kind {
            "type" => Self::new_type(Expr::col(col).into(), 0),
            "label" => Self::new_label(Expr::col(col).into()),
            _ => panic!("Unsupported tag kind for col builder"),
        }
    }
}

impl MergeQueryParts {
    pub(crate) fn diff_tags(all_cols: &[ColumnDef]) -> SelectStatement {
        let mut source_q = Query::select();
        source_q
            .expr_as(Expr::col(Col::Type), Col::Type)
            .expr_as(
                Func::cust(crate::db::DuckDbFunc::Coalesce).args([
                    Expr::col(Col::LabelStr).into(),
                    Expr::col(Col::LabelInt).cast_as(SqlType::VARCHAR).into(),
                    Expr::col(Col::LabelDouble)
                        .cast_as(SqlType::VARCHAR)
                        .into(),
                    Expr::col(Col::LabelBool).cast_as(SqlType::VARCHAR).into(),
                ]),
                Col::Label,
            )
            .from(Tbl::BaseTagsDiff);

        for col in all_cols
            .iter()
            .filter(|c| c.target_table == TargetTable::Locations)
        {
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

    pub(crate) fn item_columns() -> [Col; 6] {
        ItemRow::all_columns()
    }

    pub(crate) fn expand_variants(tags: SelectStatement) -> SelectStatement {
        // --- Branch 1: Tag Types ('type' tags) ---
        let mut cand_q = ItemRow::variant_col("type", Col::Type).select();
        cand_q.from_subquery(tags.clone(), Tbl::Diff);

        // --- Branch 2: Labels ('label' tags) ---
        let mut label_q = ItemRow::variant_col("label", Col::Label).select();
        label_q.from_subquery(tags.clone(), Tbl::Diff);
        cand_q.union(sea_query::UnionType::Distinct, label_q.to_owned());

        // --- Branch 3: Typed Tags ('typedtag' tags) ---
        let mut tt_q = ItemRow::new_typedtag(
            Expr::col(Col::Type).into(),
            Expr::col(Col::Label).into(),
        )
        .select();
        tt_q.from_subquery(tags, Tbl::Diff);
        cand_q.union(sea_query::UnionType::Distinct, tt_q.to_owned());

        cand_q
    }

    pub(crate) fn registry_variants(
        registry: &FunctionRegistry,
    ) -> SelectStatement {
        let funcs = registry.all_functions();
        if funcs.is_empty() {
            let mut q = Query::select();
            q.expr(Expr::val(1)).and_where(Expr::val(1).eq(0));
            return q;
        }

        let first = &funcs[0];
        let mut query = ItemRow::new_type(
            Expr::val(first.name()).into(),
            first.default_rank(),
        )
        .select();

        for func in funcs.iter().skip(1) {
            query.union(
                sea_query::UnionType::Distinct,
                ItemRow::new_type(
                    Expr::val(func.name()).into(),
                    func.default_rank(),
                )
                .select(),
            );
        }
        query
    }

    pub(crate) fn filter_new(
        candidates: SelectStatement,
        items_path: &str,
    ) -> SelectStatement {
        Query::select()
            .columns(Self::item_columns().map(|c| (Tbl::Item, c)))
            .distinct()
            .from_subquery(candidates, Tbl::Item)
            .join_subquery(
                JoinType::LeftJoin,
                util::parquet_query(items_path),
                Tbl::ItemReferences,
                Condition::all()
                    .add(
                        Expr::col((Tbl::Item, Col::ItemKind)).eq(Expr::col((
                            Tbl::ItemReferences,
                            Col::ItemKind,
                        ))),
                    )
                    .add(
                        Expr::col((Tbl::Item, Col::Content))
                            .eq(Expr::col((Tbl::ItemReferences, Col::Content))),
                    ),
            )
            .and_where(Expr::col((Tbl::ItemReferences, Col::ItemId)).is_null())
            .to_owned()
    }

    pub(crate) fn assign_ids(start_id: i64) -> SelectStatement {
        Query::select()
            .expr_as(
                crate::db::CustomFunc::assign_id_window(start_id),
                Col::ItemId,
            )
            .columns(Self::item_columns())
            .from(Tbl::Item)
            .to_owned()
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
        .column(sea_query::Asterisk)
        .from(temp_table.clone())
        .to_owned();

    if !path.exists() {
        return base_query.save_parquet(conn, path);
    }

    let path_str = path.to_string_lossy().to_string();
    let mut query = util::parquet_query(&path_str);

    // 【核心】既存データから、今回更新されるレコードをキー（ID またはパス）で除外
    query.and_where(Expr::col(key_col).not_in_subquery(
        Query::select().column(key_col).from(temp_table).to_owned(),
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
        let sql = MergeQueryParts::expand_variants(tags)
            .to_string(PostgresQueryBuilder);
        assert!(sql.contains("'type'"));
        assert!(sql.contains("'label'"));
        assert!(sql.contains("'typedtag'"));
    }
}
