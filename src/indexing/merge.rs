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

use crate::db::{Col, Store, TargetTable, Tbl};
// use crate::db::SqlType;
use crate::indexing::indexer::{DynamicRow, TaggingResult};
use crate::tag::TagRegistry;
use crate::taggers::TagValue;
use crate::types::ItemId;
use crate::util::{self, ExecuteSql, IdenExt, ParquetExt};
use anyhow::Result;
use duckdb::{Connection, ToSql};
use sea_query::{Condition, Expr, Iden, Order, Query, SelectStatement};
// use sea_query::{JoinType, SimpleExpr};
use std::path::Path;

// ========================================================
// Merge Phase Orchestrator
// ========================================================

pub(crate) fn run_merge(
    conn: &Connection,
    registry: &TagRegistry,
    store: &Store,
    results: Vec<TaggingResult>,
    moved: Vec<DynamicRow>,
    deleted_ids: Vec<ItemId>,
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

    ent.cleanup()?;
    loc.cleanup()?;
    tag.cleanup()?;

    // システムアイテム（基本Type定義のみ）の更新
    // 以前はここで type/label/tag の全バリエーションを登録していたが、
    // oneview のプロジェクションにより不要になったため廃止した。
    update_sys_fn(None)?;

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
    pub(crate) registry: &'a TagRegistry,
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

    pub(crate) fn sync(self, deleted_ids: &[ItemId]) -> Result<Self> {
        let ids_i64: Vec<i64> =
            deleted_ids.iter().map(|id| id.as_i64()).collect();
        // file_entities は item_id をキーにしてマージ
        merge_and_save(
            self.conn,
            &self.store.path_for_target(TargetTable::FileReferences),
            Tbl::FileReferencesDiff,
            (!ids_i64.is_empty()).then(|| {
                Condition::all()
                    .add(Expr::col(Col::ItemId).is_not_in(ids_i64.clone()))
            }),
            Col::ItemId,
            None,
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
    pub(crate) registry: &'a TagRegistry,
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
            None,
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
    pub(crate) registry: &'a TagRegistry,
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

    pub(crate) fn sync(self, deleted_ids: &[ItemId]) -> Result<Self> {
        let ids_i64: Vec<i64> =
            deleted_ids.iter().map(|id| id.as_i64()).collect();
        // base_tags は item_id をキーにしてマージ
        merge_and_save(
            self.conn,
            &self.store.path_for_target(TargetTable::BaseTags),
            Tbl::BaseTagsDiff,
            (!ids_i64.is_empty()).then(|| {
                Condition::all()
                    .add(Expr::col(Col::ItemId).is_not_in(ids_i64.clone()))
            }),
            Col::ItemId,
            Some(vec![Col::Type, Col::LabelInt, Col::LabelStr, Col::ItemId]),
        )?;
        Ok(self)
    }

    pub(crate) fn cleanup(self) -> Result<()> {
        Tbl::BaseTagsDiff.drop_table(self.conn).ok();
        Ok(())
    }
}

// // ========================================================
// // 2. Query Builder for Merging
// // ========================================================
// pub(crate) struct MergeQueryParts;
//
// // 唯一の定義箇所。ここが Single Source of Truth となります。
// crate::define_item_schema! {
//     ItemRow {
//         kind    => ItemKind,
//         content => Content,
//         name    => Name,
//         rank    => Rank,
//         type_   => Type,
//         label   => Label,
//     }
// }
//
// impl ItemRow {
//     pub(crate) fn new_type(content: SimpleExpr, rank: i64) -> Self {
//         Self {
//             kind: Expr::val("type").into(),
//             content: content.clone(),
//             name: content.clone(),
//             rank: Expr::val(rank).into(),
//             type_: content,
//             label: util::null_as(SqlType::VARCHAR),
//         }
//     }
// }
//
// impl MergeQueryParts {
//     pub(crate) fn item_columns() -> Vec<Col> {
//         ItemRow::all_columns()
//     }
//
//     pub(crate) fn registry_variants(registry: &TagRegistry) -> SelectStatement {
//         let mut iter = registry.iter_all_for_rank();
//         let Some((first_name, first_rank)) = iter.next() else {
//             let mut q = Query::select();
//             q.expr(Expr::val(1)).and_where(Expr::val(1).eq(0));
//             return q;
//         };
//
//         let mut query =
//             ItemRow::new_type(Expr::val(first_name).into(), first_rank)
//                 .select();
//
//         for (name, rank) in iter {
//             query.union(
//                 sea_query::UnionType::Distinct,
//                 ItemRow::new_type(Expr::val(name).into(), rank).select(),
//             );
//         }
//         query
//     }
//
//     pub(crate) fn filter_new(
//         candidates: SelectStatement,
//         items_path: &str,
//     ) -> SelectStatement {
//         Query::select()
//             .columns(Self::item_columns().into_iter().map(|c| (Tbl::Item, c)))
//             .distinct()
//             .from_subquery(candidates, Tbl::Item)
//             .join_subquery(
//                 JoinType::LeftJoin,
//                 util::parquet_query(items_path),
//                 Tbl::ItemReferences,
//                 Condition::all()
//                     .add(
//                         Expr::col((Tbl::Item, Col::ItemKind)).eq(Expr::col((
//                             Tbl::ItemReferences,
//                             Col::ItemKind,
//                         ))),
//                     )
//                     .add(
//                         Expr::col((Tbl::Item, Col::Content))
//                             .eq(Expr::col((Tbl::ItemReferences, Col::Content))),
//                     ),
//             )
//             .and_where(Expr::col((Tbl::ItemReferences, Col::ItemId)).is_null())
//             .to_owned()
//     }
//
//     pub(crate) fn assign_ids(start_id: i64) -> SelectStatement {
//         Query::select()
//             .expr_as(
//                 crate::db::CustomFunc::assign_id_window(start_id),
//                 Col::ItemId,
//             )
//             .columns(Self::item_columns())
//             .from(Tbl::Item)
//             .to_owned()
//     }
// }

// ========================================================
// 3. Internal Utility (Merge context only)
// ========================================================

fn merge_and_save(
    conn: &Connection,
    path: &Path,
    temp_table: impl Iden + Clone + 'static,
    filter: Option<Condition>,
    key_col: Col,
    order_by: Option<Vec<Col>>,
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

    if let Some(cols) = order_by {
        for col in cols {
            query.order_by(col, Order::Asc);
        }
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
    use sea_query::Alias;
    use tempfile::tempdir;

    #[test]
    fn test_merge_and_save_sorting() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("test.parquet");
        let conn = Connection::open_in_memory()?;

        // 1. Initial Data (Unsorted): id=2, id=1
        conn.execute("CREATE TABLE t1 (item_id BIGINT, type VARCHAR)", [])?;
        conn.execute("INSERT INTO t1 VALUES (2, 'B'), (1, 'A')", [])?;
        conn.execute(
            &format!(
                "COPY t1 TO '{}' (FORMAT PARQUET)",
                db_path.to_string_lossy()
            ),
            [],
        )?;

        // 2. New Data (Temp Table): id=3
        let temp_table = Alias::new("temp_t");
        conn.execute("CREATE TABLE temp_t (item_id BIGINT, type VARCHAR)", [])?;
        conn.execute("INSERT INTO temp_t VALUES (3, 'C')", [])?;

        // 3. Merge with Sort (ORDER BY item_id ASC)
        // Original data (2, 1) + New (3) -> Expect (1, 2, 3)
        merge_and_save(
            &conn,
            &db_path,
            temp_table.clone(), // using alias as table name
            None,
            Col::ItemId,             // key col
            Some(vec![Col::ItemId]), // check sort by item_id
        )?;

        // 4. Verify
        let rows: Vec<i64> = conn
            .prepare(&format!(
                "SELECT item_id FROM read_parquet('{}')",
                db_path.to_string_lossy()
            ))?
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(rows, vec![1, 2, 3]);

        Ok(())
    }
}
