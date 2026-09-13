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

use super::{append_to_target, union_and_save};
use crate::db::{BiticalType, Col, CustomFunc, Store, TargetTable, Tbl};
use crate::indexing::indexer::TaggingResult;
use crate::tag::TagRegistry;
use crate::types::{Bitical, ItemId};
use crate::util::{self, ExecuteSql, IdenExt};
use anyhow::Result;
use duckdb::{Connection, ToSql};
use sea_query::{
    CaseStatement, Condition, Expr, Func, Iden, JoinType, Order, Query,
    SelectStatement, UnionType,
};
// use sea_query::{JoinType, SimpleExpr};
use std::path::{Path, PathBuf};

// ========================================================
// Merge Phase Orchestrator
// ========================================================

pub(crate) fn run_merge(
    conn: &Connection,
    registry: &TagRegistry,
    store: &Store,
    results: Vec<TaggingResult>,
    dir_changed: Vec<TaggingResult>,
    deleted_ids: Vec<ItemId>,
    temp_scan_path: &Path,
    temp_live_path: &Path,
    roots: &[PathBuf],
    update_sys_fn: impl Fn(Option<SelectStatement>) -> Result<()>,
) -> Result<Vec<i64>> {
    // 各テーブルの取り込みと同期を実行

    // 実体・場所テーブルが上書きされる前に退避する必要がある。
    record_removed_files(conn, store, &deleted_ids)?;

    // A. 実体テーブル: 新規登録と、削除 ID の行の除去。
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
    .ingest(&results, &dir_changed)?
    .sync(temp_live_path, roots)?;

    // C. タグテーブル (可変): 削除 ID の分を掃除 (DirChanged は base_tags 抽出をスキップしたため results のみ)。
    let tag = BaseTagMerger {
        conn,
        registry,
        store,
    }
    .prepare()?
    .ingest(&results)?
    .sync(&deleted_ids)?;

    // D. パス依存タグテーブル (可変): locations と同様に常に更新。
    let loc_tag = LocationTagMerger {
        conn,
        registry,
        store,
    }
    .prepare()?
    .ingest(&results, &dir_changed)?
    .sync(&deleted_ids)?;

    ent.cleanup()?;
    loc.cleanup()?;
    tag.cleanup()?;
    loc_tag.cleanup()?;

    // システムアイテム（基本Type定義のみ）の更新
    // 以前はここで type/label/tag の全バリエーションを登録していたが、
    // oneview のプロジェクションにより不要になったため廃止した。
    update_sys_fn(None)?;

    // クリーンアップ
    std::fs::remove_file(temp_scan_path).ok();
    std::fs::remove_file(temp_live_path).ok();

    let mut modified_item_ids = Vec::new();
    for r in results.iter().chain(dir_changed.iter()) {
        modified_item_ids.push(r.location_row.id);
    }
    Ok(modified_item_ids)
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

        let mut seen: rustc_hash::FxHashSet<i64> =
            rustc_hash::FxHashSet::default();
        for res in results {
            if !seen.insert(res.entity_row.id) {
                continue;
            }
            let mut er = vec![&res.entity_row.id as &dyn ToSql];
            er.extend(res.entity_row.values.iter().map(|v| v as &dyn ToSql));
            app.append_row(er.as_slice())?;
        }
        Ok(self)
    }

    fn inherit_rank(&self) -> Result<()> {
        let path = self.store.path_for_target(TargetTable::FileReferences);
        if !path.exists() {
            return Ok(());
        }
        let existing_rank = Query::select()
            .expr(CustomFunc::any_value(Expr::col((
                Tbl::FileReferences,
                Col::Rank,
            ))))
            .from_subquery(
                util::parquet_query(&path.to_string_lossy()),
                Tbl::FileReferences,
            )
            .and_where(
                Expr::col((Tbl::FileReferences, Col::ItemId))
                    .equals((Tbl::FileReferencesDiff, Col::ItemId)),
            )
            .to_owned();

        Query::update()
            .table(Tbl::FileReferencesDiff)
            .value(
                Col::Rank,
                Func::cust(crate::db::DuckDbFunc::Coalesce).args([
                    sea_query::SimpleExpr::SubQuery(
                        None,
                        Box::new(existing_rank.into_sub_query_statement()),
                    ),
                    Expr::val(0i64).into(),
                ]),
            )
            .execute(self.conn)?;
        Ok(())
    }

    pub(crate) fn sync(self, deleted_ids: &[ItemId]) -> Result<Self> {
        self.inherit_rank()?;
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
        dir_changed: &[TaggingResult],
    ) -> Result<Self> {
        if results.is_empty() && dir_changed.is_empty() {
            return Ok(self);
        }
        let table_name = Tbl::LocationsDiff.to_string().replace('"', "");
        let mut app = self.conn.appender(&table_name)?;

        for res in results.iter().chain(dir_changed.iter()) {
            let mut lr = vec![&res.location_row.id as &dyn ToSql];
            lr.extend(res.location_row.values.iter().map(|v| v as &dyn ToSql));
            lr.push(&res.scan_hash as &dyn ToSql);
            lr.push(&res.basename_scan_hash as &dyn ToSql);
            app.append_row(lr.as_slice())?;
        }
        Ok(self)
    }

    pub(crate) fn sync(
        self,
        live_path: &Path,
        roots: &[PathBuf],
    ) -> Result<Self> {
        let path_str = live_path.to_string_lossy();
        let live_query = Query::select()
            .column(Col::ScanHash)
            .from_subquery(util::parquet_query(&path_str), Tbl::Live)
            .to_owned();

        // locations は path をキーにして上書き判定を行う（ハードリンク対応）。
        // 生存フィルタは item_id ではなく scan_hash で絞る。item_id 単位だと
        // 同一アイテムの他ロケーションが生きているだけで、消えたロケーション
        // も道連れに残ってしまう。roots 範囲外の location は今回のスキャン対象
        // 外なので、生存判定と関係なく無条件に保持する。
        merge_and_save(
            self.conn,
            &self.store.path_for_target(TargetTable::Locations),
            Tbl::LocationsDiff,
            Some(
                Condition::any()
                    .add(Expr::col(Col::ScanHash).in_subquery(live_query))
                    .add(super::diff::in_scope(Col::Path, roots).not()),
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
        let mut seen: rustc_hash::FxHashSet<(i64, &str)> =
            rustc_hash::FxHashSet::default();

        for res in results {
            for t in &res.tags {
                if seen.insert((t.item_id, t.tag_type.as_str())) {
                    let (stored_col, stored) = t.value.to_col_value();
                    let none = None::<Bitical>;
                    let mut row: Vec<&dyn ToSql> =
                        vec![&t.item_id, &t.tag_type];
                    for col in BiticalType::to_columns() {
                        row.push(if col == stored_col {
                            &stored
                        } else {
                            &none
                        });
                    }
                    app.append_row(row.as_slice())?;
                }
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

pub(crate) struct LocationTagMerger<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) registry: &'a TagRegistry,
    pub(crate) store: &'a Store,
}

impl<'a> LocationTagMerger<'a> {
    pub(crate) fn prepare(self) -> Result<Self> {
        let all_cols = self.registry.get_all_columns();
        crate::db::Schema::build_table(
            TargetTable::TagsByLocation,
            Tbl::TagsByLocationDiff,
            &all_cols,
        )
        .temporary()
        .execute(self.conn)?;
        Ok(self)
    }

    pub(crate) fn ingest(
        self,
        results: &[TaggingResult],
        dir_changed: &[TaggingResult],
    ) -> Result<Self> {
        if results.is_empty() && dir_changed.is_empty() {
            return Ok(self);
        }
        let table_name = Tbl::TagsByLocationDiff.to_string().replace('"', "");
        let mut app = self.conn.appender(&table_name)?;
        let mut seen: rustc_hash::FxHashSet<(i64, &str, String)> =
            rustc_hash::FxHashSet::default();

        for res in results.iter().chain(dir_changed.iter()) {
            for t in &res.location_tags {
                let (stored_col, stored) = t.value.to_col_value();
                let val_str = stored.as_display_name();
                let key = (t.item_id, t.tag_type.as_str(), val_str);
                if seen.insert(key) {
                    let none = None::<Bitical>;
                    let mut row: Vec<&dyn ToSql> =
                        vec![&t.item_id, &t.tag_type];
                    for col in BiticalType::to_columns() {
                        row.push(if col == stored_col {
                            &stored
                        } else {
                            &none
                        });
                    }
                    app.append_row(row.as_slice())?;
                }
            }
        }
        Ok(self)
    }

    pub(crate) fn sync(self, _deleted_ids: &[ItemId]) -> Result<Self> {
        let target_path =
            self.store.path_for_target(TargetTable::TagsByLocation);
        let loc_path = self.store.path_for_target(TargetTable::Locations);
        sync_location_tags(self.conn, &target_path, &loc_path)?;
        Ok(self)
    }

    pub(crate) fn cleanup(self) -> Result<()> {
        Tbl::TagsByLocationDiff.drop_table(self.conn).ok();
        Ok(())
    }
}

fn build_stem_expr() -> sea_query::SimpleExpr {
    let ext_col = Expr::col((Tbl::Locations, Col::Extension));
    let name_col = Expr::col((Tbl::Locations, Col::Filename));

    let ext_not_empty =
        ext_col.clone().is_not_null().and(ext_col.clone().ne(""));

    let lower_name = Func::lower(name_col.clone());
    let lower_ext = Func::lower(ext_col.clone());
    let dot_ext = Func::cust(crate::db::DuckDbFunc::Concat)
        .args([Expr::val(".").into(), lower_ext.into()]);
    let name_ends_with_ext: sea_query::SimpleExpr =
        Func::cust(crate::db::DuckDbFunc::EndsWith)
            .args([lower_name.into(), dot_ext.into()])
            .into();

    let name_len = Func::char_length(name_col.clone());
    let ext_len = Func::char_length(ext_col);

    let stem_len = Expr::expr(name_len.clone())
        .sub(Expr::expr(ext_len))
        .sub(1i64);

    let stem_substr = Func::cust(crate::db::DuckDbFunc::Substr).args([
        name_col.clone().into(),
        Expr::val(1i64).into(),
        stem_len.into(),
    ]);

    // 末尾が '.' で終わるファイル名（例: "a."）の幹名は "a"（Rust の Path::file_stem() と同一）
    let ends_with_dot: sea_query::SimpleExpr =
        Func::cust(crate::db::DuckDbFunc::EndsWith)
            .args([name_col.clone().into(), Expr::val(".").into()])
            .into();
    let name_len_gt_1 = Expr::expr(name_len.clone()).gt(1i64);
    let dot_stem_substr = Func::cust(crate::db::DuckDbFunc::Substr).args([
        name_col.clone().into(),
        Expr::val(1i64).into(),
        Expr::expr(name_len).sub(1i64).into(),
    ]);

    CaseStatement::new()
        .case(ext_not_empty.and(name_ends_with_ext), stem_substr)
        .case(name_len_gt_1.and(ends_with_dot), dot_stem_substr)
        .finally(name_col)
        .into()
}

fn build_location_exists_query(loc_path: &Path) -> SelectStatement {
    let loc_subquery = util::parquet_query(&loc_path.to_string_lossy());
    let stem_expr = build_stem_expr();

    let tbl_tag = Tbl::TagsByLocation;
    let tbl_loc = Tbl::Locations;

    let mut q = Query::select();
    q.expr(Expr::val(1i64))
        .from_subquery(loc_subquery, tbl_loc)
        .and_where(
            Expr::col((tbl_loc, Col::ItemId)).equals((tbl_tag, Col::ItemId)),
        )
        .cond_where(
            Condition::any()
                .add(
                    Expr::col((tbl_tag, Col::Type)).eq("path").and(
                        Expr::col((tbl_tag, Col::LabelStr))
                            .equals((tbl_loc, Col::Path)),
                    ),
                )
                .add(
                    Expr::col((tbl_tag, Col::Type)).eq("filename").and(
                        Expr::col((tbl_tag, Col::LabelStr))
                            .equals((tbl_loc, Col::Filename)),
                    ),
                )
                .add(
                    Expr::col((tbl_tag, Col::Type)).eq("parentdir").and(
                        Expr::col((tbl_tag, Col::LabelStr))
                            .equals((tbl_loc, Col::Parentdir)),
                    ),
                )
                .add(
                    Expr::col((tbl_tag, Col::Type)).eq("extension").and(
                        Expr::col((tbl_tag, Col::LabelStr))
                            .equals((tbl_loc, Col::Extension)),
                    ),
                )
                .add(
                    Expr::col((tbl_tag, Col::Type))
                        .eq("stem")
                        .and(Expr::col((tbl_tag, Col::LabelStr)).eq(stem_expr)),
                )
                .add(Expr::col((tbl_tag, Col::Type)).is_not_in([
                    "path",
                    "filename",
                    "parentdir",
                    "extension",
                    "stem",
                ])),
        );
    q
}

fn sync_location_tags(
    conn: &Connection,
    target_path: &Path,
    loc_path: &Path,
) -> Result<()> {
    let cols = [
        Col::ItemId,
        Col::Type,
        Col::LabelStr,
        Col::LabelInt,
        Col::LabelDouble,
        Col::LabelBool,
    ];

    let diff_q = Query::select()
        .columns(cols)
        .from(Tbl::TagsByLocationDiff)
        .to_owned();

    let source_q = if target_path.exists() {
        let mut existing_q = Query::select();
        existing_q.columns(cols).from_subquery(
            util::parquet_query(&target_path.to_string_lossy()),
            Tbl::TagsByLocation,
        );
        existing_q.union(UnionType::All, diff_q);
        existing_q
    } else {
        diff_q
    };

    let mut main_q = Query::select();
    main_q
        .distinct()
        .columns([
            (Tbl::TagsByLocation, Col::ItemId),
            (Tbl::TagsByLocation, Col::Type),
            (Tbl::TagsByLocation, Col::LabelStr),
            (Tbl::TagsByLocation, Col::LabelInt),
            (Tbl::TagsByLocation, Col::LabelDouble),
            (Tbl::TagsByLocation, Col::LabelBool),
        ])
        .from_subquery(source_q, Tbl::TagsByLocation)
        .order_by((Tbl::TagsByLocation, Col::Type), Order::Asc)
        .order_by((Tbl::TagsByLocation, Col::LabelInt), Order::Asc)
        .order_by((Tbl::TagsByLocation, Col::LabelStr), Order::Asc)
        .order_by((Tbl::TagsByLocation, Col::ItemId), Order::Asc);

    if loc_path.exists() {
        let exists_q = build_location_exists_query(loc_path);
        main_q.and_where(Expr::exists(exists_q));
    } else {
        main_q.and_where(Expr::val(1i64).eq(0i64));
    }

    util::save_parquet(conn, &main_q, target_path, None)?;
    Ok(())
}

pub(crate) fn record_removed_files(
    conn: &Connection,
    store: &Store,
    deleted_ids: &[ItemId],
) -> Result<()> {
    if deleted_ids.is_empty() {
        return Ok(());
    }
    let ids_i64: Vec<i64> = deleted_ids.iter().map(|id| id.as_i64()).collect();
    let path =
        |t| util::parquet_query(&store.path_for_target(t).to_string_lossy());

    let tagged = Query::select()
        .distinct()
        .column(Col::ItemId)
        .from_subquery(path(TargetTable::UserTags), Tbl::UserTags)
        .to_owned();

    let rows = Query::select()
        .column((Tbl::Locations, Col::ItemId))
        .column((Tbl::FileReferences, Col::Rank))
        .column((Tbl::FileReferences, Col::FileId))
        .columns([
            (Tbl::Locations, Col::ScanHash),
            (Tbl::Locations, Col::BasenameScanHash),
            (Tbl::Locations, Col::Path),
        ])
        .columns([
            (Tbl::FileReferences, Col::Size),
            (Tbl::FileReferences, Col::Mtime),
            (Tbl::FileReferences, Col::IsDir),
        ])
        .expr_as(
            Expr::cust("CAST(epoch(now()) AS BIGINT)"),
            Col::RemovedFileAt,
        )
        .from_subquery(path(TargetTable::Locations), Tbl::Locations)
        .join_subquery(
            JoinType::InnerJoin,
            path(TargetTable::FileReferences),
            Tbl::FileReferences,
            Expr::col((Tbl::Locations, Col::ItemId))
                .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
        )
        .and_where(Expr::col((Tbl::Locations, Col::ItemId)).is_in(ids_i64))
        .and_where(Expr::col((Tbl::Locations, Col::ItemId)).in_subquery(tagged))
        .to_owned();

    append_to_target(conn, store, TargetTable::RemovedFiles, rows)
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
//             label: util::null_as(BiticalType::String),
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

    union_and_save(conn, path, base_query, |query| {
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
    })
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

    #[test]
    fn test_location_tag_merger_sync() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(dir.path().join("db"))?;
        let registry = TagRegistry::new();
        let merger = LocationTagMerger {
            conn: &store.conn,
            registry: &registry,
            store: &store,
        };
        merger.prepare()?.sync(&[])?.cleanup()?;
        assert!(store.path_for_target(TargetTable::TagsByLocation).exists());
        Ok(())
    }

    fn insert_dummy_locations(
        store: &Store,
        item_id: i64,
        paths: &[&str],
    ) -> Result<()> {
        let p = store.path_for_target(TargetTable::Locations);
        let mut rows = Vec::new();
        for path_str in paths {
            let path = Path::new(path_str);
            let parent = path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let filename = path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            rows.push(format!(
                "({item_id}, '{path_str}', '{parent}', '{filename}', \
                 '{ext}', 0, 0)"
            ));
        }
        let sql = format!(
            "COPY (SELECT * FROM (VALUES {}) AS t(item_id, path, \
             parentdir, filename, extension, scan_hash, \
             basename_scan_hash)) TO '{}' (FORMAT PARQUET)",
            rows.join(", "),
            p.to_string_lossy().replace('\'', "''")
        );
        store.conn.execute(&sql, [])?;
        Ok(())
    }

    #[test]
    fn test_location_and_base_tag_merger_deduplicates_hardlinks() {
        use crate::indexing::indexer::{DynamicRow, ScanHash, TagRow};
        use sea_query::PostgresQueryBuilder;

        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();

        insert_dummy_locations(&store, 1, &["/path/a.txt", "/path/b.txt"])
            .unwrap();

        let make_res = |path: &str| TaggingResult {
            entity_row: DynamicRow {
                id: 1,
                values: vec![Some(Bitical::Integer(0))],
            },
            location_row: DynamicRow {
                id: 1,
                values: vec![Some(Bitical::String(path.to_string()))],
            },
            tags: vec![TagRow {
                item_id: 1,
                tag_type: "size".to_string(),
                value: Bitical::Integer(100),
            }],
            location_tags: vec![TagRow {
                item_id: 1,
                tag_type: "path".to_string(),
                value: Bitical::String(path.to_string()),
            }],
            scan_hash: ScanHash(0),
            basename_scan_hash: ScanHash(0),
        };
        let results = vec![make_res("/path/a.txt"), make_res("/path/b.txt")];

        let base_merger = BaseTagMerger {
            conn: &store.conn,
            registry: &registry,
            store: &store,
        };
        base_merger
            .prepare()
            .unwrap()
            .ingest(&results)
            .unwrap()
            .sync(&[])
            .unwrap();

        let loc_merger = LocationTagMerger {
            conn: &store.conn,
            registry: &registry,
            store: &store,
        };
        loc_merger
            .prepare()
            .unwrap()
            .ingest(&results, &[])
            .unwrap()
            .sync(&[])
            .unwrap();

        let base_p = store.path_for_target(TargetTable::BaseTags);
        let mut q = Query::select();
        q.expr(Func::count(Expr::col(sea_query::Asterisk)))
            .from_subquery(
                util::parquet_query(&base_p.to_string_lossy()),
                Tbl::BaseTags,
            )
            .and_where(Expr::col((Tbl::BaseTags, Col::ItemId)).eq(1))
            .and_where(Expr::col((Tbl::BaseTags, Col::Type)).eq("size"));
        let sql = q.to_string(PostgresQueryBuilder);
        let count: i64 = store.conn.query_row(&sql, [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);

        let loc_p = store.path_for_target(TargetTable::TagsByLocation);
        let mut loc_q = Query::select();
        loc_q
            .expr(Func::count(Expr::col(sea_query::Asterisk)))
            .from_subquery(
                util::parquet_query(&loc_p.to_string_lossy()),
                Tbl::TagsByLocation,
            )
            .and_where(Expr::col((Tbl::TagsByLocation, Col::ItemId)).eq(1))
            .and_where(Expr::col((Tbl::TagsByLocation, Col::Type)).eq("path"));
        let loc_sql = loc_q.to_string(PostgresQueryBuilder);
        let loc_count: i64 =
            store.conn.query_row(&loc_sql, [], |r| r.get(0)).unwrap();
        assert_eq!(loc_count, 2);
    }

    #[test]
    fn test_location_tag_merger_incremental_hardlink_preserves_existing() {
        use crate::indexing::indexer::{DynamicRow, ScanHash, TagRow};

        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();

        insert_dummy_locations(&store, 1, &["/path/a.txt", "/path/b.txt"])
            .unwrap();

        let make_res = |path: &str| TaggingResult {
            entity_row: DynamicRow {
                id: 1,
                values: vec![Some(Bitical::Integer(0))],
            },
            location_row: DynamicRow {
                id: 1,
                values: vec![Some(Bitical::String(path.to_string()))],
            },
            tags: vec![],
            location_tags: vec![TagRow {
                item_id: 1,
                tag_type: "path".to_string(),
                value: Bitical::String(path.to_string()),
            }],
            scan_hash: ScanHash(0),
            basename_scan_hash: ScanHash(0),
        };

        // 1. 初回インデックス: a.txt のみ
        let loc_merger1 = LocationTagMerger {
            conn: &store.conn,
            registry: &registry,
            store: &store,
        };
        loc_merger1
            .prepare()
            .unwrap()
            .ingest(&[make_res("/path/a.txt")], &[])
            .unwrap()
            .sync(&[])
            .unwrap()
            .cleanup()
            .unwrap();

        // 2. インクリメンタルインデックス: b.txt のみ
        let loc_merger2 = LocationTagMerger {
            conn: &store.conn,
            registry: &registry,
            store: &store,
        };
        loc_merger2
            .prepare()
            .unwrap()
            .ingest(&[make_res("/path/b.txt")], &[])
            .unwrap()
            .sync(&[])
            .unwrap()
            .cleanup()
            .unwrap();

        // 3. a.txt と b.txt の両方が残っていることを検証
        let target_p = store.path_for_target(TargetTable::TagsByLocation);
        let count: i64 = store
            .conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}') \
                     WHERE item_id = 1 AND type = 'path'",
                    target_p.display()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_build_stem_expr_case_insensitive_and_trailing_dot() -> Result<()> {
        use sea_query::PostgresQueryBuilder;

        let conn = Connection::open_in_memory()?;
        conn.execute(
            "CREATE TABLE locations (filename VARCHAR, extension VARCHAR)",
            [],
        )?;
        conn.execute(
            "INSERT INTO locations VALUES 
             ('photo.JPG', 'jpg'),
             ('archive.tar.GZ', 'gz'),
             ('document.pdf', 'pdf'),
             ('no_ext', ''),
             ('trailing_dot.', '')",
            [],
        )?;

        let stem_expr = build_stem_expr();
        let mut q = Query::select();
        q.column(Col::Filename)
            .expr_as(stem_expr, Alias::new("stem"))
            .from(Tbl::Locations)
            .order_by(Col::Filename, Order::Asc);

        let sql = q.to_string(PostgresQueryBuilder);
        let rows: Vec<(String, String)> = conn
            .prepare(&sql)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(
            rows,
            vec![
                ("archive.tar.GZ".to_string(), "archive.tar".to_string()),
                ("document.pdf".to_string(), "document".to_string()),
                ("no_ext".to_string(), "no_ext".to_string()),
                ("photo.JPG".to_string(), "photo".to_string()),
                ("trailing_dot.".to_string(), "trailing_dot".to_string()),
            ]
        );

        Ok(())
    }
}
