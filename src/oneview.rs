use crate::db::{Col, SqlType, TargetTable, Tbl, Val};
use crate::taggers::ColumnDef;
use duckdb::{Connection, Result};
use sea_query::{CaseStatement, Expr, Func, PostgresQueryBuilder, Query};
use std::path::Path;

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

        let _basic_cols: Vec<Col> = std::iter::once(Col::Type)
            .chain(Col::typed_label_columns())
            .collect();

        // 1. BaseTags
        let mut q1 = Query::select();
        let label_str_expr =
            Func::cust(crate::db::DuckDbFunc::Coalesce).args([
                Expr::col((Tbl::BaseTags, Col::LabelStr)).into(),
                Expr::col((Tbl::BaseTags, Col::LabelInt))
                    .cast_as(SqlType::VARCHAR)
                    .into(),
                Expr::col((Tbl::BaseTags, Col::LabelDouble))
                    .cast_as(SqlType::VARCHAR)
                    .into(),
                CaseStatement::new()
                    .case(
                        Expr::col((Tbl::BaseTags, Col::LabelBool)).eq(true),
                        "true",
                    )
                    .finally("false")
                    .into(),
            ]);

        q1.column((Tbl::BaseTags, Col::ItemId))
            .expr_as(
                Expr::val(Into::<&'static str>::into(Val::System))
                    .cast_as(SqlType::VARCHAR),
                Col::Origin,
            )
            .expr_as(
                Func::cust(crate::db::DuckDbFunc::Coalesce).args([
                    Expr::col((Tbl::FileReferences, Col::Rank)).into(),
                    Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
                    Expr::val(0).into(),
                ]),
                Col::Rank,
            )
            .expr_as(
                CaseStatement::new()
                    .case(
                        Expr::col((Tbl::FileReferences, Col::ItemId))
                            .is_not_null(),
                        Expr::val(Into::<&'static str>::into(Val::File)),
                    )
                    .case(
                        Expr::col((Tbl::ItemReferences, Col::ItemId))
                            .is_not_null(),
                        Expr::col((Tbl::ItemReferences, Col::ItemKind)),
                    )
                    .finally(Expr::val(Into::<&'static str>::into(
                        Val::Unknown,
                    ))),
                Col::ItemKind,
            )
            .column((Tbl::BaseTags, Col::Type))
            .expr_as(
                Func::cust(crate::db::DuckDbFunc::Concat).args([
                    Expr::col((Tbl::BaseTags, Col::Type))
                        .cast_as(SqlType::VARCHAR)
                        .into(),
                    Expr::val(":").into(),
                    label_str_expr.clone().into(),
                ]),
                Col::TypedTag,
            )
            .expr_as(label_str_expr, Col::LabelStr)
            .column((Tbl::BaseTags, Col::LabelInt))
            .column((Tbl::BaseTags, Col::LabelDouble))
            .column((Tbl::BaseTags, Col::LabelBool))
            .from_subquery(
                crate::util::parquet_query(&path(TargetTable::BaseTags)),
                Tbl::BaseTags,
            )
            .join_subquery(
                sea_query::JoinType::LeftJoin,
                crate::util::parquet_query(&path(TargetTable::FileReferences)),
                Tbl::FileReferences,
                Expr::col((Tbl::BaseTags, Col::ItemId))
                    .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
            )
            .join_subquery(
                sea_query::JoinType::LeftJoin,
                crate::util::parquet_query(&path(TargetTable::ItemReferences)),
                Tbl::ItemReferences,
                Expr::col((Tbl::BaseTags, Col::ItemId))
                    .eq(Expr::col((Tbl::ItemReferences, Col::ItemId))),
            );
        query_parts.push(q1.to_string(PostgresQueryBuilder));

        // 2. SystemTags
        let mut q2 = Query::select();
        let label_str_expr_q2 = Func::cust(crate::db::DuckDbFunc::Coalesce)
            .args([
                Expr::col((Tbl::SystemTags, Col::LabelStr)).into(),
                Expr::col((Tbl::SystemTags, Col::LabelInt))
                    .cast_as(SqlType::VARCHAR)
                    .into(),
                Expr::col((Tbl::SystemTags, Col::LabelDouble))
                    .cast_as(SqlType::VARCHAR)
                    .into(),
                CaseStatement::new()
                    .case(
                        Expr::col((Tbl::SystemTags, Col::LabelBool)).eq(true),
                        "true",
                    )
                    .finally("false")
                    .into(),
            ]);

        q2.column((Tbl::SystemTags, Col::ItemId))
            .expr_as(
                Expr::val(Into::<&'static str>::into(Val::System))
                    .cast_as(SqlType::VARCHAR),
                Col::Origin,
            )
            .expr_as(
                Func::cust(crate::db::DuckDbFunc::Coalesce).args([
                    Expr::col((Tbl::FileReferences, Col::Rank)).into(),
                    Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
                    Expr::val(0).into(),
                ]),
                Col::Rank,
            )
            .expr_as(
                CaseStatement::new()
                    .case(
                        Expr::col((Tbl::FileReferences, Col::ItemId))
                            .is_not_null(),
                        Expr::val(Into::<&'static str>::into(Val::File)),
                    )
                    .case(
                        Expr::col((Tbl::ItemReferences, Col::ItemId))
                            .is_not_null(),
                        Expr::col((Tbl::ItemReferences, Col::ItemKind)),
                    )
                    .finally(Expr::val(Into::<&'static str>::into(
                        Val::Unknown,
                    ))),
                Col::ItemKind,
            )
            .column((Tbl::SystemTags, Col::Type))
            .expr_as(
                Func::cust(crate::db::DuckDbFunc::Concat).args([
                    Expr::col((Tbl::SystemTags, Col::Type)).into(),
                    Expr::val(":").into(),
                    label_str_expr_q2.clone().into(),
                ]),
                Col::TypedTag,
            )
            .expr_as(label_str_expr_q2, Col::LabelStr)
            .column((Tbl::SystemTags, Col::LabelInt))
            .column((Tbl::SystemTags, Col::LabelDouble))
            .column((Tbl::SystemTags, Col::LabelBool))
            .from_subquery(
                crate::util::parquet_query(&path(TargetTable::SystemTags)),
                Tbl::SystemTags,
            )
            .join_subquery(
                sea_query::JoinType::LeftJoin,
                crate::util::parquet_query(&path(TargetTable::FileReferences)),
                Tbl::FileReferences,
                Expr::col((Tbl::SystemTags, Col::ItemId))
                    .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
            )
            .join_subquery(
                sea_query::JoinType::LeftJoin,
                crate::util::parquet_query(&path(TargetTable::ItemReferences)),
                Tbl::ItemReferences,
                Expr::col((Tbl::SystemTags, Col::ItemId))
                    .eq(Expr::col((Tbl::ItemReferences, Col::ItemId))),
            );
        query_parts.push(q2.to_string(PostgresQueryBuilder));

        // 3. UserTags
        let mut q3 = Query::select();
        let label_str_expr_q3 = Func::cust(crate::db::DuckDbFunc::Coalesce)
            .args([
                Expr::col((Tbl::UserTags, Col::LabelStr)).into(),
                Expr::col((Tbl::UserTags, Col::LabelInt))
                    .cast_as(SqlType::VARCHAR)
                    .into(),
                Expr::col((Tbl::UserTags, Col::LabelDouble))
                    .cast_as(SqlType::VARCHAR)
                    .into(),
                CaseStatement::new()
                    .case(
                        Expr::col((Tbl::UserTags, Col::LabelBool)).eq(true),
                        "true",
                    )
                    .finally("false")
                    .into(),
            ]);

        q3.column((Tbl::UserTags, Col::ItemId))
            .expr_as(
                Expr::val(Into::<&'static str>::into(Val::User))
                    .cast_as(SqlType::VARCHAR),
                Col::Origin,
            )
            .expr_as(
                Func::cust(crate::db::DuckDbFunc::Coalesce).args([
                    Expr::col((Tbl::FileReferences, Col::Rank)).into(),
                    Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
                    Expr::val(0).into(),
                ]),
                Col::Rank,
            )
            .expr_as(
                CaseStatement::new()
                    .case(
                        Expr::col((Tbl::FileReferences, Col::ItemId))
                            .is_not_null(),
                        Expr::val(Into::<&'static str>::into(Val::File)),
                    )
                    .case(
                        Expr::col((Tbl::ItemReferences, Col::ItemId))
                            .is_not_null(),
                        Expr::col((Tbl::ItemReferences, Col::ItemKind)),
                    )
                    .finally(Expr::val(Into::<&'static str>::into(
                        Val::Unknown,
                    ))),
                Col::ItemKind,
            )
            .column((Tbl::UserTags, Col::Type))
            .expr_as(
                Func::cust(crate::db::DuckDbFunc::Concat).args([
                    Expr::col((Tbl::UserTags, Col::Type)).into(),
                    Expr::val(":").into(),
                    label_str_expr_q3.clone().into(),
                ]),
                Col::TypedTag,
            )
            .expr_as(label_str_expr_q3, Col::LabelStr)
            .column((Tbl::UserTags, Col::LabelInt))
            .column((Tbl::UserTags, Col::LabelDouble))
            .column((Tbl::UserTags, Col::LabelBool))
            .from_subquery(
                crate::util::parquet_query(&path(TargetTable::UserTags)),
                Tbl::UserTags,
            )
            .join_subquery(
                sea_query::JoinType::LeftJoin,
                crate::util::parquet_query(&path(TargetTable::FileReferences)),
                Tbl::FileReferences,
                Expr::col((Tbl::UserTags, Col::ItemId))
                    .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
            )
            .join_subquery(
                sea_query::JoinType::LeftJoin,
                crate::util::parquet_query(&path(TargetTable::ItemReferences)),
                Tbl::ItemReferences,
                Expr::col((Tbl::UserTags, Col::ItemId))
                    .eq(Expr::col((Tbl::ItemReferences, Col::ItemId))),
            );
        query_parts.push(q3.to_string(PostgresQueryBuilder));

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
                // name type
                let mut q_name = Query::select();
                q_name
                    .column((Tbl::Locations, Col::ItemId))
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
                        Expr::val(Into::<&'static str>::into(Val::Name))
                            .cast_as(SqlType::VARCHAR),
                        Col::Type,
                    )
                    .expr_as(
                        Expr::col((Tbl::Locations, Col::Filename)),
                        Col::LabelStr,
                    )
                    .expr_as(
                        crate::util::null_as(SqlType::BIGINT),
                        Col::LabelInt,
                    )
                    .expr_as(
                        crate::util::null_as(SqlType::DOUBLE),
                        Col::LabelDouble,
                    )
                    .expr_as(
                        crate::util::null_as(SqlType::BOOLEAN),
                        Col::LabelBool,
                    )
                    .expr_as(
                        Func::cust(crate::db::DuckDbFunc::Concat).args([
                            Expr::val(Into::<&'static str>::into(Val::Name))
                                .cast_as(SqlType::VARCHAR)
                                .into(),
                            Expr::val(":").into(),
                            Expr::col((Tbl::Locations, Col::Filename))
                                .cast_as(SqlType::VARCHAR)
                                .into(),
                        ]),
                        Col::TypedTag,
                    )
                    .from_subquery(
                        crate::util::parquet_query(&parquet_path),
                        Tbl::Locations,
                    )
                    .join_subquery(
                        sea_query::JoinType::LeftJoin,
                        crate::util::parquet_query(&path(
                            TargetTable::FileReferences,
                        )),
                        Tbl::FileReferences,
                        Expr::col((Tbl::Locations, Col::ItemId))
                            .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
                    );
                query_parts.push(q_name.to_string(PostgresQueryBuilder));

                // filename type
                let mut q_filename = Query::select();
                q_filename
                    .column((Tbl::Locations, Col::ItemId))
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
                        Expr::val(Into::<&'static str>::into(Val::Filename))
                            .cast_as(SqlType::VARCHAR),
                        Col::Type,
                    )
                    .expr_as(
                        Expr::col((Tbl::Locations, Col::Filename)),
                        Col::LabelStr,
                    )
                    .expr_as(
                        crate::util::null_as(SqlType::BIGINT),
                        Col::LabelInt,
                    )
                    .expr_as(
                        crate::util::null_as(SqlType::DOUBLE),
                        Col::LabelDouble,
                    )
                    .expr_as(
                        crate::util::null_as(SqlType::BOOLEAN),
                        Col::LabelBool,
                    )
                    .expr_as(
                        Func::cust(crate::db::DuckDbFunc::Concat).args([
                            Expr::val(Into::<&'static str>::into(
                                Val::Filename,
                            ))
                            .cast_as(SqlType::VARCHAR)
                            .into(),
                            Expr::val(":").into(),
                            Expr::col((Tbl::Locations, Col::Filename))
                                .cast_as(SqlType::VARCHAR)
                                .into(),
                        ]),
                        Col::TypedTag,
                    )
                    .from_subquery(
                        crate::util::parquet_query(&parquet_path),
                        Tbl::Locations,
                    )
                    .join_subquery(
                        sea_query::JoinType::LeftJoin,
                        crate::util::parquet_query(&path(
                            TargetTable::FileReferences,
                        )),
                        Tbl::FileReferences,
                        Expr::col((Tbl::Locations, Col::ItemId))
                            .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
                    );
                query_parts.push(q_filename.to_string(PostgresQueryBuilder));
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
