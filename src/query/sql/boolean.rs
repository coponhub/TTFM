use crate::db::{Col, CustomFunc, SqlType};
use crate::query::ast::ComparisonOp;
use crate::query::lens_resolver::{LabelSetOpKind, ResolvedNode};
use crate::query::lens_schema::to_bin_op;
use sea_query::{Alias, Expr, ExprTrait, Query, SelectStatement, SimpleExpr};
use super::{
    wrap_in_subquery, build_aggregation_context,
    label_to_unit_aware_expr, subquery, BuildPick, PickNode,
};
use super::agg_pieces::{build_agg, build_agg_calc_subquery, build_agg_calc_expr};

fn bool_to_volatile_row(bool_expr: SimpleExpr) -> SimpleExpr {
    let name_str = Expr::case(
        Expr::expr(bool_expr.clone()).is_null(),
        Expr::val("NULL"),
    )
    .case(bool_expr.clone(), Expr::val("TRUE"))
    .finally(Expr::val("FALSE"));
    let name_sp = CustomFunc::struct_pack_tag(
        Expr::val("name").into(),
        CustomFunc::union_value(SqlType::VARCHAR, name_str),
        Expr::val("system").into(),
    );
    let type_sp = CustomFunc::struct_pack_tag(
        Expr::val("type").into(),
        CustomFunc::union_value(SqlType::VARCHAR, Expr::val("boolean")),
        Expr::val("system").into(),
    );
    let value_sp = CustomFunc::struct_pack_tag(
        Expr::val("value").into(),
        CustomFunc::union_value(SqlType::BOOLEAN, bool_expr),
        Expr::val("system").into(),
    );
    CustomFunc::list_value([name_sp, type_sp, value_sp])
}

fn build_boolean_comparison_sql(
    left: SimpleExpr,
    op: ComparisonOp,
    right: SimpleExpr,
) -> SelectStatement {
    let bin_op = to_bin_op(op);
    let bool_expr = Expr::expr(left).binary(bin_op, right).into();
    let mut q = Query::select();
    q.expr_as(Expr::val(0i64), Col::ItemId)
        .expr_as(Expr::val(0i64), Col::Rank)
        .expr_as(Expr::val("volatile"), Col::ItemKind)
        .expr_as(bool_to_volatile_row(bool_expr), crate::db::QueryResultCol::Tags);
    q
}

pub(super) fn build_boolean_existence_sql(pick_sql: SelectStatement) -> SelectStatement {
    let bool_expr =
        CustomFunc::any_value(Expr::col((Alias::new("pk"), Col::ItemId))).is_not_null();
    let mut q = Query::select();
    q.expr_as(Expr::val(0i64), Col::ItemId)
        .expr_as(Expr::val(0i64), Col::Rank)
        .expr_as(Expr::val("volatile"), Col::ItemKind)
        .expr_as(bool_to_volatile_row(bool_expr), crate::db::QueryResultCol::Tags)
        .from_subquery(pick_sql, Alias::new("pk"));
    q
}

pub(super) fn reduce_with_union(
    child_sqls: Vec<SelectStatement>,
    union_type: sea_query::UnionType,
    empty_fallback: SelectStatement,
) -> SelectStatement {
    child_sqls
        .into_iter()
        .map(wrap_in_subquery)
        .reduce(|mut acc, next| {
            acc.union(union_type, next);
            acc
        })
        .unwrap_or(empty_fallback)
}

pub(super) fn build_resolved_and_sql(
    child_sqls: Vec<SelectStatement>,
    view: &str,
) -> SelectStatement {
    let fallback = Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view))
        .to_owned();
    reduce_with_union(child_sqls, sea_query::UnionType::Intersect, fallback)
}

pub(super) fn build_resolved_or_sql(
    child_sqls: Vec<SelectStatement>,
    view: &str,
) -> SelectStatement {
    let fallback = Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from(Alias::new(view))
        .and_where(Expr::val(1).eq(0))
        .to_owned();
    reduce_with_union(child_sqls, sea_query::UnionType::Distinct, fallback)
}

pub(super) fn build_resolved_diff_sql(
    l: SelectStatement,
    r: SelectStatement,
) -> SelectStatement {
    let mut q = wrap_in_subquery(l);
    q.union(sea_query::UnionType::Except, wrap_in_subquery(r));
    q
}

pub(super) fn build_label_set_op_pick_sql(
    op: &LabelSetOpKind,
    child_sqls: Vec<SelectStatement>,
) -> SelectStatement {
    match op {
        LabelSetOpKind::Union => reduce_with_union(
            child_sqls,
            sea_query::UnionType::Distinct,
            Query::select().to_owned(),
        ),
        LabelSetOpKind::Intersect => reduce_with_union(
            child_sqls,
            sea_query::UnionType::Intersect,
            Query::select().to_owned(),
        ),
        LabelSetOpKind::Except => {
            let mut it = child_sqls.into_iter();
            if let (Some(l), Some(r)) = (it.next(), it.next()) {
                build_resolved_diff_sql(l, r)
            } else {
                Query::select().to_owned()
            }
        }
    }
}

pub(super) fn build_boolean_sql(node: &ResolvedNode, view: &str) -> SelectStatement {
    let agg_ctx = build_aggregation_context(node, view);
    match node {
        ResolvedNode::AggregationMatch { agg, op, label } => {
            build_boolean_comparison_sql(
                subquery(build_agg(agg, view, &agg_ctx)),
                *op,
                label_to_unit_aware_expr(label),
            )
        }
        ResolvedNode::AggregationAggregationMatch { left, op, right } => {
            build_boolean_comparison_sql(
                subquery(build_agg(left, view, &agg_ctx)),
                *op,
                subquery(build_agg(right, view, &agg_ctx)),
            )
        }
        ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
            let calc_expr = if calc.contains_aggregation() {
                build_agg_calc_subquery(calc, view, &agg_ctx)
            } else {
                build_agg_calc_expr(calc, &agg_ctx)
            };
            build_boolean_comparison_sql(
                subquery(build_agg(agg, view, &agg_ctx)),
                *op,
                calc_expr,
            )
        }
        ResolvedNode::AggregationTagMatch { .. } => {
            let pick_sql = PickNode::new(node, view).build_pick();
            build_boolean_existence_sql(pick_sql)
        }
        ResolvedNode::ScalarMatch { left, op, right } => {
            build_boolean_comparison_sql(
                label_to_unit_aware_expr(left),
                *op,
                label_to_unit_aware_expr(right),
            )
        }
        _ => {
            let pick_sql = PickNode::new(node, view).build_pick();
            build_boolean_existence_sql(pick_sql)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{BasicOp, ComparisonOp};
    use sea_query::PostgresQueryBuilder;

    #[test]
    fn test_build_boolean_comparison_sql_volatile_row() {
        let sql = build_boolean_comparison_sql(
            Expr::val(1i64).into(),
            ComparisonOp::Scalar(BasicOp::Gt),
            Expr::val(0i64).into(),
        );
        let s = sql.to_string(PostgresQueryBuilder);

        assert!(s.contains("item_id"), "should have item_id: {}", s);
        assert!(s.contains("item_kind"), "should have item_kind: {}", s);
        assert!(s.contains("tags"), "should have tags: {}", s);
        assert!(s.contains("tag_type"), "should have tag_type: {}", s);
        assert!(
            s.contains("union_value(s :="),
            "should have string union arm: {}",
            s
        );
        assert!(s.contains("'TRUE'"), "should have TRUE: {}", s);
        assert!(s.contains("'FALSE'"), "should have FALSE: {}", s);
        assert!(
            !s.contains("scalar_value"),
            "should not have scalar_value: {}",
            s
        );
    }

    #[test]
    fn test_build_boolean_existence_sql_volatile_row() {
        let pick_sql = Query::select()
            .column(Col::ItemId)
            .from(Alias::new("oneview"))
            .to_owned();
        let sql = build_boolean_existence_sql(pick_sql);
        let s = sql.to_string(PostgresQueryBuilder);

        assert!(s.contains("item_id"), "should have item_id: {}", s);
        assert!(s.contains("item_kind"), "should have item_kind: {}", s);
        assert!(s.contains("tags"), "should have tags: {}", s);
        assert!(
            s.contains("union_value(s :="),
            "should have string union arm: {}",
            s
        );
        assert!(
            !s.contains("scalar_value"),
            "should not have scalar_value: {}",
            s
        );
    }
}
