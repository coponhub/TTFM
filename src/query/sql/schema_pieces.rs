use crate::db::{Col, Pronoun::*, Tbl};
use sea_query::{Expr, Query, SelectStatement};

/// Column ストレージ用ラベル SELECT。`WHERE type = ?` フィルタなし。
pub(crate) fn build_lens_select_column(
    col: Col,
    ids_sql: SelectStatement,
) -> SelectStatement {
    let cast_expr =
        Expr::cust_with_exprs("CAST($1 AS VARCHAR)", vec![Expr::col(col).into()]);
    let mut s = Query::select();
    s.expr_as(cast_expr, Cast)
        .column(Col::ItemId)
        .from(Tbl::OneView)
        .and_where(Expr::col(col).is_not_null())
        .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
    s
}

/// RowTag ストレージ用ラベル SELECT。`WHERE type = tag_type` フィルタあり。
pub(crate) fn build_lens_select_row_tag(
    col: Col,
    tag_type: &str,
    ids_sql: SelectStatement,
) -> SelectStatement {
    let cast_expr =
        Expr::cust_with_exprs("CAST($1 AS VARCHAR)", vec![Expr::col(col).into()]);
    let mut s = Query::select();
    s.expr_as(cast_expr, Cast)
        .column(Col::ItemId)
        .from(Tbl::OneView)
        .and_where(Expr::col(Col::Type).eq(tag_type))
        .and_where(Expr::col(col).is_not_null())
        .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::Query;

    fn dummy_ids() -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId).from(Tbl::OneView);
        q
    }

    #[test]
    fn test_build_lens_select_column_no_type_filter() {
        use sea_query::SqliteQueryBuilder;
        let sql =
            build_lens_select_column(Col::Type, dummy_ids()).to_string(SqliteQueryBuilder);
        assert!(!sql.contains("\"type\" = "), "Column select must not have type filter");
        assert!(sql.contains("CAST("), "must CAST to VARCHAR");
    }

    #[test]
    fn test_build_lens_select_row_tag_has_type_filter() {
        use sea_query::SqliteQueryBuilder;
        let sql =
            build_lens_select_row_tag(Col::LabelStr, "parentdir", dummy_ids())
                .to_string(SqliteQueryBuilder);
        assert!(sql.contains("parentdir"), "RowTag select must filter by tag_type");
        assert!(sql.contains("CAST("), "must CAST to VARCHAR");
    }
}
