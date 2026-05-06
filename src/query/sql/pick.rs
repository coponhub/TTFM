use super::{
    agg_expr, apply_arithmetic_agg, apply_arithmetic_op, build_agg,
    build_agg_calc_eav_expr, build_agg_calc_expr, build_agg_calc_subquery,
    build_agg_calc_subquery_nest, build_agg_nest, build_aggregation_context,
    build_merged_nest_match_sql, build_nest_context, build_nest_match_sql,
    build_nest_nest_match_sql, build_resolved_literal_expr,
    build_storage_column_expr, build_tag_value_agg_expr,
    label_to_unit_aware_expr, needs_aggregation_context, needs_nest_context,
    subquery, try_dispatch_common, AggregationContext, NestContext,
};
use crate::db::{Col, CustomFunc, Pronoun::Sub, QueryResultCol, SqlType, Tbl};
use crate::query::ast::ComparisonOp;
use crate::query::lens_resolver::{
    ResolvedAggregationNode, ResolvedCalculationNode, ResolvedNode,
    ResolvedOperand,
};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{ItemKind, Label};
use sea_query::{Condition, Expr, Query, SelectStatement, SimpleExpr};

// ── Pick 型システム ────────────────────────────────────────────────────────

/// SQL 生成を担当する trait。`SimplePickNode` / `AggPickNode` / `NestPickNode` に実装します。
pub trait BuildPick {
    fn build_pick(&self) -> SelectStatement;
}

/// コンテキスト不要なクエリ用ノード。
pub struct SimplePickNode<'a> {
    pub node: &'a ResolvedNode,
}

/// 集約コンテキストが必要なクエリ用ノード。
pub struct AggPickNode<'a> {
    pub node: &'a ResolvedNode,
    pub agg_ctx: AggregationContext,
}

/// 集約コンテキストと Nest コンテキスト両方が必要なクエリ用ノード。
pub struct NestPickNode<'a> {
    pub node: &'a ResolvedNode,
    pub agg_ctx: AggregationContext,
    pub nest_ctx: NestContext,
}

impl BuildPick for SimplePickNode<'_> {
    fn build_pick(&self) -> SelectStatement {
        build_pick(self.node)
    }
}

impl BuildPick for AggPickNode<'_> {
    fn build_pick(&self) -> SelectStatement {
        build_pick_agg(self.node, &self.agg_ctx)
    }
}

impl BuildPick for NestPickNode<'_> {
    fn build_pick(&self) -> SelectStatement {
        build_pick_nest(self.node, &self.agg_ctx, &self.nest_ctx)
    }
}

/// ノード種別を保持する enum。型が実行時まで不明な call site 向け。
pub enum PickNode<'a> {
    Simple(SimplePickNode<'a>),
    Aggregation(AggPickNode<'a>),
    Nest(NestPickNode<'a>),
}

impl<'a> PickNode<'a> {
    /// ノードを走査してコンテキストを事前計算し、適切な型で包みます。
    pub fn new(node: &'a ResolvedNode) -> Self {
        if needs_nest_context(node) {
            let agg_ctx = build_aggregation_context(node);
            let nest_ctx = build_nest_context(node);
            PickNode::Nest(NestPickNode {
                node,
                agg_ctx,
                nest_ctx,
            })
        } else if needs_aggregation_context(node) {
            let agg_ctx = build_aggregation_context(node);
            PickNode::Aggregation(AggPickNode { node, agg_ctx })
        } else {
            PickNode::Simple(SimplePickNode { node })
        }
    }

    pub fn node(&self) -> &ResolvedNode {
        match self {
            PickNode::Simple(n) => n.node,
            PickNode::Aggregation(n) => n.node,
            PickNode::Nest(n) => n.node,
        }
    }

    pub fn agg_ctx(&self) -> Option<&AggregationContext> {
        match self {
            PickNode::Simple(_) => None,
            PickNode::Aggregation(n) => Some(&n.agg_ctx),
            PickNode::Nest(n) => Some(&n.agg_ctx),
        }
    }

    pub fn nest_ctx(&self) -> Option<&NestContext> {
        match self {
            PickNode::Simple(_) | PickNode::Aggregation(_) => None,
            PickNode::Nest(n) => Some(&n.nest_ctx),
        }
    }
}

impl BuildPick for PickNode<'_> {
    fn build_pick(&self) -> SelectStatement {
        match self {
            PickNode::Simple(n) => n.build_pick(),
            PickNode::Aggregation(n) => n.build_pick(),
            PickNode::Nest(n) => n.build_pick(),
        }
    }
}

/// CalculationNodeに含まれるRowTagのtypeフィルタをWHERE句に追加します。
fn add_type_filters(
    stmt: &mut SelectStatement,
    calc: &ResolvedCalculationNode,
) {
    for op in calc.left.walk().into_iter().chain(calc.right.walk()) {
        if let ResolvedOperand::TagRef {
            storage: StorageMapping::RowTag { tag_type, .. },
            ..
        } = op
        {
            stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
        }
    }
}

/// コンテキストなしで SQL を生成します（集約・Nest ノードを含まないクエリ向け）。
pub fn build_pick(node: &ResolvedNode) -> SelectStatement {
    node.fold(&|node, child_sqls: Vec<SelectStatement>| {
        match try_dispatch_common(node, child_sqls) {
            Ok(sql) => sql,
            Err(_) => unreachable!("build_pick called with aggregation or nest node: use build_pick_agg or build_pick_nest"),
        }
    })
}

/// 事前計算済み `AggregationContext` を使って SQL を生成します。
pub fn build_pick_agg(
    node: &ResolvedNode,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    node.fold(&|node, child_sqls: Vec<SelectStatement>| {
        match try_dispatch_common(node, child_sqls) {
            Ok(sql) => sql,
            Err(_) => match node {
                ResolvedNode::MergedNestMatch {
                    keys,
                    matches,
                    is_or,
                } => {
                    build_merged_nest_match_sql(keys, matches, *is_or, agg_ctx)
                }
                ResolvedNode::Aggregation(agg) => build_agg(agg, agg_ctx),
                ResolvedNode::AggregationMatch { agg, op, label } => {
                    build_agg_match(agg, *op, label, agg_ctx)
                }
                ResolvedNode::CalculationMatch { calc, op, label } => {
                    build_calculation_match_sql(calc, *op, label, agg_ctx)
                }
                ResolvedNode::TagCalculationMatch {
                    storage,
                    sql_type,
                    op,
                    calc,
                    ..
                } => build_tag_calculation_match_sql(
                    storage, *sql_type, *op, calc, agg_ctx,
                ),
                ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
                    build_agg_calc_match(agg, *op, calc, agg_ctx)
                }
                ResolvedNode::CalculationCalculationMatch {
                    left_calc,
                    op,
                    right_calc,
                } => build_calculation_calculation_match_sql(
                    left_calc, *op, right_calc, agg_ctx,
                ),
                ResolvedNode::AggregationAggregationMatch {
                    left,
                    op,
                    right,
                } => build_agg_agg_match(left, *op, right, agg_ctx),
                ResolvedNode::AggregationTagMatch {
                    agg,
                    op,
                    storage,
                    sql_type,
                    ..
                } => build_agg_tag_match(agg, *op, storage, *sql_type, agg_ctx),
                _ => unreachable!(
                    "build_pick_agg called with nest node: use build_pick_nest"
                ),
            },
        }
    })
}

/// 事前計算済み `AggregationContext` と `NestContext` 両方を使って SQL を生成します。
pub fn build_pick_nest(
    node: &ResolvedNode,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    node.fold(&|node, child_sqls: Vec<SelectStatement>| {
        match try_dispatch_common(node, child_sqls) {
            Ok(sql) => sql,
            Err(_) => match node {
                ResolvedNode::MergedNestMatch {
                    keys,
                    matches,
                    is_or,
                } => {
                    build_merged_nest_match_sql(keys, matches, *is_or, agg_ctx)
                }
                ResolvedNode::Aggregation(agg) => {
                    build_agg_nest(agg, agg_ctx, nest_ctx)
                }
                ResolvedNode::AggregationMatch { agg, op, label } => {
                    build_agg_match(agg, *op, label, agg_ctx)
                }
                ResolvedNode::CalculationMatch { calc, op, label } => {
                    build_calculation_match_sql(calc, *op, label, agg_ctx)
                }
                ResolvedNode::TagCalculationMatch {
                    storage,
                    sql_type,
                    op,
                    calc,
                    ..
                } => build_tag_calculation_match_sql(
                    storage, *sql_type, *op, calc, agg_ctx,
                ),
                ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
                    build_agg_calc_match_nest(agg, *op, calc, agg_ctx, nest_ctx)
                }
                ResolvedNode::CalculationCalculationMatch {
                    left_calc,
                    op,
                    right_calc,
                } => build_calculation_calculation_match_sql(
                    left_calc, *op, right_calc, agg_ctx,
                ),
                ResolvedNode::AggregationAggregationMatch {
                    left,
                    op,
                    right,
                } => build_agg_agg_match_nest(
                    left, *op, right, agg_ctx, nest_ctx,
                ),
                ResolvedNode::AggregationTagMatch {
                    agg,
                    op,
                    storage,
                    sql_type,
                    ..
                } => build_agg_tag_match_nest(
                    agg, *op, storage, *sql_type, agg_ctx, nest_ctx,
                ),
                ResolvedNode::NestMatch {
                    keys,
                    nvalue,
                    op,
                    label,
                    context,
                } => build_nest_match_sql(
                    keys, nvalue, *op, label, context, agg_ctx, nest_ctx,
                ),
                ResolvedNode::NestNestMatch {
                    left_keys,
                    left_nvalue,
                    left_context,
                    op,
                    right_keys,
                    right_nvalue,
                    right_context,
                } => build_nest_nest_match_sql(
                    left_keys,
                    left_nvalue,
                    left_context,
                    op,
                    right_keys,
                    right_nvalue,
                    right_context,
                    agg_ctx,
                    nest_ctx,
                ),
                _ => unreachable!("unexpected node type in build_pick_nest"),
            },
        }
    })
}

fn build_calculation_match_sql(
    calc: &ResolvedCalculationNode,
    op: ComparisonOp,
    label: &Label,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    if calc.contains_row_tag() {
        let mut stmt = Query::select();
        stmt.column(Col::ItemId)
            .from(Tbl::OneView)
            .group_by_col(Col::ItemId);
        let calc_expr = build_agg_calc_eav_expr(calc, agg_ctx);
        let label_expr = label_to_unit_aware_expr(label);
        stmt.and_having(
            Expr::expr(calc_expr).binary(to_bin_op(op), label_expr),
        );
        stmt
    } else {
        let mut stmt = Query::select();
        stmt.from(Tbl::OneView);
        stmt.column(Col::ItemId);
        let calc_expr = if calc.contains_aggregation() {
            build_agg_calc_subquery(calc, agg_ctx)
        } else {
            build_agg_calc_expr(calc, agg_ctx)
        };
        let label_expr = label_to_unit_aware_expr(label);
        let cond = Expr::expr(calc_expr).binary(to_bin_op(op), label_expr);
        stmt.cond_where(cond);
        if !calc.contains_aggregation() {
            add_type_filters(&mut stmt, calc);
        }
        stmt
    }
}

fn build_tag_calculation_match_sql(
    storage: &StorageMapping,
    sql_type: SqlType,
    op: ComparisonOp,
    calc: &ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let needs_eav = calc.contains_row_tag()
        || matches!(storage, StorageMapping::RowTag { .. });
    if needs_eav {
        build_tag_calc_match_eav_sql(storage, sql_type, op, calc, agg_ctx)
    } else {
        let mut stmt = Query::select();
        stmt.from(Tbl::OneView);
        stmt.column(Col::ItemId);
        let tag_expr = build_storage_column_expr(storage, sql_type);
        let calc_expr = if calc.contains_aggregation() {
            build_agg_calc_subquery(calc, agg_ctx)
        } else {
            build_agg_calc_expr(calc, agg_ctx)
        };
        let cond = Expr::expr(tag_expr).binary(to_bin_op(op), calc_expr);
        stmt.cond_where(cond);
        if !calc.contains_aggregation() {
            add_type_filters(&mut stmt, calc);
        }
        stmt
    }
}

fn build_agg_calc_match(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    calc: &ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Tbl::OneView);
    stmt.column(Col::ItemId);
    let agg_expr = subquery(build_agg(agg, agg_ctx));
    let calc_expr = if calc.contains_aggregation() {
        build_agg_calc_subquery(calc, agg_ctx)
    } else {
        build_agg_calc_expr(calc, agg_ctx)
    };
    stmt.cond_where(Expr::expr(agg_expr).binary(to_bin_op(op), calc_expr));
    stmt
}

fn build_agg_calc_match_nest(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    calc: &ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Tbl::OneView);
    stmt.column(Col::ItemId);
    let agg_expr = subquery(build_agg_nest(agg, agg_ctx, nest_ctx));
    let calc_expr = if calc.contains_aggregation() {
        build_agg_calc_subquery_nest(calc, agg_ctx, nest_ctx)
    } else {
        build_agg_calc_expr(calc, agg_ctx)
    };
    stmt.cond_where(Expr::expr(agg_expr).binary(to_bin_op(op), calc_expr));
    stmt
}

fn build_calculation_calculation_match_sql(
    left_calc: &ResolvedCalculationNode,
    op: ComparisonOp,
    right_calc: &ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.column(Col::ItemId)
        .from(Tbl::OneView)
        .group_by_col(Col::ItemId);
    let left_expr = build_agg_calc_eav_expr(left_calc, agg_ctx);
    let right_expr = build_agg_calc_eav_expr(right_calc, agg_ctx);
    stmt.and_having(Expr::expr(left_expr).binary(to_bin_op(op), right_expr));
    stmt
}

fn build_agg_agg_match(
    left: &ResolvedAggregationNode,
    op: ComparisonOp,
    right: &ResolvedAggregationNode,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Tbl::OneView);
    stmt.column(Col::ItemId);
    let left_expr = subquery(build_agg(left, agg_ctx));
    let right_expr = subquery(build_agg(right, agg_ctx));
    stmt.cond_where(Expr::expr(left_expr).binary(to_bin_op(op), right_expr));
    stmt
}

fn build_agg_agg_match_nest(
    left: &ResolvedAggregationNode,
    op: ComparisonOp,
    right: &ResolvedAggregationNode,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Tbl::OneView);
    stmt.column(Col::ItemId);
    let left_expr = subquery(build_agg_nest(left, agg_ctx, nest_ctx));
    let right_expr = subquery(build_agg_nest(right, agg_ctx, nest_ctx));
    stmt.cond_where(Expr::expr(left_expr).binary(to_bin_op(op), right_expr));
    stmt
}

fn build_agg_tag_match(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    storage: &StorageMapping,
    sql_type: SqlType,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Tbl::OneView);
    stmt.column(Col::ItemId);
    let agg_expr = subquery(build_agg(agg, agg_ctx));
    let tag_expr = build_storage_column_expr(storage, sql_type);
    stmt.cond_where(Expr::expr(agg_expr).binary(to_bin_op(op), tag_expr));
    if let StorageMapping::RowTag { tag_type, .. } = storage {
        stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
    }
    stmt
}

fn build_agg_tag_match_nest(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    storage: &StorageMapping,
    sql_type: SqlType,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Tbl::OneView);
    stmt.column(Col::ItemId);
    let agg_expr = subquery(build_agg_nest(agg, agg_ctx, nest_ctx));
    let tag_expr = build_storage_column_expr(storage, sql_type);
    stmt.cond_where(Expr::expr(agg_expr).binary(to_bin_op(op), tag_expr));
    if let StorageMapping::RowTag { tag_type, .. } = storage {
        stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
    }
    stmt
}

fn build_tag_calc_match_eav_sql(
    left_storage: &StorageMapping,
    left_sql_type: SqlType,
    op: ComparisonOp,
    calc: &ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let mut q = Query::select();
    q.column(Col::ItemId)
        .from(Tbl::OneView)
        .group_by_col(Col::ItemId);
    let left_expr = build_tag_value_agg_expr(left_storage, left_sql_type);
    let calc_expr = build_agg_calc_eav_expr(calc, agg_ctx);
    q.and_having(left_expr.binary(to_bin_op(op), calc_expr));
    q
}

fn build_resolved_operand_eav_row_expr(
    operand: &ResolvedOperand,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => build_tag_value_eav_row_expr(storage, *sql_type),
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] =
                child_results.try_into().unwrap();
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(agg) => agg_expr(agg, agg_ctx),
    })
}

fn decompose_agg(
    agg: &ResolvedAggregationNode,
    agg_ctx: &AggregationContext,
) -> (SimpleExpr, Option<ResolvedNode>, Option<String>) {
    match agg {
        ResolvedAggregationNode::Count(node) => {
            let (storage, cond, _) = node.extract_agg_parts();
            let tag_type;
            let expr = if let Some(s) = storage {
                let col = match s {
                    StorageMapping::Column(c) => {
                        tag_type = None;
                        *c
                    }
                    StorageMapping::RowTag {
                        column,
                        tag_type: key,
                    } => {
                        tag_type = Some(key.clone());
                        *column
                    }
                    _ => {
                        tag_type = None;
                        Col::LabelInt
                    }
                };
                Expr::col(col).count_distinct().into()
            } else {
                tag_type = None;
                Expr::col(Col::ItemId).count_distinct().into()
            };
            (expr, cond, tag_type)
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let (storage, cond, operand) = inner.extract_agg_parts();
            let tag_type = match &storage {
                Some(StorageMapping::RowTag { tag_type: key, .. }) => {
                    Some(key.clone())
                }
                _ => None,
            };
            let expr: SimpleExpr = if let Some(operand) = operand {
                let is_string = operand.is_string_type();
                let inner_expr =
                    build_resolved_operand_eav_row_expr(operand, agg_ctx);
                apply_arithmetic_agg(op, inner_expr, is_string)
            } else {
                let tag_row_expr = build_tag_value_eav_row_expr(
                    &storage.unwrap(),
                    SqlType::DOUBLE,
                );
                apply_arithmetic_agg(op, tag_row_expr, false)
            };
            (expr, cond, tag_type)
        }
    }
}

fn build_agg_match(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    label: &Label,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let (agg_expr, cond, tag_type) = decompose_agg(agg, agg_ctx);
    let mut stmt = Query::select();
    stmt.from(Tbl::OneView);

    let op_bin = to_bin_op(op);
    let rhs = Expr::val(label.as_i64());
    let condition = Expr::expr(agg_expr).binary(op_bin, rhs);

    let _target_type = match agg {
        ResolvedAggregationNode::Count(inner) => inner.get_projection(),
        ResolvedAggregationNode::Arithmetic { inner, .. } => {
            inner.get_projection()
        }
    };

    let mut final_cond = Condition::all();
    if let Some(key) = tag_type {
        final_cond =
            final_cond.add(Expr::col((Tbl::OneView, Col::Type)).eq(key));
    }

    if cond.is_some() {
        let inner_ptr = agg.inner_node() as *const ResolvedNode as usize;
        let pick_sql = agg_ctx
            .agg_filters
            .get(&inner_ptr)
            .expect("filter SQL must be pre-computed")
            .clone();
        let sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(pick_sql, Sub)
            .to_owned();
        final_cond = final_cond.add(Expr::col(Col::ItemId).in_subquery(sub));
    }
    stmt.cond_where(final_cond);

    stmt.cond_having(condition);

    stmt.expr_as(CustomFunc::any_value(Expr::col(Col::ItemId)), Col::ItemId);
    stmt.expr_as(
        Expr::val(<&'static str>::from(ItemKind::Volatile)),
        Col::ItemKind,
    );
    stmt.expr_as(
        Expr::val(<&'static str>::from(ItemKind::Volatile)),
        Col::Type,
    );
    stmt.expr_as(Expr::val(0i64), Col::Rank);
    stmt.expr_as(CustomFunc::list_value([]), QueryResultCol::Tags);
    stmt.limit(1);

    stmt.to_owned()
}

fn build_tag_value_eav_row_expr(
    storage: &StorageMapping,
    sql_type: crate::db::SqlType,
) -> SimpleExpr {
    match storage {
        StorageMapping::RowTag { tag_type, .. } => {
            let col_expr = build_storage_column_expr(storage, sql_type);
            Expr::case(Expr::col(Col::Type).eq(tag_type.as_str()), col_expr)
                .finally(Expr::val(None::<f64>))
                .into()
        }
        StorageMapping::Column(col) => Expr::col(*col).into(),
        StorageMapping::Virtual => Expr::val(None::<f64>).into(),
    }
}
