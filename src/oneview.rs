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

use crate::db::{BiticalType, Col, ColumnDef, TargetTable, Tbl};
use crate::query::lens_reader::Reader;
use crate::tag::TagRegistry;
use crate::types::{ItemKind, SType};
use duckdb::{Connection, Result};
use sea_query::{
    CaseStatement, Expr, Func, IntoIden, PostgresQueryBuilder, Query,
};
use std::path::Path;

// ============================================================================
// データ駆動設計のための型定義
// ============================================================================

/// タグテーブルのソース定義
#[derive(Clone, Copy)]
struct TagSource {
    table: Tbl,
    target: TargetTable,
}

/// タグテーブルのソース一覧
const TAG_SOURCES: &[TagSource] = &[
    TagSource {
        table: Tbl::BaseTags,
        target: TargetTable::BaseTags,
    },
    TagSource {
        table: Tbl::SystemTags,
        target: TargetTable::SystemTags,
    },
    TagSource {
        table: Tbl::UserTags,
        target: TargetTable::UserTags,
    },
];

/// Physical テーブル（FileReferences, Locations）のソース定義
struct PhysicalSource {
    table: Tbl,
    target: TargetTable,
    /// FileReferences との JOIN が必要か（自テーブルなら不要）
    needs_file_ref_join: bool,
}

const PHYSICAL_SOURCES: &[PhysicalSource] = &[
    PhysicalSource {
        table: Tbl::FileReferences,
        target: TargetTable::FileReferences,
        needs_file_ref_join: false,
    },
    PhysicalSource {
        table: Tbl::Locations,
        target: TargetTable::Locations,
        needs_file_ref_join: true,
    },
];

// ============================================================================
// ヘルパー関数（ロジック1箇所化）
// ============================================================================

/// label_str の COALESCE 式を生成
fn build_label_str_expr(tbl: Tbl) -> sea_query::SimpleExpr {
    Func::cust(crate::db::DuckDbFunc::Coalesce)
        .args([
            Expr::col((tbl, Col::LabelStr)).into(),
            Expr::col((tbl, Col::LabelInt))
                .cast_as(BiticalType::String)
                .into(),
            Expr::col((tbl, Col::LabelDouble))
                .cast_as(BiticalType::String)
                .into(),
            CaseStatement::new()
                .case(Expr::col((tbl, Col::LabelBool)).eq(true), "true")
                .finally("false")
                .into(),
        ])
        .into()
}

/// user_tags 上の rank 行を item ごとに1行へ畳んだソース
fn user_rank_source(user_tags_path: &str) -> sea_query::SelectStatement {
    Query::select()
        .column(Col::ItemId)
        .expr_as(Expr::col(Col::LabelInt).max(), Col::Rank)
        .from_subquery(
            crate::util::parquet_query(user_tags_path),
            Tbl::UserTags,
        )
        .and_where(Expr::col(Col::Type).eq(Col::Rank.as_str()))
        .group_by_col(Col::ItemId)
        .to_owned()
}

/// user rank を base テーブルへ LEFT JOIN する
fn join_user_rank(
    q: &mut sea_query::SelectStatement,
    base: Tbl,
    user_tags_path: &str,
) {
    q.join_subquery(
        sea_query::JoinType::LeftJoin,
        user_rank_source(user_tags_path),
        Tbl::UserRank,
        Expr::col((base, Col::ItemId))
            .eq(Expr::col((Tbl::UserRank, Col::ItemId))),
    );
}

/// rank の COALESCE 式を生成
fn build_rank_expr() -> sea_query::SimpleExpr {
    Func::cust(crate::db::DuckDbFunc::Coalesce)
        .args([
            Expr::col((Tbl::UserRank, Col::Rank)).into(),
            Expr::col((Tbl::FileReferences, Col::Rank)).into(),
            Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
            Expr::val(0).into(),
        ])
        .into()
}

/// item_kind の CASE 式を生成
/// 注意: これは FileReferences・ItemReferences・RemovedFiles の全てが JOIN
/// されている環境（Tag Source）でのみ使用可能。
/// RemovedFiles のフォールバックが必要な理由: user_tags のタグは削除後も
/// 残るが、file_references の行は消えているため、RemovedFiles を見なければ
/// 'volatile' に落ちてしまい、同じアイテムの removed_file_* 側（'file' 固定）
/// と item_kind が食い違う。
fn build_item_kind_expr() -> sea_query::SimpleExpr {
    CaseStatement::new()
        .case(
            Expr::col((Tbl::FileReferences, Col::ItemId)).is_not_null(),
            Expr::val(Into::<&'static str>::into(ItemKind::File)),
        )
        .case(
            Expr::col((Tbl::ItemReferences, Col::ItemId)).is_not_null(),
            Expr::col((Tbl::ItemReferences, Col::ItemKind)),
        )
        .case(
            Expr::col((Tbl::RemovedFiles, Col::ItemId)).is_not_null(),
            Expr::val(Into::<&'static str>::into(ItemKind::File)),
        )
        .finally(Expr::val(Into::<&'static str>::into(ItemKind::Volatile)))
        .into()
}

/// Oneview のデータソースを識別するための型
enum OneViewSource<'a> {
    /// タグテーブル (Base, System, UserTags)
    Tag(&'a TagSource),
    /// 物理テーブル (FileReferences, Locations) の一般カラム
    Physical { cd: &'a ColumnDef, tbl: Tbl },
    /// ItemReferences (非ファイルアイテム) の unpivot カラム
    ItemRef(Col),
    /// RemovedFiles の unpivot カラム (removed_file_* 型)。
    /// `ty` は投影する論理型 (removed_file_path 等)、`col` はその値を
    /// 保持する RemovedFiles 上の物理カラム (Path 等)。
    Removed { ty: SType, col: Col },
}

// ----------------------------------------------------------------------------
// 各カラムの「仕様（Spec）」を定義する関数群
// ----------------------------------------------------------------------------

fn spec_origin(source: &OneViewSource) -> sea_query::SimpleExpr {
    use crate::types::Origin;
    match source {
        OneViewSource::Tag(s) if s.table == Tbl::UserTags => {
            Expr::val(Origin::User.as_str())
                .cast_as(BiticalType::String)
                .into()
        }
        OneViewSource::Tag(s) if s.table == Tbl::SystemTags => {
            Expr::val(Origin::Builtin.as_str())
                .cast_as(BiticalType::String)
                .into()
        }
        OneViewSource::ItemRef(_) => {
            sea_query::Expr::cust(
                &crate::db::CustomFunc::item_id_origin_qualified(
                    Tbl::ItemReferences,
                    Col::ItemId,
                ),
            )
            .into()
        }
        OneViewSource::Tag(_)
        | OneViewSource::Physical { .. }
        | OneViewSource::Removed { .. } => {
            // base_tags (スキャン抽出タグ)・Physical (FileReferences/Locations)・
            // Removed (RemovedFiles) はいずれも File 由来。
            Expr::val(Origin::File.as_str())
                .cast_as(BiticalType::String)
                .into()
        }
    }
}

fn spec_rank(source: &OneViewSource) -> sea_query::SimpleExpr {
    match source {
        // Tag系は FileRefs と ItemRefs 両方を JOIN しているため、元々の build_rank_expr が使える
        OneViewSource::Tag(_) => build_rank_expr(),

        // Physical系（FileReferences, Locations）
        OneViewSource::Physical { tbl, .. } => {
            let system = if *tbl == Tbl::FileReferences {
                // FileReferences 自身なら直のカラム
                Expr::col((*tbl, Col::Rank))
            } else {
                // Locations なら FileReferences を JOIN している
                Expr::col((Tbl::FileReferences, Col::Rank))
            };
            Func::cust(crate::db::DuckDbFunc::Coalesce)
                .args([
                    Expr::col((Tbl::UserRank, Col::Rank)).into(),
                    system.into(),
                    Expr::val(0).into(),
                ])
                .into()
        }

        // ItemRef は自身のテーブルの Rank カラムを使う
        OneViewSource::ItemRef(_) => Func::cust(crate::db::DuckDbFunc::Coalesce)
            .args([
                Expr::col((Tbl::UserRank, Col::Rank)).into(),
                Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
                Expr::val(0).into(),
            ])
            .into(),

        // RemovedFiles には system rank が無いため、user rank だけを見る
        OneViewSource::Removed { .. } => Func::cust(crate::db::DuckDbFunc::Coalesce)
            .args([
                Expr::col((Tbl::UserRank, Col::Rank)).into(),
                Expr::val(0).into(),
            ])
            .into(),
    }
}

fn spec_item_kind(source: &OneViewSource) -> sea_query::SimpleExpr {
    match source {
        // Tag系は両方の JOIN があるため共通ロジックが使える
        OneViewSource::Tag(_) => {
            build_item_kind_expr().cast_as(BiticalType::String).into()
        }

        // Physical系・Removed系は常に File 確定
        OneViewSource::Physical { .. } | OneViewSource::Removed { .. } => {
            Expr::val(Into::<&'static str>::into(ItemKind::File))
                .cast_as(BiticalType::String)
                .into()
        }

        // ItemRef は自身のカラムを直接使う
        OneViewSource::ItemRef(_) => {
            Expr::col(Col::ItemKind).cast_as(BiticalType::String).into()
        }
    }
}

fn spec_type(source: &OneViewSource) -> sea_query::SimpleExpr {
    let expr = match source {
        OneViewSource::Tag(s) => Expr::col((s.table, Col::Type)),
        OneViewSource::Physical { cd, .. } => Expr::val(&cd.name[..]),
        OneViewSource::ItemRef(col) => {
            Expr::val(Into::<&'static str>::into(*col))
        }
        OneViewSource::Removed { ty, .. } => Expr::val(ty.as_str()),
    };
    expr.cast_as(BiticalType::String).into()
}

fn spec_typed_tag(source: &OneViewSource) -> sea_query::SimpleExpr {
    // 素材を集める
    let type_expr = spec_type(source);
    let val_expr: sea_query::SimpleExpr = match source {
        OneViewSource::Tag(s) => build_label_str_expr(s.table),
        OneViewSource::Physical { cd, tbl } => {
            Expr::col((*tbl, crate::util::col_to_iden(&cd.name))).into()
        }
        OneViewSource::ItemRef(col) => Expr::col(*col).into(),
        OneViewSource::Removed { col, .. } => {
            Expr::col((Tbl::RemovedFiles, *col)).into()
        }
    };

    // "type:value" 形式で結合
    Func::cust(crate::db::DuckDbFunc::Concat)
        .args([
            type_expr.into(),
            Expr::val(":").into(),
            val_expr.cast_as(BiticalType::String).into(),
        ])
        .into()
}

/// Oneview の共通カラム群を一括設定する「真の集約エンジン」
fn apply_oneview_schema(
    q: &mut sea_query::SelectStatement,
    source: OneViewSource,
) {
    let s = &source;
    add_col(Col::Origin, spec_origin, q, s);
    add_col(Col::Rank, spec_rank, q, s);
    add_col(Col::ItemKind, spec_item_kind, q, s);
    add_col(Col::Type, spec_type, q, s);
    add_col(Col::TypedTag, spec_typed_tag, q, s);
}

fn add_col(
    col: Col,
    f: fn(&OneViewSource) -> sea_query::SimpleExpr,
    q: &mut sea_query::SelectStatement,
    s: &OneViewSource,
) {
    q.expr_as(f(s), col);
}

/// ラベルカラム（label_str, label_int, label_double, label_bool）をクエリに追加
///
/// SQLTypeに応じて適切なカラムに値を設定し、他はNULLで埋める。
/// - VARCHAR/UUID → LabelStr にキャスト
/// - BIGINT → LabelInt に直接、LabelStr にもキャスト
/// - DOUBLE → LabelDouble に直接、LabelStr にもキャスト
/// - BOOLEAN → LabelBool に直接、LabelStr にもキャスト
fn apply_label_columns(
    q: &mut sea_query::SelectStatement,
    tbl: Tbl,
    iden: &sea_query::DynIden,
    bitical_type: BiticalType,
) {
    let label_col = bitical_type.to_column();

    let label_columns = [
        (Col::LabelStr, BiticalType::String),
        (Col::LabelInt, BiticalType::Integer),
        (Col::LabelDouble, BiticalType::Double),
        (Col::LabelBool, BiticalType::Boolean),
    ];

    for (col, null_type) in label_columns {
        if col == label_col {
            let expr = Expr::col((tbl, iden.clone()));
            if col == Col::LabelStr {
                q.expr_as(expr.cast_as(BiticalType::String), col);
            } else {
                q.expr_as(expr, col);
            }
        } else {
            q.expr_as(crate::util::null_as(null_type), col);
        }
    }
}

/// Physical Table (FileReferences, Locations) のカラムからクエリを生成
fn build_physical_column_query(
    cd: &ColumnDef,
    tbl_alias: Tbl,
    parquet_path: &str,
    file_ref_path: Option<&str>,
    user_tags_path: &str,
) -> String {
    let iden = crate::util::col_to_iden(&cd.name);

    let mut q = Query::select();
    q.column((tbl_alias, Col::ItemId));

    // ラベルカラムの設定（型に応じて分岐）
    apply_label_columns(&mut q, tbl_alias, &iden, cd.bitical_type);

    // 【仕様の完全集約】全共通カラムをエンジンに委託
    apply_oneview_schema(
        &mut q,
        OneViewSource::Physical { cd, tbl: tbl_alias },
    );

    q.from_subquery(crate::util::parquet_query(parquet_path), tbl_alias);

    if let Some(fr_path) = file_ref_path {
        q.join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(fr_path),
            Tbl::FileReferences,
            Expr::col((tbl_alias, Col::ItemId))
                .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
        );
    }

    join_user_rank(&mut q, tbl_alias, user_tags_path);

    q.to_string(PostgresQueryBuilder)
}

/// ItemReferences のカラムからクエリを生成
fn build_item_ref_query(
    col: Col,
    items_path: &str,
    user_tags_path: &str,
) -> String {
    let label_col = BiticalType::String.to_column();
    let mut q = Query::select();
    q.column((Tbl::ItemReferences, Col::ItemId))
        .expr_as(Expr::col((Tbl::ItemReferences, col)), label_col);

    // 【仕様の完全集約】全共通カラムをエンジンに委託
    apply_oneview_schema(&mut q, OneViewSource::ItemRef(col));

    q.from_subquery(
        crate::util::parquet_query(items_path),
        Tbl::ItemReferences,
    );

    join_user_rank(&mut q, Tbl::ItemReferences, user_tags_path);

    q.to_string(PostgresQueryBuilder)
}

/// RemovedFiles のカラムから removed_file_* 型の SELECT 文を生成
fn build_removed_file_query(
    registry: &TagRegistry,
    ty: SType,
    col: Col,
    removed_path: &str,
    user_tags_path: &str,
) -> String {
    let bitical = registry
        .get(ty.into())
        .map(|f| f.query().logical_type().to_bitical())
        .unwrap_or(BiticalType::String);

    let mut q = Query::select();
    q.column((Tbl::RemovedFiles, Col::ItemId));
    apply_label_columns(&mut q, Tbl::RemovedFiles, &col.into_iden(), bitical);
    apply_oneview_schema(&mut q, OneViewSource::Removed { ty, col });
    q.from_subquery(crate::util::parquet_query(removed_path), Tbl::RemovedFiles);
    join_user_rank(&mut q, Tbl::RemovedFiles, user_tags_path);

    q.to_string(PostgresQueryBuilder)
}

/// タグソースからSELECT文を生成
fn build_tag_query(
    source: &TagSource,
    path_fn: impl Fn(TargetTable) -> String,
) -> String {
    let tbl = source.table;

    let mut q = Query::select();
    q.column((tbl, Col::ItemId))
        .column((tbl, Col::LabelStr))
        .column((tbl, Col::LabelInt))
        .column((tbl, Col::LabelDouble))
        .column((tbl, Col::LabelBool));

    // 【仕様の完全集約】全共通カラムをエンジンに委託
    apply_oneview_schema(&mut q, OneViewSource::Tag(source));

    q.from_subquery(crate::util::parquet_query(&path_fn(source.target)), tbl)
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(&path_fn(TargetTable::FileReferences)),
            Tbl::FileReferences,
            Expr::col((tbl, Col::ItemId))
                .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
        )
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(&path_fn(TargetTable::ItemReferences)),
            Tbl::ItemReferences,
            Expr::col((tbl, Col::ItemId))
                .eq(Expr::col((Tbl::ItemReferences, Col::ItemId))),
        )
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            Query::select()
                .column(Col::ItemId)
                .distinct()
                .from_subquery(
                    crate::util::parquet_query(&path_fn(TargetTable::RemovedFiles)),
                    Tbl::RemovedFiles,
                )
                .to_owned(),
            Tbl::RemovedFiles,
            Expr::col((tbl, Col::ItemId))
                .eq(Expr::col((Tbl::RemovedFiles, Col::ItemId))),
        );

    join_user_rank(&mut q, tbl, &path_fn(TargetTable::UserTags));

    q.to_string(PostgresQueryBuilder)
}

pub struct OneView;

impl OneView {
    /// データベース上に oneview ビューを構築（または置換）します。
    pub fn recreate(
        conn: &Connection,
        registry: &TagRegistry,
        all_columns: &[ColumnDef],
        reader: Option<Reader>,
        db_dir: &Path,
    ) -> anyhow::Result<()> {
        let path = |t| {
            db_dir
                .join(format!("{}.parquet", t))
                .to_string_lossy()
                .into_owned()
        };

        let mut query_parts = Vec::new();
        let user_tags_path = path(TargetTable::UserTags);

        // Tag系テーブル（BaseTags, SystemTags, UserTags）
        for source in TAG_SOURCES {
            query_parts.push(build_tag_query(source, &path));
        }

        // 4. Physical Tables (FileReferences, Locations)
        let file_ref_path = path(TargetTable::FileReferences);
        for source in PHYSICAL_SOURCES {
            let parquet_path = path(source.target);
            let join_path =
                source.needs_file_ref_join.then_some(file_ref_path.as_str());

            // カラムごとのクエリを追加
            query_parts.extend(
                all_columns
                    .iter()
                    .filter(|c| c.target_table == source.target)
                    .map(|cd| {
                        build_physical_column_query(
                            cd,
                            source.table,
                            &parquet_path,
                            join_path,
                            &user_tags_path,
                        )
                    }),
            );
        }

        // 5. ItemReferences (非ファイルアイテム) の unpivot
        let items_path = path(TargetTable::ItemReferences);
        for col in Col::item_references_columns() {
            if col == Col::ItemId || col == Col::Rank {
                continue;
            }
            query_parts.push(build_item_ref_query(
                col,
                &items_path,
                &user_tags_path,
            ));
        }

        // 6. RemovedFiles の unpivot (removed_file_* 型)
        let removed_path = path(TargetTable::RemovedFiles);
        for (ty, col) in Col::removed_file_columns() {
            query_parts.push(build_removed_file_query(
                registry,
                ty,
                col,
                &removed_path,
                &user_tags_path,
            ));
        }

        // read 解決（reader）があれば、生の合流を中間ビュー `_oneview` に置き、
        // 解決済み SELECT 群を `oneview` として合成する（fetcher/nest は解決済みを読むだけ）。
        let oneview = sea_query::Iden::to_string(&Tbl::OneView);
        match reader {
            None => create_view_union_by_name(conn, &oneview, &query_parts)?,
            Some(reader) => {
                let _oneview = sea_query::Iden::to_string(&Tbl::_OneView);
                create_view_union_by_name(conn, &_oneview, &query_parts)?;
                create_view_union_by_name(conn, &oneview, reader.selects())?;
            }
        }

        Ok(())
    }
}

/// 指定されたSQLパーツを UNION ALL BY NAME で結合し、ビューを作成します。
fn create_view_union_by_name(
    conn: &Connection,
    view_name: &str,
    select_sqls: &[String],
) -> Result<()> {
    // DuckDB 独自の UNION ALL BY NAME を使用するため、ここでは文字列結合を行います。
    // select_sqls の各要素は sea-query で安全に構築されていることが前提です。
    let combined_sql = select_sqls.join("\nUNION ALL BY NAME\n");
    if std::env::var("TTFM_DEBUG").is_ok() {
        println!("DEBUG ONEVIEW SQL:\n{}", combined_sql);
    }
    conn.execute(
        &format!("CREATE OR REPLACE VIEW {} AS {}", view_name, combined_sql),
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::Store;
    use crate::tag::TagRegistry;
    use crate::{indexing, tagging};
    use tempfile::tempdir;

    #[test]
    fn test_oneview_consistency() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let store = Store::open(&db_dir).unwrap();
        let registry = TagRegistry::with_standard();
        indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();

        // Noteを作成してタグを付ける
        let note_id = tagging::add_item(
            &store,
            &registry,
            "note",
            "Consistency Test Memo",
        )
        .unwrap();
        crate::edit::edit(
            &store,
            &registry,
            &format!("item_id:{note_id}"),
            Some("testtag:true"),
            crate::edit::QueryType::Tag,
            None,
            crate::edit::WriteOptions { yes: true },
            &mut Vec::new(),
        )
        .unwrap();

        // oneview ビューを直接クエリして不整合をチェック
        // 同じIDなのに異なるNameまたは異なるRankを持つグループがあるか探す
        let sql = "
            SELECT item_id
            FROM oneview
            WHERE type = 'name' OR type = 'rank'
            GROUP BY item_id
            HAVING COUNT(DISTINCT label_str) > 1 OR COUNT(DISTINCT label_int) > 1
        ";

        let mut stmt = store.conn.prepare(sql).unwrap();
        let inconsistent_ids: Vec<i64> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert!(
            inconsistent_ids.is_empty(),
            "Inconsistency found in oneview for IDs: {:?}. \
             Each item must have exactly one unique Name and Rank \
             across all its tag rows.",
            inconsistent_ids
        );
    }
}
