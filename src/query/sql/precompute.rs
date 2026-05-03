use super::{
    resolve_count_target, try_dispatch_common, AggregationContext, NestContext,
};
use crate::query::lens_resolver::{
    ResolvedAggregationNode, ResolvedNode, ResolvedOperand,
};
use sea_query::SelectStatement;

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
            Err(_) => unreachable!(
                "build_filter_sql: filter/context nodes must not contain aggregation or nest nodes"
            ),
        }
    })
}
