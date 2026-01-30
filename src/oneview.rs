use crate::db::{Col, SqlType, TargetTable, Tbl, Val};
use crate::taggers::ColumnDef;
use duckdb::{Connection, Result};
use sea_query::{CaseStatement, Expr, Func, PostgresQueryBuilder, Query};
use std::path::Path;

// ============================================================================
// データ駆動設計のための型定義
// ============================================================================

/// タグテーブルのソース定義
#[derive(Clone, Copy)]
struct TagSource {
    table: Tbl,
    target: TargetTable,
    origin: Val,
}

/// タグテーブルのソース一覧
const TAG_SOURCES: &[TagSource] = &[
    TagSource {
        table: Tbl::BaseTags,
        target: TargetTable::BaseTags,
        origin: Val::System,
    },
    TagSource {
        table: Tbl::SystemTags,
        target: TargetTable::SystemTags,
        origin: Val::System,
    },
    TagSource {
        table: Tbl::UserTags,
        target: TargetTable::UserTags,
        origin: Val::User,
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
                .cast_as(SqlType::VARCHAR)
                .into(),
            Expr::col((tbl, Col::LabelDouble))
                .cast_as(SqlType::VARCHAR)
                .into(),
            CaseStatement::new()
                .case(Expr::col((tbl, Col::LabelBool)).eq(true), "true")
                .finally("false")
                .into(),
        ])
        .into()
}

/// rank の COALESCE 式を生成
fn build_rank_expr() -> sea_query::SimpleExpr {
    Func::cust(crate::db::DuckDbFunc::Coalesce)
        .args([
            Expr::col((Tbl::FileReferences, Col::Rank)).into(),
            Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
            Expr::val(0).into(),
        ])
        .into()
}

/// item_kind の CASE 式を生成
fn build_item_kind_expr() -> sea_query::SimpleExpr {
    CaseStatement::new()
        .case(
            Expr::col((Tbl::FileReferences, Col::ItemId)).is_not_null(),
            Expr::val(Into::<&'static str>::into(Val::File)),
        )
        .case(
            Expr::col((Tbl::ItemReferences, Col::ItemId)).is_not_null(),
            Expr::col((Tbl::ItemReferences, Col::ItemKind)),
        )
        .finally(Expr::val(Into::<&'static str>::into(Val::Unknown)))
        .into()
}

/// tag (typed_tag) の CONCAT 式を生成
fn build_tag_expr(tbl: Tbl, label_str_expr: sea_query::SimpleExpr) -> sea_query::SimpleExpr {
    Func::cust(crate::db::DuckDbFunc::Concat)
        .args([
            Expr::col((tbl, Col::Type))
                .cast_as(SqlType::VARCHAR)
                .into(),
            Expr::val(":").into(),
            label_str_expr.into(),
        ])
        .into()
}

/// Locations テーブルの name/filename エイリアス用クエリを生成
fn build_location_alias_query(
    type_val: Val,
    parquet_path: &str,
    file_ref_path: &str,
) -> String {
    let mut q = Query::select();
    q.column((Tbl::Locations, Col::ItemId))
        .expr_as(
            Expr::val(Into::<&'static str>::into(Val::System)).cast_as(SqlType::VARCHAR),
            Col::Origin,
        )
        .expr_as(
            Func::cust(crate::db::DuckDbFunc::Coalesce).args([
                Expr::col((Tbl::FileReferences, Col::Rank)).into(),
                Expr::val(0).into(),
            ]),
            Col::Rank,
        )
        .expr_as(
            Expr::val(Into::<&'static str>::into(Val::File)).cast_as(SqlType::VARCHAR),
            Col::ItemKind,
        )
        .expr_as(
            Expr::val(Into::<&'static str>::into(type_val)).cast_as(SqlType::VARCHAR),
            Col::Type,
        )
        .expr_as(Expr::col((Tbl::Locations, Col::Filename)), Col::LabelStr)
        .expr_as(crate::util::null_as(SqlType::BIGINT), Col::LabelInt)
        .expr_as(crate::util::null_as(SqlType::DOUBLE), Col::LabelDouble)
        .expr_as(crate::util::null_as(SqlType::BOOLEAN), Col::LabelBool)
        .expr_as(
            Func::cust(crate::db::DuckDbFunc::Concat).args([
                Expr::val(Into::<&'static str>::into(type_val))
                    .cast_as(SqlType::VARCHAR)
                    .into(),
                Expr::val(":").into(),
                Expr::col((Tbl::Locations, Col::Filename))
                    .cast_as(SqlType::VARCHAR)
                    .into(),
            ]),
            Col::TypedTag,
        )
        .from_subquery(crate::util::parquet_query(parquet_path), Tbl::Locations)
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(file_ref_path),
            Tbl::FileReferences,
            Expr::col((Tbl::Locations, Col::ItemId))
                .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
        );
    q.to_string(PostgresQueryBuilder)
}

/// タグソースからSELECT文を生成
fn build_tag_query(source: &TagSource, path_fn: impl Fn(TargetTable) -> String) -> String {
    let tbl = source.table;
    let label_str = build_label_str_expr(tbl);

    let mut q = Query::select();
    q.column((tbl, Col::ItemId))
        .expr_as(
            Expr::val(Into::<&'static str>::into(source.origin)).cast_as(SqlType::VARCHAR),
            Col::Origin,
        )
        .expr_as(build_rank_expr(), Col::Rank)
        .expr_as(build_item_kind_expr(), Col::ItemKind)
        .column((tbl, Col::Type))
        .expr_as(build_tag_expr(tbl, label_str.clone()), Col::TypedTag)
        .expr_as(label_str, Col::LabelStr)
        .column((tbl, Col::LabelInt))
        .column((tbl, Col::LabelDouble))
        .column((tbl, Col::LabelBool))
        .from_subquery(crate::util::parquet_query(&path_fn(source.target)), tbl)
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(&path_fn(TargetTable::FileReferences)),
            Tbl::FileReferences,
            Expr::col((tbl, Col::ItemId)).eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
        )
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(&path_fn(TargetTable::ItemReferences)),
            Tbl::ItemReferences,
            Expr::col((tbl, Col::ItemId)).eq(Expr::col((Tbl::ItemReferences, Col::ItemId))),
        );

    q.to_string(PostgresQueryBuilder)
}

pub struct OneView;

impl OneView {
    /// データベース上に oneview ビューを構築（または置換）します。
    pub fn recreate(
        conn: &Connection,
        all_columns: &[ColumnDef],
        db_dir: &Path,
    ) -> anyhow::Result<()> {
        let path = |t| {
            db_dir
                .join(format!("{}.parquet", t))
                .to_string_lossy()
                .into_owned()
        };

        let mut query_parts = Vec::new();

        // Tag系テーブル（BaseTags, SystemTags, UserTags）
        for source in TAG_SOURCES {
            query_parts.push(build_tag_query(source, &path));
        }

        // 4. Physical Tables (FileReferences, Locations)
        for target in [TargetTable::FileReferences, TargetTable::Locations] {
            let parquet_path = path(target);
            let tbl_alias = match target {
                TargetTable::FileReferences => Tbl::FileReferences,
                TargetTable::Locations => Tbl::Locations,
                _ => unreachable!(),
            };

            for cd in all_columns.iter().filter(|c| c.target_table == target) {
                let iden = crate::util::col_to_iden(&cd.name);
                let label_col = Col::from_sql_type(cd.sql_type);

                let mut q = Query::select();
                q.column((tbl_alias, Col::ItemId))
                    .expr_as(
                        Expr::val(Into::<&'static str>::into(Val::System))
                            .cast_as(SqlType::VARCHAR),
                        Col::Origin,
                    )
                    .expr_as(
                        Func::cust(crate::db::DuckDbFunc::Coalesce).args([
                            Expr::col((Tbl::FileReferences, Col::Rank)).into(),
                            Expr::val(0).into(),
                        ]),
                        Col::Rank,
                    )
                    .expr_as(
                        Expr::val(Into::<&'static str>::into(Val::File))
                            .cast_as(SqlType::VARCHAR),
                        Col::ItemKind,
                    )
                    .expr_as(
                        Expr::val(&cd.name[..]).cast_as(SqlType::VARCHAR),
                        Col::Type,
                    );

                if cd.sql_type == SqlType::UUID
                    || cd.sql_type == SqlType::VARCHAR
                {
                    q.expr_as(
                        Expr::col((tbl_alias, iden.clone()))
                            .cast_as(SqlType::VARCHAR),
                        Col::LabelStr,
                    );
                    q.expr_as(
                        crate::util::null_as(SqlType::BIGINT),
                        Col::LabelInt,
                    );
                    q.expr_as(
                        crate::util::null_as(SqlType::DOUBLE),
                        Col::LabelDouble,
                    );
                    q.expr_as(
                        crate::util::null_as(SqlType::BOOLEAN),
                        Col::LabelBool,
                    );
                } else {
                    q.expr_as(Expr::col((tbl_alias, iden.clone())), label_col);
                    q.expr_as(
                        Expr::col((tbl_alias, iden.clone()))
                            .cast_as(SqlType::VARCHAR),
                        Col::LabelStr,
                    );
                    // Fill others to be safe
                    if label_col != Col::LabelInt {
                        q.expr_as(
                            crate::util::null_as(SqlType::BIGINT),
                            Col::LabelInt,
                        );
                    }
                    if label_col != Col::LabelDouble {
                        q.expr_as(
                            crate::util::null_as(SqlType::DOUBLE),
                            Col::LabelDouble,
                        );
                    }
                    if label_col != Col::LabelBool {
                        q.expr_as(
                            crate::util::null_as(SqlType::BOOLEAN),
                            Col::LabelBool,
                        );
                    }
                }

                // tag column
                q.expr_as(
                    Func::cust(crate::db::DuckDbFunc::Concat).args([
                        Expr::val(&cd.name[..])
                            .cast_as(SqlType::VARCHAR)
                            .into(),
                        Expr::val(":").into(),
                        Expr::col((tbl_alias, iden))
                            .cast_as(SqlType::VARCHAR)
                            .into(),
                    ]),
                    Col::TypedTag,
                );

                q.from_subquery(
                    crate::util::parquet_query(&parquet_path),
                    tbl_alias,
                );

                if tbl_alias != Tbl::FileReferences {
                    q.join_subquery(
                        sea_query::JoinType::LeftJoin,
                        crate::util::parquet_query(&path(
                            TargetTable::FileReferences,
                        )),
                        Tbl::FileReferences,
                        Expr::col((tbl_alias, Col::ItemId))
                            .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
                    );
                }

                query_parts.push(q.to_string(PostgresQueryBuilder));
            }

            if target == TargetTable::Locations {
                let file_ref_path = path(TargetTable::FileReferences);
                query_parts.push(build_location_alias_query(Val::Name, &parquet_path, &file_ref_path));
                query_parts.push(build_location_alias_query(Val::Filename, &parquet_path, &file_ref_path));
            }
        }

        // 5. ItemReferences (非ファイルアイテム) の unpivot
        let items_path = path(TargetTable::ItemReferences);
        for col in Col::item_references_columns() {
            if col == Col::ItemId || col == Col::Rank {
                continue;
            }
            let label_col = Col::from_sql_type(SqlType::VARCHAR); // content, item_kind etc
            let mut q = Query::select();
            q.column(Col::ItemId)
                .expr_as(
                    Expr::val(Into::<&'static str>::into(Val::System))
                        .cast_as(SqlType::VARCHAR),
                    Col::Origin,
                )
                .expr_as(Expr::col(Col::Rank), Col::Rank)
                .expr_as(
                    Expr::col(Col::ItemKind).cast_as(SqlType::VARCHAR),
                    Col::ItemKind,
                )
                .expr_as(
                    Expr::val::<&str>(col.into()).cast_as(SqlType::VARCHAR),
                    Col::Type,
                )
                .expr_as(Expr::col(col), label_col)
                .expr_as(
                    Func::cust(crate::db::DuckDbFunc::Concat).args([
                        Expr::val::<&str>(col.into()).into(),
                        Expr::val(":").into(),
                        Expr::col(col).into(),
                    ]),
                    Col::TypedTag,
                )
                .from_subquery(
                    crate::util::parquet_query(&items_path),
                    Tbl::ItemReferences,
                );
            query_parts.push(q.to_string(PostgresQueryBuilder));
        }

        create_view_union_by_name(conn, "oneview", &query_parts)?;

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
    use crate::FileManager;
    use tempfile::tempdir;

    #[test]
    fn test_oneview_consistency() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

        // Noteを作成してタグを付ける
        let note_id = fm.add_item("note", "Consistency Test Memo").unwrap();
        fm.tag_item(&note_id.to_string(), "testtag:true").unwrap();

        // oneview ビューを直接クエリして不整合をチェック
        // 同じIDなのに異なるNameまたは異なるRankを持つグループがあるか探す
        let sql = "
            SELECT item_id 
            FROM oneview 
            WHERE type = 'name' OR type = 'rank'
            GROUP BY item_id 
            HAVING COUNT(DISTINCT label_str) > 1 OR COUNT(DISTINCT label_int) > 1
        ";

        let mut stmt = fm.conn.prepare(sql).unwrap();
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
