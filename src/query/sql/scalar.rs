use crate::db::{Col, SqlType};
use crate::query::ast::ComparisonOp;
use crate::query::lens_resolver::ResolvedOperand;
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{Label, SType};
use sea_query::{Alias, BinOper, Expr, Query, SelectStatement};
use super::{
    build_aggregation_context_for_operand,
    needs_nest_context, build_nest_context, build_nest_context_for_operand,
    build_tag_value_agg_expr, label_to_unit_aware_expr,
};
use super::agg_pieces::{
    build_agg, build_agg_nest,
    build_agg_operand_subquery, build_agg_operand_subquery_nest,
};

pub(super) fn build_resolved_match_sql(
    storage: &StorageMapping,
    sql_type: SqlType,
    op: ComparisonOp,
    label: &Label,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));
    q.cond_where(storage.to_condition(op, label, sql_type));
    q
}

pub(super) fn build_column_match_sql(
    tag: SType,
    label: &Label,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));
    match label.value() {
        crate::types::LabelValue::Integer(i) => {
            let t = if matches!(tag, SType::Label) { Col::LabelInt.into() } else { tag };
            q.and_where(Expr::col(t).eq(i));
        }
        crate::types::LabelValue::String(s) => {
            let t = if matches!(tag, SType::Label) { Col::LabelStr.into() } else { tag };
            let val_str = if s.starts_with('^') {
                format!("{}*", &s[1..])
            } else {
                s.clone()
            };
            q.and_where(Expr::col(t).binary(BinOper::Custom("GLOB"), Expr::val(val_str)));
        }
        crate::types::LabelValue::Literal(s) => {
            let t = if matches!(tag, SType::Label) { Col::LabelStr.into() } else { tag };
            q.and_where(Expr::col(t).eq(s.as_str()));
        }
        crate::types::LabelValue::Boolean(b) => {
            q.and_where(Expr::col(Col::LabelBool).eq(b));
        }
        crate::types::LabelValue::Double(bits) => {
            q.and_where(Expr::col(Col::LabelDouble).eq(f64::from_bits(bits)));
        }
        crate::types::LabelValue::Null => {
            q.and_where(Expr::col(Col::LabelStr).is_null());
        }
    }
    q
}

pub(super) fn build_resolved_tag_tag_match_sql(
    left_storage: &StorageMapping,
    left_sql_type: SqlType,
    op: ComparisonOp,
    right_storage: &StorageMapping,
    right_sql_type: SqlType,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.column(Col::ItemId)
        .from(Alias::new(view))
        .group_by_col(Col::ItemId);
    let left_expr = build_tag_value_agg_expr(left_storage, left_sql_type);
    let right_expr = build_tag_value_agg_expr(right_storage, right_sql_type);
    q.and_having(left_expr.binary(to_bin_op(op), right_expr));
    q
}

pub(super) fn build_scalar_match_sql(
    left: &Label,
    op: ComparisonOp,
    right: &Label,
    view: &str,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));
    stmt.column(Col::ItemId);
    let cond = Expr::expr(label_to_unit_aware_expr(left))
        .binary(to_bin_op(op), label_to_unit_aware_expr(right));
    stmt.cond_where(cond);
    stmt.limit(1);
    stmt
}

pub(super) fn build_resolved_scalar_sql(
    op: &ResolvedOperand,
    view: &str,
) -> SelectStatement {
    let agg_ctx = build_aggregation_context_for_operand(op, view);
    match op {
        ResolvedOperand::Aggregation(agg) => {
            if needs_nest_context(agg.inner_node()) {
                let nest_ctx = build_nest_context(agg.inner_node(), view);
                build_agg_nest(agg, view, &agg_ctx, &nest_ctx)
            } else {
                build_agg(agg, view, &agg_ctx)
            }
        }
        _ => {
            let needs_nest = op.walk().into_iter().any(|o| {
                if let ResolvedOperand::Aggregation(agg) = o {
                    needs_nest_context(agg.inner_node())
                } else {
                    false
                }
            });
            let scalar_expr = if needs_nest {
                let nest_ctx = build_nest_context_for_operand(op, view);
                build_agg_operand_subquery_nest(op, view, &agg_ctx, &nest_ctx)
            } else {
                build_agg_operand_subquery(op, view, &agg_ctx)
            };
            let mut stmt = Query::select();
            stmt.from(Alias::new(view));
            stmt.expr_as(scalar_expr, Alias::new("scalar_value"));
            stmt.limit(1);
            stmt
        }
    }
}
