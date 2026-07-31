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

use super::agg_pieces::{
    build_agg, build_agg_calc_expr, build_agg_calc_subquery,
};
use super::{
    build_aggregation_context, label_to_simple_expr, nvalue_rhs_condition,
    subquery, wrap_in_subquery, BuildPick, PickNode,
};
use crate::db::{BiticalType, Col, CustomFunc, Pronoun::*, Src};
use crate::query::ast::ComparisonOp;
use crate::query::lens_resolver::{LabelSetOpKind, ResolvedNode};
use crate::query::lens_schema::to_bin_op;
use sea_query::{Expr, ExprTrait, Query, SelectStatement, SimpleExpr};

fn bool_to_volatile_row(bool_expr: SimpleExpr) -> SimpleExpr {
    let name_str =
        Expr::case(Expr::expr(bool_expr.clone()).is_null(), Expr::val("NULL"))
            .case(bool_expr.clone(), Expr::val("TRUE"))
            .finally(Expr::val("FALSE"));
    let name_sp = CustomFunc::struct_pack_tag(
        Expr::val("name").into(),
        CustomFunc::union_value(BiticalType::String, name_str),
        Expr::val("system").into(),
    );
    let type_sp = CustomFunc::struct_pack_tag(
        Expr::val("bitical_type").into(),
        CustomFunc::union_value(
            BiticalType::String,
            Expr::val(BiticalType::Boolean.to_string()),
        ),
        Expr::val("system").into(),
    );
    let value_sp = CustomFunc::struct_pack_tag(
        Expr::val("value").into(),
        CustomFunc::union_value(BiticalType::Boolean, bool_expr),
        Expr::val("system").into(),
    );
    CustomFunc::list_value([name_sp, type_sp, value_sp])
}

fn build_boolean_condition_sql(bool_expr: SimpleExpr) -> SelectStatement {
    let mut q = Query::select();
    // 揮発 id は SQL 側では NULL とし、fetch 後に Rust 側で採番する。
    q.expr_as(Expr::val(None::<i64>), Col::ItemId)
        .expr_as(Expr::val(0i64), Col::Rank)
        .expr_as(Expr::val("volatile"), Col::ItemKind)
        .expr_as(
            bool_to_volatile_row(bool_expr),
            crate::db::QueryResultCol::Tags,
        );
    q
}

fn build_boolean_comparison_sql(
    left: SimpleExpr,
    op: ComparisonOp,
    right: SimpleExpr,
) -> SelectStatement {
    let bin_op = to_bin_op(op);
    build_boolean_condition_sql(Expr::expr(left).binary(bin_op, right).into())
}

pub(super) fn build_boolean_existence_sql(
    pick_sql: SelectStatement,
) -> SelectStatement {
    let bool_expr =
        CustomFunc::any_value(Expr::col((Pk, Col::ItemId))).is_not_null();
    let mut q = Query::select();
    q.expr_as(Expr::val(None::<i64>), Col::ItemId)
        .expr_as(Expr::val(0i64), Col::Rank)
        .expr_as(Expr::val("volatile"), Col::ItemKind)
        .expr_as(
            bool_to_volatile_row(bool_expr),
            crate::db::QueryResultCol::Tags,
        )
        .from_subquery(pick_sql, Pk);
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
    src: &Src,
    child_sqls: Vec<SelectStatement>,
) -> SelectStatement {
    let fallback = Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(src)
        .to_owned();
    reduce_with_union(child_sqls, sea_query::UnionType::Intersect, fallback)
}

pub(super) fn build_resolved_or_sql(
    src: &Src,
    child_sqls: Vec<SelectStatement>,
) -> SelectStatement {
    let fallback = Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from(src)
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

pub(super) fn build_boolean_sql(
    src: &Src,
    node: &ResolvedNode,
) -> SelectStatement {
    let agg_ctx = build_aggregation_context(src, node);
    match node {
        ResolvedNode::AggregationMatch { agg, op, rhs } => {
            let cond = nvalue_rhs_condition(
                subquery(build_agg(src, agg, &agg_ctx)),
                *op,
                rhs,
                agg.is_string_type(),
            );
            build_boolean_condition_sql(cond.into())
        }
        ResolvedNode::AggregationAggregationMatch { left, op, right } => {
            build_boolean_comparison_sql(
                subquery(build_agg(src, left, &agg_ctx)),
                *op,
                subquery(build_agg(src, right, &agg_ctx)),
            )
        }
        ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
            let calc_expr = if calc.contains_aggregation() {
                build_agg_calc_subquery(src, calc, &agg_ctx)
            } else {
                build_agg_calc_expr(calc, &agg_ctx)
            };
            build_boolean_comparison_sql(
                subquery(build_agg(src, agg, &agg_ctx)),
                *op,
                calc_expr,
            )
        }
        ResolvedNode::AggregationTagMatch { .. } => {
            let pick_sql = PickNode::new(src, node).build_pick();
            build_boolean_existence_sql(pick_sql)
        }
        ResolvedNode::ScalarMatch { left, op, right } => {
            build_boolean_comparison_sql(
                label_to_simple_expr(left),
                *op,
                label_to_simple_expr(right),
            )
        }
        _ => {
            let pick_sql = PickNode::new(src, node).build_pick();
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
            s.contains("union_value(\"string\" :="),
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
            .from(&Src::OneView)
            .to_owned();
        let sql = build_boolean_existence_sql(pick_sql);
        let s = sql.to_string(PostgresQueryBuilder);

        assert!(s.contains("item_id"), "should have item_id: {}", s);
        assert!(s.contains("item_kind"), "should have item_kind: {}", s);
        assert!(s.contains("tags"), "should have tags: {}", s);
        assert!(
            s.contains("union_value(\"string\" :="),
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
