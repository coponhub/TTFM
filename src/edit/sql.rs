use crate::db::{Col, Tbl};
use crate::types::{Bitical, TypedTag};
use crate::util::parquet_query;
use sea_query::{
    CaseStatement, Expr, Order, Query, SelectStatement, SimpleExpr, UnionType,
    UpdateStatement,
};

pub(crate) struct UserTagDelete {
    pub(crate) item_id: i64,
    pub(crate) tag_type: String,
    pub(crate) value: Option<Bitical>,
}

pub(crate) fn item_references_write(
    path: &str,
    inserts: Vec<(i64, String, String)>,
    cascade_ids: &[i64],
) -> SelectStatement {
    let mut q = parquet_query(path);
    if !cascade_ids.is_empty() {
        q.and_where(Expr::col(Col::ItemId).is_not_in(cascade_ids.to_vec()));
    }
    if !inserts.is_empty() {
        let rows: Vec<(i64, Option<i64>, Option<String>, String, String)> = inserts
            .into_iter()
            .map(|(item_id, item_kind, content)| {
                (item_id, None, None, item_kind, content)
            })
            .collect();
        q.union(
            UnionType::All,
            Query::select()
                .column(sea_query::Asterisk)
                .from_values(rows, crate::db::Pronoun::Sub)
                .to_owned(),
        );
    }
    q.order_by(Col::ItemId, Order::Asc);
    q
}

pub(crate) fn user_tags_write(
    path: &str,
    inserts: Vec<(i64, TypedTag)>,
    deletes: Vec<UserTagDelete>,
    cascade_ids: &[i64],
) -> SelectStatement {
    let mut q = parquet_query(path);

    if !cascade_ids.is_empty() {
        q.and_where(Expr::col(Col::ItemId).is_not_in(cascade_ids.to_vec()));
    }

    for d in &deletes {
        let mut row_match = Expr::col(Col::ItemId)
            .eq(d.item_id)
            .and(Expr::col(Col::Type).eq(d.tag_type.clone()));
        if let Some(ref v) = d.value {
            let (col, val_expr) = v.to_col_expr();
            row_match = row_match.and(Expr::col(col).eq(val_expr));
        }
        q.and_where(row_match.not());
    }

    if !inserts.is_empty() {
        let rows: Vec<(i64, String, Option<String>, Option<i64>, Option<f64>, Option<bool>)> =
            inserts
                .into_iter()
                .map(|(item_id, tag)| {
                    let tag_type = tag.tag_type().to_string();
                    let (ls, li, ld, lb) =
                        Bitical::to_eav_columns(Some(tag.value()));
                    (item_id, tag_type, ls, li, ld, lb)
                })
                .collect();
        q.union(
            UnionType::All,
            Query::select()
                .column(sea_query::Asterisk)
                .from_values(rows, crate::db::Pronoun::Sub)
                .to_owned(),
        );
    }

    q.order_by(Col::Type, Order::Asc)
        .order_by(Col::LabelInt, Order::Asc)
        .order_by(Col::LabelStr, Order::Asc)
        .order_by(Col::ItemId, Order::Asc);
    q
}

pub(crate) fn rank_case_update(
    tmp: Tbl,
    updates: &[(i64, i64)],
) -> UpdateStatement {
    let rank_case: SimpleExpr = updates
        .iter()
        .fold(CaseStatement::new(), |acc, (id, rank)| {
            acc.case(Expr::col(Col::ItemId).eq(*id), *rank)
        })
        .finally(Expr::col(Col::Rank))
        .into();

    Query::update()
        .table(tmp)
        .value(Col::Rank, rank_case)
        .and_where(
            Expr::col(Col::ItemId).is_in(
                updates
                    .iter()
                    .map(|(id, _)| sea_query::Value::from(*id))
                    .collect::<Vec<_>>(),
            ),
        )
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::save_parquet;
    use duckdb::Connection;
    use tempfile::tempdir;

    // 既存の user_tags.parquet を空スキーマで用意する。
    fn make_empty_user_tags(conn: &Connection, path: &std::path::Path) {
        conn.execute(
            &format!(
                "COPY (SELECT \
                    1::BIGINT AS item_id, \
                    'x'::VARCHAR AS type, \
                    'x'::VARCHAR AS label_str, \
                    1::BIGINT AS label_int, \
                    1.0::DOUBLE AS label_dbl, \
                    true AS label_bool \
                 WHERE FALSE) TO '{}' (FORMAT 'parquet')",
                path.to_string_lossy()
            ),
            [],
        )
        .unwrap();
    }

    // 既存の item_references.parquet を空スキーマで用意する。
    fn make_empty_item_references(conn: &Connection, path: &std::path::Path) {
        conn.execute(
            &format!(
                "COPY (SELECT \
                    1::BIGINT AS item_id, \
                    1::BIGINT AS rank, \
                    'x'::VARCHAR AS name, \
                    'x'::VARCHAR AS item_kind, \
                    'x'::VARCHAR AS content \
                 WHERE FALSE) TO '{}' (FORMAT 'parquet')",
                path.to_string_lossy()
            ),
            [],
        )
        .unwrap();
    }

    // 挿入内容は正しく反映される（少数件で実際に実行して確認）。
    #[test]
    fn item_ref_insert_rows_are_all_present() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item_references.parquet");
        let conn = Connection::open_in_memory().unwrap();
        make_empty_item_references(&conn, &path);

        let inserts = vec![
            (1i64, "note".to_string(), "a".to_string()),
            (2i64, "note".to_string(), "b".to_string()),
            (3i64, "note".to_string(), "c".to_string()),
        ];
        let q = item_references_write(&path.to_string_lossy(), inserts, &[]);
        save_parquet(&conn, &q, &path, None).unwrap();

        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}')",
                    path.to_string_lossy()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    // user_tags_write と同じ理由（挿入行1件ごとの UNION ALL 分岐は分岐数に対して
    // 破滅的にスケールする）で、item_references_write でも分岐数が挿入件数に
    // 比例しないことを保証する。
    #[test]
    fn item_ref_union_all_branch_count_does_not_scale_with_insert_count() {
        fn union_all_count(n: usize) -> usize {
            let inserts: Vec<(i64, String, String)> = (0..n as i64)
                .map(|i| (i, "note".to_string(), format!("v{i}")))
                .collect();
            let q = item_references_write("dummy_path.parquet", inserts, &[]);
            let sql = q.to_string(sea_query::PostgresQueryBuilder);
            sql.matches("UNION ALL").count()
        }

        let small = union_all_count(3);
        let large = union_all_count(2000);
        assert_eq!(
            small, large,
            "UNION ALL branch count must not grow with the number of inserted rows"
        );
    }

    // 挿入内容は正しく反映される（少数件で実際に実行して確認）。
    #[test]
    fn insert_rows_are_all_present() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("user_tags.parquet");
        let conn = Connection::open_in_memory().unwrap();
        make_empty_user_tags(&conn, &path);

        let inserts = vec![
            (1i64, TypedTag::new("cate", "a")),
            (2i64, TypedTag::new("cate", "b")),
            (3i64, TypedTag::new("cate", "c")),
        ];
        let q = user_tags_write(&path.to_string_lossy(), inserts, vec![], &[]);
        save_parquet(&conn, &q, &path, None).unwrap();

        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}')",
                    path.to_string_lossy()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    // 挿入行1件ごとに UNION ALL の分岐を1本積む実装だと、分岐数に対して
    // DuckDB のプランナ/オプティマイザのコストが破滅的に増大し、大量件数の
    // 一括タグ付けが現実的な時間・メモリで終わらなくなる（実機で25GiBの
    // メモリ上限到達を確認済み）。生成される SQL 中の UNION ALL 出現回数が
    // 挿入件数に比例して増えないことを、実行時間ではなく SQL の形そのもので保証する。
    #[test]
    fn union_all_branch_count_does_not_scale_with_insert_count() {
        fn union_all_count(n: usize) -> usize {
            let inserts: Vec<(i64, TypedTag)> = (0..n as i64)
                .map(|i| (i, TypedTag::new("cate", format!("v{i}"))))
                .collect();
            let q = user_tags_write("dummy_path.parquet", inserts, vec![], &[]);
            let sql = q.to_string(sea_query::PostgresQueryBuilder);
            sql.matches("UNION ALL").count()
        }

        let small = union_all_count(3);
        let large = union_all_count(2000);
        assert_eq!(
            small, large,
            "UNION ALL branch count must not grow with the number of inserted rows"
        );
    }
}
