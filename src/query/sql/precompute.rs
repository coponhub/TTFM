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

use super::{
    apply_arithmetic_op, build_agg, build_resolved_literal_expr,
    build_tag_value_agg_expr, nvalue_rhs_condition, resolve_count_target,
    subquery, try_dispatch_common, AggregationContext, NestContext,
};
use crate::db::{Col, Src};
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

pub fn build_aggregation_context(
    src: &Src,
    node: &ResolvedNode,
) -> AggregationContext {
    let mut ctx = AggregationContext::new();
    build_agg_context_into(node, &mut ctx);
    materialize_agg_context(src, &mut ctx);
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
    src: &Src,
    op: &ResolvedOperand,
) -> AggregationContext {
    let mut ctx = AggregationContext::new();
    precompute_operand_aggs_into(op, &mut ctx);
    materialize_agg_context(src, &mut ctx);
    ctx
}

pub fn build_aggregation_context_for_agg(
    src: &Src,
    agg: &ResolvedAggregationNode,
) -> AggregationContext {
    let mut ctx = AggregationContext::new();
    precompute_agg_into(agg, &mut ctx);
    materialize_agg_context(src, &mut ctx);
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

    // rest は split のたびに新しく合成されるノードでポインタが安定しないため、
    // rest 自身ではなく inner のポインタ（key）に登録する。
    if matches!(agg, ResolvedAggregationNode::Count(_)) {
        if let Some((defs, rest)) = inner.split_definition_branches() {
            if !ctx.definition_counts.contains_key(&key)
                && !ctx.definition_count_nodes.contains_key(&key)
            {
                ctx.definition_count_nodes
                    .insert(key, defs.into_iter().cloned().collect());
            }
            if let Some(rest_node) = rest {
                if !ctx.agg_filters.contains_key(&key)
                    && !ctx.filter_nodes.contains_key(&key)
                {
                    ctx.filter_nodes.insert(key, rest_node);
                }
            }
            return;
        }
    }

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

pub fn build_nest_context(src: &Src, node: &ResolvedNode) -> NestContext {
    let mut ctx = NestContext::new();
    build_nest_context_into(node, &mut ctx);
    materialize_nest_context(src, &mut ctx);
    ctx
}

pub fn build_nest_context_for_operand(
    src: &Src,
    op: &ResolvedOperand,
) -> NestContext {
    let mut ctx = NestContext::new();
    for o in op.walk() {
        if let ResolvedOperand::Aggregation(agg) = o {
            build_nest_context_into(agg.inner_node(), &mut ctx);
        }
    }
    materialize_nest_context(src, &mut ctx);
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

fn materialize_nest_context(src: &Src, ctx: &mut NestContext) {
    let nodes: Vec<(usize, ResolvedNode)> = ctx.context_nodes.drain().collect();
    for (key, node) in nodes {
        ctx.contexts.insert(key, build_filter_sql(src, &node));
    }
}

fn materialize_agg_context(src: &Src, ctx: &mut AggregationContext) {
    let filter_nodes: Vec<(usize, ResolvedNode)> =
        ctx.filter_nodes.drain().collect();
    for (key, node) in filter_nodes {
        ctx.agg_filters.insert(key, build_filter_sql(src, &node));
    }
    let inner_nodes: Vec<(usize, ResolvedNode)> =
        ctx.inner_nodes.drain().collect();
    for (key, node) in inner_nodes {
        ctx.agg_inner_sqls.insert(key, build_filter_sql(src, &node));
    }
    let definition_count_nodes: Vec<(usize, Vec<ResolvedNode>)> =
        ctx.definition_count_nodes.drain().collect();
    for (key, defs) in definition_count_nodes {
        let sql = crate::query::lens_builder::count_definitions(src, &defs);
        ctx.definition_counts.insert(key, sql);
    }
}

fn build_filter_sql(src: &Src, node: &ResolvedNode) -> SelectStatement {
    node.fold(&|node, child_sqls: Vec<SelectStatement>| {
        match try_dispatch_common(src, node, child_sqls) {
            Ok(sql) => sql,
            Err(_) => build_filter_node_sql(src, node),
        }
    })
}

fn build_filter_node_sql(src: &Src, node: &ResolvedNode) -> SelectStatement {
    // 集約を含む calc のために agg_ctx を事前計算する。
    // build_filter_operand_expr はタグを EAV で、集約をスカラーサブクエリで展開する。
    let agg_ctx = build_aggregation_context(src, node);
    match node {
        ResolvedNode::CalculationMatch { calc, op, rhs } => {
            let mut stmt = Query::select();
            stmt.column(Col::ItemId)
                .from(src)
                .group_by_col(Col::ItemId);
            let calc_expr = build_filter_calc_expr(src, calc, &agg_ctx);
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            stmt.cond_having(nvalue_rhs_condition(
                calc_expr, *op, rhs, is_string,
            ));
            stmt
        }
        ResolvedNode::TagCalculationMatch {
            storage,
            bitical_type,
            op,
            calc,
            ..
        } => {
            let mut stmt = Query::select();
            stmt.column(Col::ItemId)
                .from(src)
                .group_by_col(Col::ItemId);
            let tag_expr = build_tag_value_agg_expr(storage, *bitical_type);
            let calc_expr = build_filter_calc_expr(src, calc, &agg_ctx);
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
                .from(src)
                .group_by_col(Col::ItemId);
            let left_expr = build_filter_calc_expr(src, left_calc, &agg_ctx);
            let right_expr = build_filter_calc_expr(src, right_calc, &agg_ctx);
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
    src: &Src,
    calc: &ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left = build_filter_operand_expr(src, &calc.left, agg_ctx);
    let right = build_filter_operand_expr(src, &calc.right, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

fn build_filter_operand_expr(
    src: &Src,
    operand: &ResolvedOperand,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef {
            storage,
            bitical_type,
            ..
        } => build_tag_value_agg_expr(storage, *bitical_type),
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] =
                child_results.try_into().unwrap();
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(agg) => {
            subquery(build_agg(src, agg, agg_ctx))
        }
    })
}
