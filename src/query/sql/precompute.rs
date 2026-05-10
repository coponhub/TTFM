use super::{
    apply_arithmetic_op, build_agg, build_resolved_literal_expr,
    build_tag_value_agg_expr, label_to_unit_aware_expr, resolve_count_target,
    subquery, try_dispatch_common, AggregationContext, NestContext,
};
use crate::db::{Col, Tbl};
use crate::query::lens_resolver::{
    ResolvedAggregationNode, ResolvedCalculationNode, ResolvedNode,
    ResolvedOperand,
};
use crate::query::lens_schema::to_bin_op;
use sea_query::{Expr, Query, SelectStatement, SimpleExpr};

pub fn needs_aggregation_context(node: &ResolvedNode) -> bool {
    node.walk().into_iter().any(|n| {
        matches!(
            n,
            ResolvedNode::Aggregation(_)
                | ResolvedNode::AggregationMatch { .. }
                | ResolvedNode::AggregationTagMatch { .. }
                | ResolvedNode::AggregationCalculationMatch { .. }
                | ResolvedNode::AggregationAggregationMatch { .. }
                | ResolvedNode::CalculationMatch { .. }
                | ResolvedNode::TagCalculationMatch { .. }
                | ResolvedNode::CalculationCalculationMatch { .. }
                | ResolvedNode::NestMatch { .. }
                | ResolvedNode::NestNestMatch { .. }
                | ResolvedNode::MergedNestMatch { .. }
                | ResolvedNode::Nest {
                    nvalue: Some(_),
                    ..
                }
        )
    })
}

pub fn build_aggregation_context(node: &ResolvedNode) -> AggregationContext {
    let mut ctx = AggregationContext::new();
    build_agg_context_into(node, &mut ctx);
    materialize_agg_context(&mut ctx);
    ctx
}

fn build_agg_context_into(node: &ResolvedNode, ctx: &mut AggregationContext) {
    for n in node.walk() {
        match n {
            ResolvedNode::Aggregation(agg)
            | ResolvedNode::AggregationMatch { agg, .. }
            | ResolvedNode::AggregationTagMatch { agg, .. } => {
                precompute_agg_into(agg, ctx);
            }
            ResolvedNode::AggregationCalculationMatch { agg, calc, .. } => {
                precompute_agg_into(agg, ctx);
                precompute_operand_aggs_into(&calc.left, ctx);
                precompute_operand_aggs_into(&calc.right, ctx);
            }
            ResolvedNode::AggregationAggregationMatch {
                left, right, ..
            } => {
                precompute_agg_into(left, ctx);
                precompute_agg_into(right, ctx);
            }
            ResolvedNode::NestMatch { nvalue, .. } => {
                precompute_operand_aggs_into(nvalue, ctx);
            }
            ResolvedNode::NestNestMatch {
                left_nvalue,
                right_nvalue,
                ..
            } => {
                precompute_operand_aggs_into(left_nvalue, ctx);
                precompute_operand_aggs_into(right_nvalue, ctx);
            }
            ResolvedNode::MergedNestMatch { matches, .. } => {
                for m in matches {
                    precompute_operand_aggs_into(&m.nvalue, ctx);
                }
            }
            ResolvedNode::Nest {
                nvalue: Some(nv), ..
            } => {
                precompute_operand_aggs_into(nv, ctx);
            }
            ResolvedNode::CalculationMatch { calc, .. }
            | ResolvedNode::TagCalculationMatch { calc, .. } => {
                precompute_operand_aggs_into(&calc.left, ctx);
                precompute_operand_aggs_into(&calc.right, ctx);
            }
            ResolvedNode::CalculationCalculationMatch {
                left_calc,
                right_calc,
                ..
            } => {
                precompute_operand_aggs_into(&left_calc.left, ctx);
                precompute_operand_aggs_into(&left_calc.right, ctx);
                precompute_operand_aggs_into(&right_calc.left, ctx);
                precompute_operand_aggs_into(&right_calc.right, ctx);
            }
            _ => {}
        }
    }
}

pub fn build_aggregation_context_for_operand(
    op: &ResolvedOperand,
) -> AggregationContext {
    let mut ctx = AggregationContext::new();
    precompute_operand_aggs_into(op, &mut ctx);
    materialize_agg_context(&mut ctx);
    ctx
}

pub fn build_aggregation_context_for_agg(
    agg: &ResolvedAggregationNode,
) -> AggregationContext {
    let mut ctx = AggregationContext::new();
    precompute_agg_into(agg, &mut ctx);
    materialize_agg_context(&mut ctx);
    ctx
}

fn precompute_operand_aggs_into(
    operand: &ResolvedOperand,
    ctx: &mut AggregationContext,
) {
    for op in operand.walk() {
        if let ResolvedOperand::Aggregation(agg) = op {
            precompute_agg_into(agg, ctx);
        }
    }
}

fn precompute_agg_into(
    agg: &ResolvedAggregationNode,
    ctx: &mut AggregationContext,
) {
    let inner = agg.inner_node();
    let key = inner as *const ResolvedNode as usize;

    if !ctx.agg_filters.contains_key(&key)
        && !ctx.filter_nodes.contains_key(&key)
    {
        let (_, filter_opt, _) = inner.extract_agg_parts();
        if let Some(filter) = filter_opt {
            ctx.filter_nodes.insert(key, filter);
        }
    }

    if matches!(agg, ResolvedAggregationNode::Count(_))
        && !ctx.agg_inner_sqls.contains_key(&key)
        && !ctx.inner_nodes.contains_key(&key)
    {
        let (_, inner_tag_type) = resolve_count_target(inner);
        if inner_tag_type.is_none() {
            ctx.inner_nodes.insert(key, inner.clone());
        }
    }
}

pub fn needs_nest_context(node: &ResolvedNode) -> bool {
    node.walk().into_iter().any(|n| {
        matches!(
            n,
            ResolvedNode::NestMatch { .. }
                | ResolvedNode::NestNestMatch { .. }
                | ResolvedNode::MergedNestMatch { .. }
                | ResolvedNode::Nest {
                    context: Some(_),
                    ..
                }
        )
    })
}

pub fn build_nest_context(node: &ResolvedNode) -> NestContext {
    let mut ctx = NestContext::new();
    build_nest_context_into(node, &mut ctx);
    materialize_nest_context(&mut ctx);
    ctx
}

pub fn build_nest_context_for_operand(op: &ResolvedOperand) -> NestContext {
    let mut ctx = NestContext::new();
    for o in op.walk() {
        if let ResolvedOperand::Aggregation(agg) = o {
            build_nest_context_into(agg.inner_node(), &mut ctx);
        }
    }
    materialize_nest_context(&mut ctx);
    ctx
}

fn build_nest_context_into(node: &ResolvedNode, ctx: &mut NestContext) {
    for n in node.walk() {
        match n {
            ResolvedNode::NestMatch { context, .. } => {
                if let Some(c) = context {
                    precompute_ctx_into(c, ctx);
                }
            }
            ResolvedNode::NestNestMatch {
                left_context,
                right_context,
                ..
            } => {
                if let Some(c) = left_context {
                    precompute_ctx_into(c, ctx);
                }
                if let Some(c) = right_context {
                    precompute_ctx_into(c, ctx);
                }
            }
            ResolvedNode::MergedNestMatch { matches, .. } => {
                for m in matches {
                    if let Some(c) = &m.context {
                        precompute_ctx_into(c, ctx);
                    }
                }
            }
            ResolvedNode::Nest {
                context: Some(c), ..
            } => {
                precompute_ctx_into(c, ctx);
            }
            _ => {}
        }
    }
}

fn precompute_ctx_into(ctx_node: &ResolvedNode, ctx: &mut NestContext) {
    let key = ctx_node as *const ResolvedNode as usize;
    if !ctx.contexts.contains_key(&key) && !ctx.context_nodes.contains_key(&key)
    {
        ctx.context_nodes.insert(key, ctx_node.clone());
    }
}

fn materialize_nest_context(ctx: &mut NestContext) {
    let nodes: Vec<(usize, ResolvedNode)> = ctx.context_nodes.drain().collect();
    for (key, node) in nodes {
        ctx.contexts.insert(key, build_filter_sql(&node));
    }
}

fn materialize_agg_context(ctx: &mut AggregationContext) {
    let filter_nodes: Vec<(usize, ResolvedNode)> =
        ctx.filter_nodes.drain().collect();
    for (key, node) in filter_nodes {
        ctx.agg_filters.insert(key, build_filter_sql(&node));
    }
    let inner_nodes: Vec<(usize, ResolvedNode)> =
        ctx.inner_nodes.drain().collect();
    for (key, node) in inner_nodes {
        ctx.agg_inner_sqls.insert(key, build_filter_sql(&node));
    }
}

fn build_filter_sql(node: &ResolvedNode) -> SelectStatement {
    node.fold(&|node, child_sqls: Vec<SelectStatement>| {
        match try_dispatch_common(node, child_sqls) {
            Ok(sql) => sql,
            Err(_) => build_filter_node_sql(node),
        }
    })
}

fn build_filter_node_sql(node: &ResolvedNode) -> SelectStatement {
    // 集約を含む calc のために agg_ctx を事前計算する。
    // build_filter_operand_expr はタグを EAV で、集約をスカラーサブクエリで展開する。
    let agg_ctx = build_aggregation_context(node);
    match node {
        ResolvedNode::CalculationMatch { calc, op, label } => {
            let mut stmt = Query::select();
            stmt.column(Col::ItemId)
                .from(Tbl::OneView)
                .group_by_col(Col::ItemId);
            let calc_expr = build_filter_calc_expr(calc, &agg_ctx);
            let label_expr = label_to_unit_aware_expr(label);
            stmt.and_having(
                Expr::expr(calc_expr).binary(to_bin_op(*op), label_expr),
            );
            stmt
        }
        ResolvedNode::TagCalculationMatch {
            storage,
            sql_type,
            op,
            calc,
            ..
        } => {
            let mut stmt = Query::select();
            stmt.column(Col::ItemId)
                .from(Tbl::OneView)
                .group_by_col(Col::ItemId);
            let tag_expr = build_tag_value_agg_expr(storage, *sql_type);
            let calc_expr = build_filter_calc_expr(calc, &agg_ctx);
            stmt.and_having(
                Expr::expr(tag_expr).binary(to_bin_op(*op), calc_expr),
            );
            stmt
        }
        ResolvedNode::CalculationCalculationMatch {
            left_calc,
            op,
            right_calc,
        } => {
            let mut stmt = Query::select();
            stmt.column(Col::ItemId)
                .from(Tbl::OneView)
                .group_by_col(Col::ItemId);
            let left_expr = build_filter_calc_expr(left_calc, &agg_ctx);
            let right_expr = build_filter_calc_expr(right_calc, &agg_ctx);
            stmt.and_having(
                Expr::expr(left_expr).binary(to_bin_op(*op), right_expr),
            );
            stmt
        }
        _ => unreachable!(
            "build_filter_sql: filter/context nodes must not contain aggregation or nest nodes"
        ),
    }
}

/// フィルター文脈用の算術演算式を構築する。
/// タグ参照は EAV 集約（item_id GROUP BY 配下で機能）、
/// 集約ノードはスカラーサブクエリとして展開する（再帰的）。
fn build_filter_calc_expr(
    calc: &ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left = build_filter_operand_expr(&calc.left, agg_ctx);
    let right = build_filter_operand_expr(&calc.right, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

fn build_filter_operand_expr(
    operand: &ResolvedOperand,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef { storage, sql_type, .. } => {
            build_tag_value_agg_expr(storage, *sql_type)
        }
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] =
                child_results.try_into().unwrap();
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(agg) => subquery(build_agg(agg, agg_ctx)),
    })
}
