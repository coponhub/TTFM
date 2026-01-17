use crate::db::{Col, SqlType, TargetTable, Tbl, Val};
use crate::taggers::ColumnDef;
use duckdb::{Connection, Result};
use sea_query::{Expr, Iden, PostgresQueryBuilder, Query};
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

        let basic_cols: Vec<Col> = std::iter::once(Col::Type)
            .chain(Col::typed_label_columns())
            .collect();

        // 1. BaseTags
        let mut q1 = Query::select();
        q1.column(Col::ItemId)
            .expr_as(Expr::val(Val::System.to_string()), Col::Origin)
            .columns(basic_cols.clone())
            .from_subquery(
                crate::util::parquet_query(&path(TargetTable::BaseTags)),
                Tbl::BaseTags,
            );
        query_parts.push(q1.to_string(PostgresQueryBuilder));

        // 2. SystemTags
        let mut q2 = Query::select();
        q2.column(Col::ItemId)
            .expr_as(Expr::val(Val::System.to_string()), Col::Origin)
            .columns(basic_cols.clone())
            .from_subquery(
                crate::util::parquet_query(&path(TargetTable::SystemTags)),
                Tbl::SystemTags,
            );
        query_parts.push(q2.to_string(PostgresQueryBuilder));

        // 3. UserTags
        let mut q3 = Query::select();
        q3.column(Col::ItemId)
            .expr_as(Expr::val(Val::User.to_string()), Col::Origin)
            .columns(basic_cols.clone())
            .from_subquery(
                crate::util::parquet_query(&path(TargetTable::UserTags)),
                Tbl::UserTags,
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
                q.column(Col::ItemId)
                    .expr_as(Expr::val(Val::System.to_string()), Col::Origin)
                    .expr_as(Expr::val(&cd.name), Col::Type);

                if cd.sql_type == SqlType::UUID {
                    q.expr_as(
                        Expr::col(iden).cast_as(SqlType::VARCHAR),
                        label_col,
                    );
                } else {
                    q.expr_as(Expr::col(iden), label_col);
                }

                q.from_subquery(
                    crate::util::parquet_query(&parquet_path),
                    tbl_alias,
                );
                query_parts.push(q.to_string(PostgresQueryBuilder));
            }

            if target == TargetTable::FileReferences {
                let mut q_kind = Query::select();
                q_kind
                    .column(Col::ItemId)
                    .expr_as(Expr::val(Val::System.to_string()), Col::Origin)
                    .expr_as(Expr::val(Val::ItemKind.to_string()), Col::Type)
                    .expr_as(Expr::val(Val::File.to_string()), Col::LabelStr)
                    .from_subquery(
                        crate::util::parquet_query(&parquet_path),
                        Tbl::FileReferences,
                    );
                query_parts.push(q_kind.to_string(PostgresQueryBuilder));

                let mut q_rank = Query::select();
                q_rank
                    .column(Col::ItemId)
                    .expr_as(Expr::val(Val::System.to_string()), Col::Origin)
                    .expr_as(Expr::val(Val::Rank.to_string()), Col::Type)
                    .expr_as(Expr::col(Col::Rank), Col::LabelInt)
                    .from_subquery(
                        crate::util::parquet_query(&parquet_path),
                        Tbl::FileReferences,
                    );
                query_parts.push(q_rank.to_string(PostgresQueryBuilder));
            }
            if target == TargetTable::Locations {
                let mut q_name = Query::select();
                q_name
                    .column(Col::ItemId)
                    .expr_as(Expr::val(Val::System.to_string()), Col::Origin)
                    .expr_as(Expr::val(Val::Name.to_string()), Col::Type)
                    .expr_as(
                        Expr::col(Col::Filename), // use specific col not alias_from
                        Col::LabelStr,
                    )
                    .from_subquery(
                        crate::util::parquet_query(&parquet_path),
                        Tbl::Locations,
                    );
                query_parts.push(q_name.to_string(PostgresQueryBuilder));
            }
        }

        // 5. ItemReferences (非ファイルアイテム) の unpivot
        let items_path = path(TargetTable::ItemReferences);
        for col in Col::item_references_columns() {
            if col == Col::ItemId {
                continue;
            }
            let label_col = if col == Col::Rank {
                Col::LabelInt
            } else {
                Col::LabelStr
            };
            let mut q = Query::select();
            q.column(Col::ItemId)
                .expr_as(Expr::val(Val::System.to_string()), Col::Origin)
                .expr_as(Expr::val::<&str>(col.into()), Col::Type)
                .expr_as(Expr::col(col), label_col)
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
