use crate::db::{Col, Pronoun::*, Tbl};
use sea_query::{BinOper, Condition, Expr, Query, SelectStatement, SimpleExpr};

// ── to_label_select 用 ────────────────────────────────────────────────────

/// Column ストレージ用ラベル SELECT。`WHERE type = ?` フィルタなし。
pub(crate) fn build_lens_select_column(
    col: Col,
    ids_sql: SelectStatement,
) -> SelectStatement {
    let cast_expr = Expr::cust_with_exprs(
        "CAST($1 AS VARCHAR)",
        vec![Expr::col(col).into()],
    );
    let mut s = Query::select();
    s.expr_as(cast_expr, Cast)
        .column(Col::ItemId)
        .from(Tbl::OneView)
        .and_where(Expr::col(col).is_not_null())
        .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
    s
}

/// タグ用ラベル SELECT。`WHERE type = tag_type` フィルタあり。
pub(crate) fn build_lens_select_tag(
    col: Col,
    tag_type: &str,
    ids_sql: SelectStatement,
) -> SelectStatement {
    let cast_expr = Expr::cust_with_exprs(
        "CAST($1 AS VARCHAR)",
        vec![Expr::col(col).into()],
    );
    let mut s = Query::select();
    s.expr_as(cast_expr, Cast)
        .column(Col::ItemId)
        .from(Tbl::OneView)
        .and_where(Expr::col(Col::Type).eq(tag_type))
        .and_where(Expr::col(col).is_not_null())
        .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
    s
}

// ── to_condition 用 ───────────────────────────────────────────────────────

/// カラムに対する整数値の比較式を生成。
pub(crate) fn col_cmp_i64(col: Col, op: BinOper, val: i64) -> SimpleExpr {
    Expr::col(col).binary(op, Expr::val(val))
}

/// カラムに対する浮動小数点値の比較式を生成。
pub(crate) fn col_cmp_f64(col: Col, op: BinOper, val: f64) -> SimpleExpr {
    Expr::col(col).binary(op, Expr::val(val))
}

/// カラムに対する文字列値の比較式を生成。
pub(crate) fn col_cmp_str(col: Col, op: BinOper, val: &str) -> SimpleExpr {
    Expr::col(col).binary(op, val)
}

/// カラムに対する真偽値の比較式を生成。
pub(crate) fn col_cmp_bool(col: Col, op: BinOper, val: bool) -> SimpleExpr {
    Expr::col(col).binary(op, Expr::val(val))
}

/// type カラムに対するフィルタ式を生成。
pub(crate) fn type_filter(op: BinOper, tag_type: &str) -> SimpleExpr {
    Expr::col(Col::Type).binary(op, tag_type)
}

/// カラムの NULL 判定式を生成。
pub(crate) fn col_is_null(col: Col) -> SimpleExpr {
    Expr::col(col).is_null()
}

/// 浮動小数点値条件の `Condition` を生成。
pub(crate) fn build_double_condition(op: BinOper, bits: u64) -> Condition {
    Condition::any().add(col_cmp_f64(
        Col::LabelDouble,
        op,
        f64::from_bits(bits),
    ))
}

/// NULL 条件の `Condition` を生成。
pub(crate) fn build_null_condition() -> Condition {
    Condition::any().add(col_is_null(Col::LabelStr))
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
        let sql = build_lens_select_column(Col::Type, dummy_ids())
            .to_string(SqliteQueryBuilder);
        assert!(
            !sql.contains("\"type\" = "),
            "Column select must not have type filter"
        );
        assert!(sql.contains("CAST("), "must CAST to VARCHAR");
    }

    #[test]
    fn test_build_lens_select_tag_has_type_filter() {
        use sea_query::SqliteQueryBuilder;
        let sql =
            build_lens_select_tag(Col::LabelStr, "parentdir", dummy_ids())
                .to_string(SqliteQueryBuilder);
        assert!(
            sql.contains("parentdir"),
            "tag select must filter by tag_type"
        );
        assert!(sql.contains("CAST("), "must CAST to VARCHAR");
    }
}
