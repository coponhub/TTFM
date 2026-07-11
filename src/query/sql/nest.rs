// Copyright (C) 2026 Kensuke Aoyagi
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
    apply_arithmetic_agg, apply_arithmetic_op, build_agg_calc_eav_expr,
    build_agg_calc_expr, build_agg_operand_eav_expr,
    build_aggregation_context_for_operand, build_calculation_eav_expr,
    build_calculation_expr, build_nest_pivot_cte, build_nest_pivot_cte_no_agg,
    build_nvalue_cte, build_nvalue_cte_nest, build_nvalue_standalone_subquery,
    build_pick, label_to_simple_expr, label_to_unit_aware_expr,
    wrap_to_item_ids, AggregationContext, BuildPick, NestContext, PickNode,
};
use crate::db::{Col, CustomFunc, Pronoun::*, SqlType, Src, Tbl};
use crate::query::ast::{ArithmeticAggOp, ComparisonOp};
use crate::query::lens_resolver::{
    LabelSetOpKind, NestMatchCondition, NestMatchOp, ResolvedAggregationNode,
    ResolvedNode, ResolvedOperand,
};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{Label, SType, TagType};
use sea_query::{
    Alias, Condition, DynIden, Expr, ExprTrait, Func, IntoIden, Order, Query,
    SelectStatement, SimpleExpr,
};

// ── Projection SQL ─────────────────────────────────────────────────────────

pub(super) fn build_resolved_projection_sql(
    src: &Src,
    op: &ResolvedOperand,
) -> SelectStatement {
    op.fold(&|op, child_results: Vec<SelectStatement>| match op {
        ResolvedOperand::TagRef { tag_type, .. } => {
            let mut q = Query::select();
            q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
                .distinct()
                .from(src);
            let cond = ResolvedNode::Nest {
                keys: vec![op.clone()],
                nvalue: None,
                context: None,
            }
            .to_condition();
            q.cond_where(cond);
            if let TagType::Base(SType::TypedTag) = tag_type {
                q.and_where(Expr::col(Col::TypedTag).is_not_null());
            } else if let TagType::Base(SType::Origin) = tag_type {
                q.and_where(Expr::col(Col::Origin).is_not_null());
            }
            q
        }
        ResolvedOperand::Calculation(_) => {
            let [mut l, r]: [SelectStatement; 2] =
                child_results.try_into().unwrap();
            l.union(sea_query::UnionType::Intersect, r);
            l
        }
        _ => Query::select()
            .columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .distinct()
            .from(src)
            .to_owned(),
    })
}

pub(super) fn filter(
    src: &Src,
    keys: &[ResolvedOperand],
    ctx_sql: Option<SelectStatement>,
) -> SelectStatement {
    let mut stmt = build_resolved_projection_sql(src, keys.first().unwrap());
    for key in keys.iter().skip(1) {
        let key_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(build_resolved_projection_sql(src, key), Key)
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(key_sub));
    }
    if let Some(ctx) = ctx_sql {
        let ctx_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(ctx, Ctx)
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(ctx_sub));
    }
    stmt
}

// ── LabelSetOp helpers ─────────────────────────────────────────────────────

pub(super) fn extract_primary_storage_from_node(
    node: &ResolvedNode,
) -> Option<StorageMapping> {
    node.walk().into_iter().find_map(|n| match n {
        ResolvedNode::Nest { keys, .. } => match keys.first()? {
            ResolvedOperand::TagRef { storage, .. } => Some(storage.clone()),
            _ => None,
        },
        _ => None,
    })
}

/// Calculation キーを持つ Nest オペランドのラベル SELECT を構築します。
/// `(size: / 2)` のような算術式から代表値を SELECT します。
/// `build_calculation_expr` を使用（GROUP BY なし＋WHERE フィルタ済みコンテキスト向け）。
fn build_calculation_key_label_select(
    src: &Src,
    node: &ResolvedNode,
    ids_sql: SelectStatement,
) -> anyhow::Result<SelectStatement> {
    use crate::query::lens_resolver::ResolvedCalculationNode;
    let calc: &ResolvedCalculationNode = node
        .walk()
        .into_iter()
        .find_map(|n| match n {
            ResolvedNode::Nest { keys, .. } => match keys.first()? {
                ResolvedOperand::Calculation(c) => Some(c.as_ref()),
                _ => None,
            },
            _ => None,
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "label_set_op_sql: cannot determine label type from operand (not a Calculation key)"
            )
        })?;

    let calc_expr = build_calculation_expr(calc);
    let cast_expr = CustomFunc::as_representative(calc_expr);
    let cond = calc.to_condition();
    let mut s = Query::select();
    s.expr_as(cast_expr, Representative)
        .column(Col::ItemId)
        .from(src)
        .cond_where(cond)
        .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
    Ok(s)
}

pub(super) fn extract_multi_key_nest_operands(
    node: &ResolvedNode,
) -> Option<Vec<ResolvedOperand>> {
    node.walk().into_iter().find_map(|n| match n {
        ResolvedNode::Nest { keys, .. } if keys.len() > 1 => Some(keys.clone()),
        _ => None,
    })
}

pub(super) fn build_multi_key_labels_sql(
    src: &Src,
    keys: &[ResolvedOperand],
    ids_sql: SelectStatement,
) -> anyhow::Result<SelectStatement> {
    use std::collections::HashSet;
    let mut pivot = Query::select();
    pivot.column(Col::ItemId);
    let mut type_filters: HashSet<String> = HashSet::new();
    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef {
                storage: StorageMapping::Basic { tag_type, column },
                ..
            } => {
                type_filters.insert(tag_type.as_str().to_string());
                let case_expr = Expr::case(
                    Expr::col(Col::Type).eq(tag_type.as_str()),
                    Expr::col(*column),
                );
                let max_expr: SimpleExpr = Func::max(case_expr).into();
                pivot.expr_as(
                    max_expr.clone(),
                    Alias::new(&format!("key{}", i)),
                );
                pivot.and_having(max_expr.is_not_null());
            }
            ResolvedOperand::TagRef {
                storage: StorageMapping::Fixed(col),
                ..
            } => {
                let max_expr = Expr::col(*col).max();
                pivot.expr_as(
                    max_expr.clone(),
                    Alias::new(&format!("key{}", i)),
                );
                pivot.and_having(max_expr.is_not_null());
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "build_multi_key_labels_sql: unsupported key type at index {}",
                    i
                ));
            }
        }
    }
    pivot.from(src);
    if !type_filters.is_empty() {
        pivot.and_where(Expr::col(Col::Type).is_in(type_filters));
    }
    pivot
        .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql))
        .group_by_col(Col::ItemId);

    let union_type = "UNION(v VARCHAR, i BIGINT, d DOUBLE, b BOOLEAN, u UUID)";
    let repr_expr = Expr::cust(format!(
        "list_value({})",
        (0..keys.len())
            .map(|i| format!("CAST(\"key{}\" AS {})", i, union_type))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let mut outer = Query::select();
    outer
        .expr_as(repr_expr, Representative)
        .column(Col::ItemId)
        .from_subquery(pivot, Alias::new("pivot_sub"));
    Ok(outer)
}

// ── Nest match SQL ─────────────────────────────────────────────────────────

fn required_tag_types(node: &ResolvedNode) -> Vec<String> {
    node.walk()
        .into_iter()
        .filter_map(|n| {
            if let ResolvedNode::Match {
                storage: StorageMapping::Basic { tag_type, .. },
                ..
            } = n
            {
                Some(tag_type.clone())
            } else {
                None
            }
        })
        .collect()
}

fn resolve_simple_filter_condition<T: sea_query::Iden + Clone + 'static>(
    node: &ResolvedNode,
    table: T,
) -> Option<Condition> {
    node.fold(&|n, child_results: Vec<Option<Condition>>| match n {
        ResolvedNode::Match { storage, label, .. } => {
            if let StorageMapping::Basic { tag_type, .. } = storage {
                let s_val = label.as_str();
                let mut cond = Condition::all();
                if tag_type.as_str() != "*" {
                    cond = cond.add(
                        Expr::col((table.clone(), Col::Type))
                            .eq(tag_type.as_str()),
                    );
                }
                if s_val != "*" && s_val != "" {
                    cond = cond.add(
                        Expr::col((table.clone(), Col::LabelStr)).eq(s_val),
                    );
                }
                Some(cond)
            } else {
                None
            }
        }
        ResolvedNode::ColumnMatch { tag, label } => {
            let s_val = label.as_str();
            Some(
                Condition::all()
                    .add(Expr::col((table.clone(), *tag)).eq(s_val)),
            )
        }
        ResolvedNode::And(_) => {
            let mut required_tags = required_tag_types(n);
            required_tags.sort();
            required_tags.dedup();
            if required_tags.len() > 1 {
                return None;
            }
            child_results
                .into_iter()
                .try_fold(Condition::all(), |acc, c| {
                    c.map(|cond| acc.add(cond))
                })
        }
        _ => None,
    })
}

fn build_merged_nvalue_agg_expr(
    nvalue: &ResolvedOperand,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    nvalue.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            let (inner_tag, inner_filter, _) = inner.extract_agg_parts();
            if let Some(filter_node) = inner_filter {
                let case_expr: SimpleExpr = if let Some(cond) =
                    resolve_simple_filter_condition(&filter_node, View)
                {
                    if cond.is_empty() {
                        Expr::col((View, Col::ItemId)).into()
                    } else {
                        Expr::case(cond, Expr::col((View, Col::ItemId)))
                            .finally(Expr::val(None::<i32>))
                            .into()
                    }
                } else {
                    let inner_ptr =
                        inner.as_ref() as *const ResolvedNode as usize;
                    let filter_sql = agg_ctx
                        .agg_filters
                        .get(&inner_ptr)
                        .expect("filter SQL must be pre-computed")
                        .clone();
                    let filter_sub = Query::select()
                        .column(Col::ItemId)
                        .from_subquery(filter_sql, NvFilter)
                        .to_owned();
                    let in_expr =
                        Expr::col((View, Col::ItemId)).in_subquery(filter_sub);
                    Expr::case(in_expr, Expr::col((View, Col::ItemId)))
                        .finally(Expr::val(None::<i32>))
                        .into()
                };
                Expr::expr(case_expr).count_distinct().into()
            } else if let Some(StorageMapping::Basic { tag_type, .. }) =
                inner_tag
            {
                let cond = Condition::all()
                    .add(Expr::col((View, Col::Type)).eq(tag_type.as_str()));
                let case_expr =
                    Expr::case(cond, Expr::col((View, Col::ItemId)))
                        .finally(Expr::val(None::<i32>));
                Expr::expr(case_expr).count_distinct().into()
            } else {
                Expr::col((View, Col::ItemId)).count_distinct().into()
            }
        }
        ResolvedOperand::Aggregation(
            agg @ ResolvedAggregationNode::Arithmetic { op, inner },
        ) => {
            let is_string = agg.is_string_type();
            let (inner_tag, inner_filter, _operand) = inner.extract_agg_parts();

            let val_expr: SimpleExpr = if is_string {
                Expr::col((View, Col::LabelStr)).into()
            } else {
                Expr::cust_with_exprs(
                    "COALESCE($1, $2, TRY_CAST($3 AS DOUBLE))",
                    [
                        Expr::col((View, Col::LabelInt)).into(),
                        Expr::col((View, Col::LabelDouble)).into(),
                        Expr::col((View, Col::LabelStr)).into(),
                    ],
                )
            };

            if let Some(_filter_node) = inner_filter {
                let inner_ptr = inner.as_ref() as *const ResolvedNode as usize;
                let filter_sql = agg_ctx
                    .agg_filters
                    .get(&inner_ptr)
                    .expect("filter SQL must be pre-computed")
                    .clone();
                let filter_sub = Query::select()
                    .column(Col::ItemId)
                    .from_subquery(filter_sql, NvFilter)
                    .to_owned();
                let in_expr =
                    Expr::col((View, Col::ItemId)).in_subquery(filter_sub);
                let case_expr: SimpleExpr =
                    if let Some(StorageMapping::Basic { tag_type, .. }) =
                        inner_tag
                    {
                        let combined_cond = Condition::all()
                            .add(
                                Expr::col((View, Col::Type))
                                    .eq(tag_type.as_str()),
                            )
                            .add(in_expr);
                        Expr::case(combined_cond, val_expr.clone())
                            .finally(Expr::val(None::<f64>))
                            .into()
                    } else {
                        Expr::case(in_expr, val_expr.clone())
                            .finally(Expr::val(None::<f64>))
                            .into()
                    };
                apply_arithmetic_agg(op, case_expr, is_string)
            } else if let Some(StorageMapping::Basic { tag_type, .. }) =
                inner_tag
            {
                let cond = Condition::all()
                    .add(Expr::col((View, Col::Type)).eq(tag_type.as_str()));
                let case_expr =
                    Expr::case(cond, val_expr).finally(Expr::val(None::<f64>));
                apply_arithmetic_agg(
                    op,
                    Expr::expr(case_expr).into(),
                    is_string,
                )
            } else {
                apply_arithmetic_agg(op, Expr::expr(val_expr).into(), is_string)
            }
        }
        ResolvedOperand::Calculation(calc) => {
            let [left_expr, right_expr]: [SimpleExpr; 2] =
                child_results.try_into().unwrap();
            let left_val = Expr::expr(left_expr)
                .cast_as(crate::db::SqlType::DOUBLE)
                .into();
            let right_val = Expr::expr(right_expr)
                .cast_as(crate::db::SqlType::DOUBLE)
                .into();
            apply_arithmetic_op(&calc.op, left_val, right_val, false)
        }
        ResolvedOperand::Literal(l) => label_to_simple_expr(l),
        ResolvedOperand::TagRef { .. } => Expr::val(1i32).into(),
    })
}

pub(super) fn build_nest_having_sql(
    src: &Src,
    key: &ResolvedOperand,
    conditions: &[(&ResolvedOperand, ComparisonOp, &ResolvedOperand)],
    is_or: bool,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let (proj_col, proj_tag_type) = match key.get_storage() {
        Some(StorageMapping::Basic { column, tag_type }) => {
            (*column, Some(tag_type.as_str()))
        }
        Some(StorageMapping::Fixed(col)) => (*col, None),
        _ => panic!("NestMatch key must have Basic or Fixed storage"),
    };

    let mut nfilter = Query::select();
    nfilter.expr_as(
        CustomFunc::as_representative(Expr::col((Proj, proj_col))),
        Group,
    );
    nfilter.from_as(src, Proj);
    nfilter.join_as(
        sea_query::JoinType::InnerJoin,
        Tbl::OneView,
        View,
        Expr::col((Proj, Col::ItemId)).equals((View, Col::ItemId)),
    );
    if let Some(tag_type) = proj_tag_type {
        nfilter.and_where(Expr::col((Proj, Col::Type)).eq(tag_type));
    }
    nfilter.group_by_col((Proj, proj_col));

    let mut having_cond = if is_or {
        Condition::any()
    } else {
        Condition::all()
    };
    for (nvalue, cmp_op, right) in conditions {
        let bin_op = to_bin_op(*cmp_op);
        let lhs = build_merged_nvalue_agg_expr(nvalue, agg_ctx);
        let rhs = build_merged_nvalue_agg_expr(right, agg_ctx);
        having_cond = having_cond.add(Expr::expr(lhs).binary(bin_op, rhs));
    }
    nfilter.cond_having(having_cond);

    let group_label_sub = Query::select()
        .column(Group)
        .from_subquery(nfilter, Filter)
        .to_owned();

    let mut stmt = Query::select();
    stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
    stmt.distinct();
    stmt.from(src);
    if let Some(tag_type) = proj_tag_type {
        stmt.and_where(Expr::col(Col::Type).eq(tag_type));
    }
    stmt.and_where(
        CustomFunc::as_representative(Expr::col(proj_col))
            .in_subquery(group_label_sub),
    );
    stmt
}

pub(super) fn build_nest_match_sql(
    src: &Src,
    keys: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    comparison_op: ComparisonOp,
    label: &Label,
    context: &Option<Box<ResolvedNode>>,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    if keys.len() == 1 {
        let nvalue_sub = build_nvalue_standalone_subquery(
            src,
            &keys[0],
            nvalue,
            context.as_deref(),
            false,
            agg_ctx,
            Some(nest_ctx),
        );
        let bin_op = to_bin_op(comparison_op);
        let label_expr = label_to_unit_aware_expr(label);

        let mut nfilter = Query::select();
        nfilter.column(Group);
        nfilter.from_subquery(nvalue_sub, Filter);
        nfilter.and_where(Expr::col(Nvalue).binary(bin_op, label_expr));

        let (proj_col, proj_tag_type) = match keys[0].get_storage() {
            Some(StorageMapping::Basic { column, tag_type }) => {
                (*column, Some(tag_type.as_str()))
            }
            Some(StorageMapping::Fixed(col)) => (*col, None),
            _ => panic!("NestMatch key must have Basic or Fixed storage"),
        };
        let mut stmt = Query::select();
        stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        stmt.distinct();
        stmt.from(src);
        if let Some(tag_type) = proj_tag_type {
            stmt.and_where(Expr::col(Col::Type).eq(tag_type));
        }
        stmt.and_where(
            CustomFunc::as_representative(Expr::col(proj_col))
                .in_subquery(nfilter),
        );
        stmt
    } else {
        let pivot_sub = build_nest_pivot_cte(src, keys, Some(nvalue), agg_ctx);

        let mut stmt = Query::select();
        stmt.column(Col::ItemId);
        stmt.column(Col::Rank);
        stmt.column(Col::ItemKind);

        let partition_keys: Vec<SimpleExpr> = (0..keys.len())
            .map(|i| Expr::col(Alias::new(&format!("key{}", i))).into())
            .collect();

        let bin_op = to_bin_op(comparison_op);
        let label_expr = label_to_unit_aware_expr(label);

        let agg_func = match nvalue {
            ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_)) => {
                Func::sum(Expr::col(Nvalue))
            }
            ResolvedOperand::Aggregation(
                ResolvedAggregationNode::Arithmetic { op, .. },
            ) => match op {
                ArithmeticAggOp::Sum => Func::sum(Expr::col(Nvalue)),
                ArithmeticAggOp::Avg => Func::avg(Expr::col(Nvalue)),
                ArithmeticAggOp::Max => Func::max(Expr::col(Nvalue)),
                ArithmeticAggOp::Min => Func::min(Expr::col(Nvalue)),
            },
            _ => Func::max(Expr::col(Nvalue)),
        };

        use sea_query::{
            OverStatement, SelectExpr, WindowSelectType, WindowStatement,
        };
        let mut window = WindowStatement::new();
        for pk in &partition_keys {
            window.add_partition_by(pk.clone());
        }
        stmt.expr(SelectExpr {
            expr: agg_func.into(),
            alias: Some(Val.into_iden()),
            window: Some(WindowSelectType::Query(window)),
        });

        if let Some(ctx) = context {
            let ctx_ptr = ctx.as_ref() as *const ResolvedNode as usize;
            let ctx_sql = nest_ctx
                .contexts
                .get(&ctx_ptr)
                .expect("context SQL must be pre-computed")
                .clone();
            let mut psub = Query::select();
            psub.column(Col::ItemId).from_subquery(ctx_sql, Ctx);
            stmt.and_where(Expr::col(Col::ItemId).in_subquery(psub));
        }
        stmt.from_subquery(pivot_sub, Sub);

        let mut final_stmt = Query::select();
        final_stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        final_stmt.distinct();
        final_stmt.from_subquery(stmt, Sub);
        final_stmt.and_where(Expr::col(Val).binary(bin_op, label_expr));
        final_stmt
    }
}

pub(super) fn build_nest_nest_match_sql(
    src: &Src,
    left_keys: &[ResolvedOperand],
    left_nvalue: &ResolvedOperand,
    left_context: &Option<Box<ResolvedNode>>,
    op: &NestMatchOp,
    right_keys: &[ResolvedOperand],
    right_nvalue: &ResolvedOperand,
    right_context: &Option<Box<ResolvedNode>>,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    match op {
        NestMatchOp::Comparison(cmp_op) => {
            let is_agg_or_calc = matches!(
                right_nvalue,
                ResolvedOperand::Aggregation(_)
                    | ResolvedOperand::Calculation(_)
            );
            if is_agg_or_calc {
                let conditions = [(*cmp_op, right_nvalue)];
                let conds: Vec<_> = conditions
                    .iter()
                    .map(|(op, rhs)| (left_nvalue, *op, *rhs))
                    .collect();
                return build_nest_having_sql(
                    src,
                    &left_keys[0],
                    &conds,
                    false,
                    agg_ctx,
                );
            }

            let mut stmt = build_resolved_projection_sql(src, &left_keys[0]);
            let sub_l = build_nvalue_standalone_subquery(
                src,
                &left_keys[0],
                left_nvalue,
                left_context.as_deref(),
                true,
                agg_ctx,
                Some(nest_ctx),
            );
            let sub_r = build_nvalue_standalone_subquery(
                src,
                &right_keys[0],
                right_nvalue,
                right_context.as_deref(),
                true,
                agg_ctx,
                Some(nest_ctx),
            );
            let bin_op = to_bin_op(*cmp_op);
            let join_sql = Query::select()
                .column((L, Group))
                .from_subquery(sub_l, L)
                .join_subquery(
                    sea_query::JoinType::InnerJoin,
                    sub_r,
                    R,
                    Expr::col((L, Col::ItemId)).eq(Expr::col((R, Col::ItemId))),
                )
                .and_where(
                    Expr::col((L, Nvalue))
                        .binary(bin_op, Expr::col((R, Nvalue))),
                )
                .to_owned();
            let proj_col = match left_keys[0].get_storage() {
                Some(StorageMapping::Basic { column, .. }) => column,
                Some(StorageMapping::Fixed(col)) => col,
                _ => panic!(
                    "unexpected NestNestMatch with non-TagRef keys: {:?}",
                    left_keys
                ),
            };
            stmt.and_where(
                CustomFunc::as_representative(Expr::col(*proj_col))
                    .in_subquery(join_sql),
            );
            stmt
        }
    }
}

pub(super) fn build_merged_nest_match_sql(
    src: &Src,
    keys: &[ResolvedOperand],
    matches: &[NestMatchCondition],
    is_or: bool,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    if keys.len() == 1 {
        let conditions: Vec<_> = matches
            .iter()
            .map(|m| {
                let NestMatchOp::Comparison(cmp_op) = m.op;
                (&m.nvalue, cmp_op, &m.right)
            })
            .collect();
        build_nest_having_sql(src, &keys[0], &conditions, is_or, agg_ctx)
    } else {
        let mut all_nv_ops: Vec<&ResolvedOperand> = Vec::new();
        for m in matches {
            if !all_nv_ops.iter().any(|&o| o == &m.nvalue) {
                all_nv_ops.push(&m.nvalue);
            }
            if let ResolvedOperand::Aggregation(_) = &m.right {
                if !all_nv_ops.iter().any(|&o| o == &m.right) {
                    all_nv_ops.push(&m.right);
                }
            }
        }

        let pivot_sub =
            build_nest_pivot_multi_nv_cte(src, keys, &all_nv_ops, agg_ctx);

        let mut stmt = Query::select();
        stmt.column(Col::ItemId);
        stmt.column(Col::Rank);
        stmt.column(Col::ItemKind);

        let mut partition_keys: Vec<SimpleExpr> = Vec::new();
        for i in 0..keys.len() {
            partition_keys
                .push(Expr::col(Alias::new(&format!("key{}", i))).into());
        }

        use sea_query::{
            OverStatement, SelectExpr, WindowSelectType, WindowStatement,
        };

        let mut group_nv_aliases = Vec::new();
        for (idx, op) in all_nv_ops.iter().enumerate() {
            let nval_pivot_alias = format!("nv{}", idx);
            let group_nv_alias = format!("group_nv_{}", idx);

            let agg_func = match op {
                ResolvedOperand::Aggregation(
                    ResolvedAggregationNode::Count(_),
                ) => Func::sum(Expr::col(Alias::new(&nval_pivot_alias))),
                ResolvedOperand::Aggregation(
                    ResolvedAggregationNode::Arithmetic { op, .. },
                ) => match op {
                    ArithmeticAggOp::Sum => {
                        Func::sum(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    ArithmeticAggOp::Avg => {
                        Func::avg(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    ArithmeticAggOp::Max => {
                        Func::max(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    ArithmeticAggOp::Min => {
                        Func::min(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                },
                _ => Func::max(Expr::col(Alias::new(&nval_pivot_alias))),
            };

            let mut window = WindowStatement::new();
            for pk in &partition_keys {
                window.add_partition_by(pk.clone());
            }

            stmt.expr(SelectExpr {
                expr: agg_func.into(),
                alias: Some(Alias::new(&group_nv_alias).into_iden()),
                window: Some(WindowSelectType::Query(window)),
            });
            group_nv_aliases.push(group_nv_alias);
        }

        let mut filter_cond = if is_or {
            Condition::any()
        } else {
            Condition::all()
        };

        for m in matches {
            let left_idx =
                all_nv_ops.iter().position(|&o| o == &m.nvalue).unwrap();
            let left_group_nv = &group_nv_aliases[left_idx];

            let NestMatchOp::Comparison(cmp_op) = m.op;
            let bin_op = to_bin_op(cmp_op);

            let right_expr = if let ResolvedOperand::Aggregation(_) = &m.right {
                let right_idx =
                    all_nv_ops.iter().position(|&o| o == &m.right).unwrap();
                Expr::col(Alias::new(&group_nv_aliases[right_idx])).into()
            } else {
                build_agg_operand_eav_expr(&m.right, agg_ctx)
            };

            filter_cond = filter_cond.add(
                Expr::col(Alias::new(left_group_nv)).binary(bin_op, right_expr),
            );
        }

        stmt.from_subquery(pivot_sub, Sub);

        let mut final_stmt = Query::select();
        final_stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        final_stmt.distinct();
        final_stmt.from_subquery(stmt, Sub);
        final_stmt.and_where(filter_cond.into());
        final_stmt
    }
}

fn build_nest_pivot_multi_nv_cte(
    src: &Src,
    keys: &[ResolvedOperand],
    nvalues: &[&ResolvedOperand],
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.column(Col::ItemId);
    stmt.expr_as(
        crate::db::CustomFunc::any_value(Expr::col(Col::Rank)),
        Col::Rank,
    );
    stmt.expr_as(
        crate::db::CustomFunc::any_value(Expr::col(Col::ItemKind)),
        Col::ItemKind,
    );
    stmt.from(src);

    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef { storage, .. } => match storage {
                StorageMapping::Basic { tag_type, column } => {
                    let case_expr = Expr::case(
                        Expr::col(Col::Type).eq(tag_type.as_str()),
                        Expr::col(*column),
                    );
                    stmt.expr_as(
                        Func::max(case_expr),
                        Alias::new(&format!("key{}", i)),
                    );
                }
                StorageMapping::Fixed(col) => {
                    stmt.expr_as(
                        Expr::col(*col).max(),
                        Alias::new(&format!("key{}", i)),
                    );
                }
                _ => {}
            },
            ResolvedOperand::Calculation(calc) => {
                let calc_expr = build_agg_calc_eav_expr(calc, agg_ctx);
                stmt.expr_as(calc_expr, Alias::new(&format!("key{}", i)));
            }
            _ => {}
        }
    }

    for (i, nv) in nvalues.iter().enumerate() {
        let nv_expr = build_agg_operand_eav_expr(nv, agg_ctx);
        stmt.expr_as(nv_expr, Alias::new(&format!("nv{}", i)));
    }

    stmt.group_by_col(Col::ItemId);
    stmt
}

// ── Nest + LabelSetOp high-level SQL ──────────────────────────────────────

pub(super) fn build_fetch_nest_sql(
    src: &Src,
    resolver: &crate::query::lens_resolver::Resolver,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    if let Some(node) = resolver.get_label_set_op_node() {
        label_set_op_sql(src, node, limit, offset)
    } else {
        let pick = PickNode::new(src, &resolver.resolved_query);
        nest(src, &pick, resolver, limit, offset)
    }
}

fn make_tag_struct_pack(
    type_str: &str,
    sql_type: SqlType,
    value_expr: impl Into<SimpleExpr>,
) -> SimpleExpr {
    // 常にクエリ時にエンジンが合成する信号タグであり、Builtin (TTFM エンジン自身) が origin となる。
    CustomFunc::struct_pack_tag(
        Expr::val(type_str).into(),
        CustomFunc::union_value(sql_type, value_expr),
        Expr::val(crate::types::Origin::Builtin.as_str()).into(),
    )
}

pub(super) fn nest(
    src: &Src,
    pick: &PickNode<'_>,
    resolver: &crate::query::lens_resolver::Resolver,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    use crate::db::CustomFunc;
    use sea_query::{CommonTableExpression, Iden, WithClause};

    let pick_sql = pick.build_pick();

    let proj_type =
        resolver.resolved_query.get_projection().ok_or_else(|| {
            anyhow::anyhow!("nest: no projection type in resolved query")
        })?;
    let desc = resolver.lens().look_up_or_default(&proj_type);
    let col_iden = match &desc.storage {
        StorageMapping::Fixed(col) => *col,
        StorageMapping::Basic { column, .. } => *column,
        _ => anyhow::bail!(
            "Unsupported storage for projection: {:?}",
            desc.storage
        ),
    };

    let mut with_clause = WithClause::new();

    let picked_ids_cte = CommonTableExpression::new()
        .query(wrap_to_item_ids(pick_sql))
        .table_name(PickedIds)
        .to_owned();
    with_clause.cte(picked_ids_cte);

    let is_or_query = matches!(&resolver.resolved_query, ResolvedNode::Or(_));
    let nvalue_condition = resolver.get_nvalue_condition();
    let has_nvalue = if !is_or_query {
        if let Some(nv) = resolver.get_nvalue() {
            let proj_operands =
                resolver.resolved_query.get_projection_operands().unwrap();
            let context = resolver.resolved_query.get_context();
            let computed_agg_ctx;
            let mut nvalue_sql = if let (Some(agg_ctx), Some(nest_ctx)) =
                (pick.agg_ctx(), pick.nest_ctx())
            {
                build_nvalue_cte_nest(
                    src,
                    proj_operands,
                    nv,
                    context,
                    agg_ctx,
                    nest_ctx,
                )
            } else {
                computed_agg_ctx =
                    build_aggregation_context_for_operand(src, nv);
                build_nvalue_cte(
                    src,
                    proj_operands,
                    nv,
                    context,
                    &computed_agg_ctx,
                )
            };
            if let Some((op, value)) = nvalue_condition {
                let bin_op = to_bin_op(*op);
                let val = label_to_simple_expr(value);
                let cond = Expr::col(Nvalue).binary(bin_op, val);
                if matches!(nv, ResolvedOperand::Calculation(_)) {
                    nvalue_sql.and_where(cond);
                } else {
                    nvalue_sql.and_having(cond);
                }
            }
            let nvalue_cte = CommonTableExpression::new()
                .query(nvalue_sql)
                .table_name(Alias::new("nvalue_agg"))
                .to_owned();
            with_clause.cte(nvalue_cte);
            true
        } else {
            false
        }
    } else {
        false
    };
    let must_filter_by_nvalue = has_nvalue;

    let proj_operands =
        resolver.resolved_query.get_projection_operands().unwrap();
    let calc_node = match &proj_operands[0] {
        crate::query::lens_resolver::ResolvedOperand::Calculation(c) => Some(c),
        _ => None,
    };

    let (label_col_name, all_hits_source_cte, need_extra_filter) =
        if proj_operands.len() > 1 {
            let pivot_q = match pick.agg_ctx() {
                Some(agg_ctx) => {
                    build_nest_pivot_cte(src, proj_operands, None, agg_ctx)
                }
                None => build_nest_pivot_cte_no_agg(src, proj_operands),
            };
            let pivot_cte = CommonTableExpression::new()
                .query(pivot_q)
                .table_name(Pivot)
                .to_owned();
            with_clause.cte(pivot_cte);
            ("key0".to_string(), Some("pivot".to_string()), false)
        } else if let Some(calc) = calc_node {
            if calc.contains_tag() {
                let calc_expr = if calc.contains_aggregation() {
                    let agg_ctx = pick.agg_ctx().expect(
                    "AggregationContext required for EAV+agg calculation CTE",
                );
                    build_agg_calc_eav_expr(&calc, agg_ctx)
                } else {
                    build_calculation_eav_expr(&calc)
                };
                let mut computed_q = Query::select();
                computed_q
                    .column(Col::ItemId)
                    .expr_as(calc_expr, Alias::new("calc_value"))
                    .expr_as(
                        CustomFunc::any_value(Expr::col(Col::Rank)),
                        Col::Rank,
                    )
                    .from(src)
                    .and_where(
                        Expr::col(Col::ItemId).in_subquery(
                            Query::select()
                                .column(Col::ItemId)
                                .from(PickedIds)
                                .to_owned(),
                        ),
                    )
                    .group_by_col(Col::ItemId);
                let computed_cte = CommonTableExpression::new()
                    .query(computed_q)
                    .table_name(Alias::new("computed"))
                    .to_owned();
                with_clause.cte(computed_cte);
                (
                    "calc_value".to_string(),
                    Some("computed".to_string()),
                    false,
                )
            } else {
                let calc_expr = if calc.contains_aggregation() {
                    let agg_ctx = pick.agg_ctx().expect(
                        "AggregationContext required for calculation CTE",
                    );
                    build_agg_calc_expr(&calc, agg_ctx)
                } else {
                    build_calculation_expr(&calc)
                };
                let mut computed_q = Query::select();
                computed_q
                    .column(Col::ItemId)
                    .expr_as(calc_expr, Alias::new("calc_value"))
                    .column(Col::Rank)
                    .from(src)
                    .and_where(
                        Expr::col(Col::ItemId).in_subquery(
                            Query::select()
                                .column(Col::ItemId)
                                .from(PickedIds)
                                .to_owned(),
                        ),
                    );
                let computed_cte = CommonTableExpression::new()
                    .query(computed_q)
                    .table_name(Alias::new("computed"))
                    .to_owned();
                with_clause.cte(computed_cte);
                (
                    "calc_value".to_string(),
                    Some("computed".to_string()),
                    false,
                )
            }
        } else if matches!(&desc.storage, StorageMapping::Fixed(_)) {
            // OneView は item_references を複数行に unpivot し intrinsic Fixed 列
            // (rank/origin/item_id/item_kind) を全行に複製するため、window 関数の前に
            // (item_id, col) で畳んで item 単位を保証する。file 由来の Fixed
            // (mtime/size/path) は元々 1 item 1 行なので畳んでも無害。
            let mut deduped_q = Query::select();
            deduped_q.distinct().column(Col::ItemId).column(col_iden);
            if col_iden != Col::Rank {
                // all_hits が引く rank 列を供給（col == rank の重複カラムを回避）
                deduped_q.column(Col::Rank);
            }
            deduped_q
                .from(src)
                .and_where(Expr::col(col_iden).is_not_null())
                .and_where(
                    Expr::col(Col::ItemId).in_subquery(
                        Query::select()
                            .column(Col::ItemId)
                            .from(PickedIds)
                            .to_owned(),
                    ),
                );
            let deduped_q = crate::query::lens_builder::complement_type_groups(
                deduped_q, col_iden,
            );
            let deduped_cte = CommonTableExpression::new()
                .query(deduped_q)
                .table_name(Deduped)
                .to_owned();
            with_clause.cte(deduped_cte);
            (
                Iden::to_string(&col_iden),
                Some(Iden::to_string(&Deduped)),
                false,
            )
        } else {
            (Iden::to_string(&col_iden), None::<String>, true)
        };

    let label_col = Alias::new(&label_col_name);

    let partition_cols: Vec<DynIden> = if proj_operands.len() > 1 {
        (0..proj_operands.len())
            .map(|i| Alias::new(&format!("key{}", i)).into_iden())
            .collect()
    } else {
        vec![label_col.clone().into_iden()]
    };

    let mut all_hits_q = Query::select();
    all_hits_q.column(Col::ItemId);
    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            all_hits_q.column(Alias::new(&format!("key{}", i)));
        }
    } else {
        all_hits_q.column(label_col.clone());
    }
    all_hits_q
        .column(Col::Rank)
        .expr_as(
            CustomFunc::row_number_over_multi(
                &partition_cols,
                vec![(Col::Rank, Order::Desc), (Col::ItemId, Order::Desc)],
            ),
            Rn,
        )
        .expr_as(CustomFunc::count_over_multi(&partition_cols), GroupTotal)
        .distinct();
    match &all_hits_source_cte {
        Some(cte_name) => all_hits_q.from(Alias::new(cte_name.as_str())),
        None => all_hits_q.from(src),
    };
    all_hits_q.and_where(
        Expr::col(Col::ItemId).in_subquery(
            Query::select()
                .column(Col::ItemId)
                .from(PickedIds)
                .to_owned(),
        ),
    );

    if need_extra_filter {
        all_hits_q.and_where(Expr::col(label_col.clone()).is_not_null());
        if let StorageMapping::Basic { tag_type, .. } = &desc.storage {
            all_hits_q.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
        }
    }

    if must_filter_by_nvalue {
        if proj_operands.len() > 1 {
            let keys_str = (0..proj_operands.len())
                .map(|i| format!("\"key{}\"", i))
                .collect::<Vec<_>>()
                .join(", ");
            all_hits_q.and_where(Expr::cust(format!(
                "({}) IN (SELECT {} FROM \"nvalue_agg\")",
                keys_str, keys_str
            )));
        } else {
            all_hits_q.and_where(
                CustomFunc::as_representative(Expr::col(label_col.clone()))
                    .in_subquery(
                        Query::select()
                            .column(Group)
                            .from(Alias::new("nvalue_agg"))
                            .to_owned(),
                    ),
            );
        }
    }

    let all_hits_cte = CommonTableExpression::new()
        .query(all_hits_q)
        .table_name(AllHits)
        .to_owned();
    with_clause.cte(all_hits_cte);

    let mut top_items_q = Query::select();
    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            top_items_q.column(Alias::new(&format!("key{}", i)));
        }
    } else {
        top_items_q.column(label_col.clone());
    }
    top_items_q
        .column(Col::ItemId)
        .column(Col::Rank)
        .column(GroupTotal)
        .from(AllHits)
        .and_where(Expr::col(Rn).lte(100));

    let top_items_cte = CommonTableExpression::new()
        .query(top_items_q)
        .table_name(TopItems)
        .to_owned();
    with_clause.cte(top_items_cte);

    // 複数のオペランドを一つのリストにまとめる。
    // 各オペランドが既にリスト型（Representative）である可能性があるため、
    // list_value で包む前に、スカラーかリストかを判断して適切に処理する必要がある。
    // ここでは DuckDB の flatten() または list_append/list_concat 的な振る舞いを目指す。

    let repr_expr = if proj_operands.len() > 1 {
        // 全要素が既にリスト(key0, key1...)であることが保証されているため、単純に flatten する。
        Expr::cust(format!(
            "flatten(list_value({}))",
            (0..proj_operands.len())
                .map(
                    |i| format!("{}.\"key{}\"", Iden::to_string(&TopItems), i,)
                )
                .collect::<Vec<_>>()
                .join(", ")
        ))
    } else {
        if label_col_name
            == Iden::to_string(&crate::db::Pronoun::Representative)
        {
            Expr::cust(format!(
                "{}.{}",
                Iden::to_string(&TopItems),
                label_col_name
            ))
        } else {
            CustomFunc::as_representative(Expr::col((
                TopItems,
                label_col.clone(),
            )))
        }
    };

    let mut name_select = Query::select();
    name_select
        .column(Col::LabelStr)
        .from(src)
        .and_where(
            Expr::col(Col::ItemId).eq(Expr::col((TopItems, Col::ItemId))),
        )
        .and_where(
            Expr::col(Col::Type).eq(Iden::to_string(&crate::db::Val::Name)),
        );
    let item_id_col = format!(
        "{}.{}",
        Iden::to_string(&TopItems),
        Iden::to_string(&Col::ItemId)
    );
    let item_label_expr = Func::cust(crate::db::DuckDbFunc::Concat).args([
        Func::cust(crate::db::DuckDbFunc::Coalesce)
            .args([
                super::util::subquery(name_select),
                Expr::val("unknown").into(),
            ])
            .into(),
        Expr::val("#").into(),
        Expr::cust(crate::db::CustomFunc::item_id_display(&item_id_col)),
    ]);
    let item_sp =
        make_tag_struct_pack("item", SqlType::VARCHAR, item_label_expr);
    let item_list_expr = Expr::cust_with_exprs(
        &format!(
            "list($1 ORDER BY {}.{} DESC, {}.{} DESC)",
            Iden::to_string(&TopItems),
            Iden::to_string(&Col::Rank),
            Iden::to_string(&TopItems),
            Iden::to_string(&Col::ItemId),
        ),
        [item_sp],
    );

    let proj_label_sp = make_tag_struct_pack(
        "item_count",
        SqlType::BIGINT,
        Expr::cust(format!(
            "ANY_VALUE({}.{})::BIGINT",
            Iden::to_string(&TopItems),
            Iden::to_string(&GroupTotal),
        )),
    );

    let mut tags_expr: SimpleExpr = Expr::cust_with_exprs(
        "$1 || list_value($2)",
        [item_list_expr, proj_label_sp],
    );

    if has_nvalue {
        let nvalue_subq_expr = if proj_operands.len() > 1 {
            let mut join_cond = "TRUE".to_string();
            for i in 0..proj_operands.len() {
                join_cond.push_str(&format!(
                    " AND \"nvalue_agg\".\"key{}\" = {}.\"key{}\"",
                    i,
                    Iden::to_string(&TopItems),
                    i
                ));
            }
            Expr::cust(format!(
                "(SELECT \"nvalue\" FROM \"nvalue_agg\" WHERE {})",
                join_cond
            ))
        } else {
            let mut sub = Query::select();
            sub.column(Nvalue).from(Alias::new("nvalue_agg"));
            if label_col_name
                == Iden::to_string(&crate::db::Pronoun::Representative)
            {
                sub.and_where(
                    Expr::col(Group)
                        .eq(Expr::col((TopItems, Alias::new(&label_col_name)))),
                );
            } else {
                sub.and_where(Expr::col(Group).eq(
                    CustomFunc::as_representative(Expr::col((
                        TopItems,
                        Alias::new(&label_col_name),
                    ))),
                ));
            }
            crate::query::sql::subquery(sub.to_owned())
        };
        let nvalue_sp = make_tag_struct_pack(
            "nvalue",
            SqlType::DOUBLE,
            Expr::cust_with_exprs("CAST(($1) AS DOUBLE)", [nvalue_subq_expr]),
        );
        tags_expr = Expr::cust_with_exprs(
            "$1 || list_value($2)",
            [tags_expr, nvalue_sp],
        );
    }

    let mut q = Query::select();
    q.with_cte(with_clause);
    // 揮発 id は SQL 側では NULL とし、fetch 後に Rust 側で採番する。
    q.expr_as(Expr::val(None::<i64>), Col::ItemId);
    q.expr_as(
        Expr::cust(format!(
            "ANY_VALUE({}.{})",
            Iden::to_string(&TopItems),
            Iden::to_string(&Col::Rank)
        )),
        Col::Rank,
    );
    q.expr_as(Expr::val("volatile"), Col::ItemKind);
    q.expr_as(tags_expr, crate::db::QueryResultCol::Tags);
    q.expr_as(repr_expr, Representative);
    q.from(TopItems);

    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            q.group_by_col((TopItems, Alias::new(&format!("key{}", i))));
        }
        for i in (0..proj_operands.len()).rev() {
            q.order_by(
                (TopItems, Alias::new(&format!("key{}", i))),
                sea_query::Order::Asc,
            );
        }
    } else {
        q.group_by_col((TopItems, label_col.clone()));
        q.order_by((TopItems, label_col), sea_query::Order::Asc);
    }

    if limit > 0 {
        q.limit((limit + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }

    Ok(q)
}

fn label_set_op_sql(
    src: &Src,
    label_set_op: &ResolvedNode,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    use sea_query::{CommonTableExpression, Iden, WithClause};

    let (op, operands) = match label_set_op {
        ResolvedNode::LabelSetOp { op, operands } => (op, operands),
        _ => anyhow::bail!("label_set_op_sql: expected LabelSetOp node"),
    };
    if operands.is_empty() {
        anyhow::bail!("label_set_op_sql: LabelSetOp with no operands");
    }

    let mut with_clause = WithClause::new();

    let cte_names: Vec<String> = (0..operands.len())
        .map(|i| format!("labels_{}", i))
        .collect();
    for (i, operand) in operands.iter().enumerate() {
        let ids_sql = wrap_to_item_ids(build_pick(src, operand));

        let labels_sql = if matches!(op, LabelSetOpKind::Union) {
            if let Some(keys) = extract_multi_key_nest_operands(operand) {
                build_multi_key_labels_sql(src, &keys, ids_sql)?
            } else if let Some(storage) =
                extract_primary_storage_from_node(operand)
            {
                storage.to_label_select(src, ids_sql).ok_or_else(|| {
                    anyhow::anyhow!(
                        "label_set_op_sql: Virtual storage cannot generate label select for operand {}", i
                    )
                })?
            } else {
                build_calculation_key_label_select(src, operand, ids_sql)?
            }
        } else if let Some(storage) = extract_primary_storage_from_node(operand)
        {
            storage.to_label_select(src, ids_sql).ok_or_else(|| {
                anyhow::anyhow!(
                    "label_set_op_sql: Virtual storage cannot generate label select for operand {}", i
                )
            })?
        } else {
            build_calculation_key_label_select(src, operand, ids_sql)?
        };

        with_clause.cte(
            CommonTableExpression::new()
                .query(labels_sql)
                .table_name(Alias::new(&cte_names[i]))
                .to_owned(),
        );
    }

    let use_item_level_except = matches!(op, LabelSetOpKind::Except)
        && extract_multi_key_nest_operands(operands.first().unwrap()).is_some();

    if use_item_level_except {
        let right_ids_sql = wrap_to_item_ids(build_pick(src, &operands[1]));

        let mut labels_sql = Query::select();
        labels_sql
            .expr_as(Expr::col(Representative), Label)
            .column(Col::ItemId)
            .from(Alias::new(&cte_names[0]))
            .and_where(Expr::col(Col::ItemId).not_in_subquery(right_ids_sql));
        with_clause.cte(
            CommonTableExpression::new()
                .query(labels_sql)
                .table_name(Alias::new("labels"))
                .to_owned(),
        );
    } else {
        let set_op_type = match op {
            LabelSetOpKind::Intersect => sea_query::UnionType::Intersect,
            LabelSetOpKind::Union => sea_query::UnionType::Distinct,
            LabelSetOpKind::Except => sea_query::UnionType::Except,
        };
        let mut op_labels_sql = Query::select()
            .column(Representative)
            .from(Alias::new(&cte_names[0]))
            .to_owned();
        for name in &cte_names[1..] {
            let other = Query::select()
                .column(Representative)
                .from(Alias::new(name))
                .to_owned();
            op_labels_sql.union(set_op_type, other);
        }
        with_clause.cte(
            CommonTableExpression::new()
                .query(op_labels_sql)
                .table_name(Alias::new("op_labels"))
                .to_owned(),
        );

        let mut all_op_items_sql = Query::select()
            .column(Representative)
            .column(Col::ItemId)
            .from(Alias::new(&cte_names[0]))
            .to_owned();
        for name in &cte_names[1..] {
            let other = Query::select()
                .column(Representative)
                .column(Col::ItemId)
                .from(Alias::new(name))
                .to_owned();
            all_op_items_sql.union(sea_query::UnionType::Distinct, other);
        }
        with_clause.cte(
            CommonTableExpression::new()
                .query(all_op_items_sql)
                .table_name(Alias::new("all_op_items"))
                .to_owned(),
        );

        let mut labels_sql = Query::select();
        labels_sql
            .expr_as(Expr::col(Representative), Label)
            .column(Col::ItemId)
            .from(Alias::new("all_op_items"))
            .and_where(
                Expr::col(Representative).in_subquery(
                    Query::select()
                        .column(Representative)
                        .from(Alias::new("op_labels"))
                        .to_owned(),
                ),
            );
        with_clause.cte(
            CommonTableExpression::new()
                .query(labels_sql)
                .table_name(Alias::new("labels"))
                .to_owned(),
        );
    }

    let all_hits_sql = Query::select()
        .column(Col::ItemId)
        .column(Label)
        .expr_as(
            CustomFunc::row_number_over_multi(
                &[Label.into_iden()],
                vec![(Col::ItemId, Order::Desc)],
            ),
            Rn,
        )
        .expr_as(CustomFunc::count_over(Label), GroupTotal)
        .from(Alias::new("labels"))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(all_hits_sql)
            .table_name(AllHits)
            .to_owned(),
    );

    let top_items_sql = Query::select()
        .column(Col::ItemId)
        .column(Label)
        .column(GroupTotal)
        .from(AllHits)
        .and_where(Expr::col(Rn).lte(100))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(top_items_sql)
            .table_name(TopItems)
            .to_owned(),
    );

    let repr_expr =
        Expr::cust(format!("list_value({}.label)", Iden::to_string(&TopItems)));

    let mut name_select = Query::select();
    name_select
        .column(Col::LabelStr)
        .from(src)
        .and_where(
            Expr::col(Col::ItemId).eq(Expr::col((TopItems, Col::ItemId))),
        )
        .and_where(
            Expr::col(Col::Type).eq(Iden::to_string(&crate::db::Val::Name)),
        );
    let item_id_col = format!(
        "{}.{}",
        Iden::to_string(&TopItems),
        Iden::to_string(&Col::ItemId)
    );
    let item_label_expr = Func::cust(crate::db::DuckDbFunc::Concat).args([
        Func::cust(crate::db::DuckDbFunc::Coalesce)
            .args([
                super::util::subquery(name_select),
                Expr::val("unknown").into(),
            ])
            .into(),
        Expr::val("#").into(),
        Expr::cust(crate::db::CustomFunc::item_id_display(&item_id_col)),
    ]);
    let item_sp =
        make_tag_struct_pack("item", SqlType::VARCHAR, item_label_expr);
    let item_list_expr = Expr::cust_with_exprs(
        &format!(
            "list($1 ORDER BY {}.{} DESC)",
            Iden::to_string(&TopItems),
            Iden::to_string(&Col::ItemId),
        ),
        [item_sp],
    );

    let proj_label_sp = make_tag_struct_pack(
        "item_count",
        SqlType::BIGINT,
        Expr::cust(format!(
            "ANY_VALUE({}.{})::BIGINT",
            Iden::to_string(&TopItems),
            Iden::to_string(&GroupTotal),
        )),
    );

    let tags_expr: SimpleExpr = Expr::cust_with_exprs(
        "$1 || list_value($2)",
        [item_list_expr, proj_label_sp],
    );

    let mut q = Query::select();
    q.with_cte(with_clause);
    // 揮発 id は SQL 側では NULL とし、fetch 後に Rust 側で採番する。
    q.expr_as(Expr::val(None::<i64>), Col::ItemId);
    q.expr_as(Expr::val(0i64), Col::Rank);
    q.expr_as(Expr::val("volatile"), Col::ItemKind);
    q.expr_as(tags_expr, crate::db::QueryResultCol::Tags);
    q.expr_as(repr_expr, Representative);
    q.from(TopItems)
        .group_by_col((TopItems, Label))
        .order_by((TopItems, Label), sea_query::Order::Asc);

    if limit > 0 {
        q.limit((limit + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }

    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::lens_resolver::Resolver;
    use crate::tag::TagRegistry;
    use sea_query::PostgresQueryBuilder;

    #[test]
    fn test_build_fetch_nest_sql_generates_concat() {
        let query_str = "extension:";
        let resolver = Resolver::new(query_str, &TagRegistry::with_standard())
            .expect("Failed to resolve");

        let sql = build_fetch_nest_sql(&Src::OneView, &resolver, 100, 0)
            .expect("Failed to build SQL");
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(
            sql_str.contains("struct_pack"),
            "SQL should contain struct_pack: {}",
            sql_str
        );
        assert!(
            sql_str.contains("'name'"),
            "SQL should contain name tag: {}",
            sql_str
        );
        assert!(
            sql_str.contains("item_count"),
            "SQL should contain projected_label tag: {}",
            sql_str
        );
    }

    #[test]
    fn test_nvalue_count_projection_sql() {
        let resolver = Resolver::new(
            "parentdir: &: count(extension:jpg)",
            &TagRegistry::with_standard(),
        )
        .unwrap();

        assert!(
            resolver.get_nvalue().is_some(),
            "Should have nvalue for nest query"
        );

        let sql =
            build_fetch_nest_sql(&Src::OneView, &resolver, 100, 0).unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(
            sql_str.contains("nvalue_agg"),
            "SQL should contain nvalue_agg CTE: {}",
            sql_str
        );
        assert!(
            sql_str.contains("nvalue"),
            "SQL should contain nvalue column: {}",
            sql_str
        );
        assert!(
            sql_str.contains("struct_pack"),
            "SQL should contain struct_pack: {}",
            sql_str
        );
    }

    #[test]
    fn test_nvalue_sum_projection_sql() {
        let resolver = Resolver::new(
            "parentdir: &: sum(size:)",
            &TagRegistry::with_standard(),
        )
        .unwrap();
        assert!(
            resolver.get_nvalue().is_some(),
            "Should have nvalue for nest query"
        );

        let sql =
            build_fetch_nest_sql(&Src::OneView, &resolver, 100, 0).unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(
            sql_str.contains("nvalue_agg"),
            "SQL should contain nvalue_agg CTE for query: {}",
            sql_str
        );
        assert!(
            sql_str.contains("nvalue"),
            "SQL should contain nvalue column for query: {}",
            sql_str
        );
        assert!(
            sql_str.contains("SUM"),
            "SQL should contain SUM for arithmetic nvalue for query: {}",
            sql_str
        );
    }

    #[test]
    fn test_fetch_projection_no_nvalue_regression() {
        let resolver =
            Resolver::new("extension:", &TagRegistry::with_standard()).unwrap();

        assert!(
            resolver.get_nvalue().is_none(),
            "Normal projection should NOT have nvalue"
        );

        let sql =
            build_fetch_nest_sql(&Src::OneView, &resolver, 100, 0).unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(
            !sql_str.contains("nvalue_agg"),
            "Normal projection should NOT contain nvalue_agg: {}",
            sql_str
        );
    }

    #[test]
    fn test_nvalue_condition_having_sql() {
        let resolver = Resolver::new(
            "parentdir: &: (count(extension:jpg) > 1)",
            &TagRegistry::with_standard(),
        )
        .unwrap();

        assert!(resolver.get_nvalue_condition().is_some());

        let sql =
            build_fetch_nest_sql(&Src::OneView, &resolver, 100, 0).unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(
            sql_str.contains("HAVING"),
            "SQL should contain HAVING for nvalue condition: {}",
            sql_str
        );
        assert!(
            sql_str.contains("nvalue_agg"),
            "SQL should use nvalue_agg for filtering: {}",
            sql_str
        );
    }

    #[test]
    fn test_build_nvalue_standalone_calculation_sql() {
        let operators = [
            ("sum(size:) + count(size:)", "+"),
            ("sum(size:) - count(size:)", "-"),
            ("sum(size:) * count(size:)", "*"),
            ("sum(size:) / count(size:)", "/"),
            ("sum(size:) % count(size:)", "%"),
        ];

        for (query_str, expected_op) in operators {
            let query = format!("parentdir: &: ({})", query_str);
            let resolver =
                Resolver::new(&query, &TagRegistry::with_standard()).unwrap();
            let proj_operand = resolver
                .resolved_query
                .get_projection_operand()
                .expect("Should have projection");
            let nvalue = resolver
                .resolved_query
                .get_nvalue_combined()
                .expect("Should have nvalue");

            let sql = build_nvalue_standalone_subquery(
                &Src::OneView,
                proj_operand,
                &nvalue,
                resolver.resolved_query.get_context(),
                false,
                &AggregationContext::new(),
                None,
            );
            let sql_str = sql.to_string(PostgresQueryBuilder);

            assert!(
                sql_str.contains("JOIN"),
                "SQL should contain JOIN for query: {}",
                query
            );
            assert!(
                sql_str.contains(expected_op),
                "SQL should contain '{}' operator for query: {}",
                expected_op,
                query
            );
            assert!(
                sql_str.contains("\"L\""),
                "SQL should use alias L for query: {}",
                query
            );
            assert!(
                sql_str.contains("\"R\""),
                "SQL should use alias R for query: {}",
                query
            );
        }
    }

    #[test]
    fn test_build_fetch_nest_with_calculation_nvalue_sql() {
        let operators = [
            ("sum(size:) + count(size:)", "+"),
            ("sum(size:) - count(size:)", "-"),
            ("sum(size:) * count(size:)", "*"),
            ("sum(size:) / count(size:)", "/"),
            ("sum(size:) % count(size:)", "%"),
        ];

        for (query_str, expected_op) in operators {
            let query = format!("parentdir: &: ({})", query_str);
            let resolver =
                Resolver::new(&query, &TagRegistry::with_standard()).unwrap();

            let sql =
                build_fetch_nest_sql(&Src::OneView, &resolver, 100, 0).unwrap();
            let sql_str = sql.to_string(PostgresQueryBuilder);

            assert!(
                sql_str.contains("nvalue_agg"),
                "SQL should contain nvalue_agg CTE for query: {}",
                query
            );
            assert!(
                sql_str.contains("JOIN"),
                "SQL should contain JOIN in nvalue_agg for query: {}",
                query
            );
            assert!(
                sql_str.contains(expected_op),
                "SQL should contain '{}' operator for query: {}",
                expected_op,
                query
            );
        }
    }

    #[test]
    fn test_build_nvalue_diverse_arithmetic_sql() {
        let test_cases = [
            ("avg(size:) + sum(size:)", vec!["AVG", "SUM", "+"]),
            ("max(size:) * 2", vec!["MAX", "2", "*"]),
            ("100 / min(size:)", vec!["100", "MIN", "/"]),
            (
                "(sum(size:) + 10) * count(size:)",
                vec!["SUM", "10", "+", "COUNT", "*"],
            ),
        ];

        for (query_body, expected_keywords) in test_cases {
            let query = format!("parentdir: &: ({})", query_body);
            let resolver =
                Resolver::new(&query, &TagRegistry::with_standard()).unwrap();
            let proj_operand = resolver
                .resolved_query
                .get_projection_operand()
                .expect("Should have projection");
            let nvalue = resolver
                .resolved_query
                .get_nvalue_combined()
                .expect("Should have nvalue");

            let sql = build_nvalue_standalone_subquery(
                &Src::OneView,
                proj_operand,
                &nvalue,
                resolver.resolved_query.get_context(),
                false,
                &AggregationContext::new(),
                None,
            );
            let sql_str = sql.to_string(PostgresQueryBuilder);

            for kw in expected_keywords {
                assert!(
                    sql_str.contains(kw),
                    "SQL should contain '{}' for query: {}\nSQL: {}",
                    kw,
                    query,
                    sql_str
                );
            }

            assert!(
                sql_str.contains("JOIN"),
                "SQL should contain JOIN for query: {}",
                query
            );
        }
    }

    #[test]
    fn test_print_sql_for_debugging_nest_bug() {
        let query_str = "(((parentdir: &: count(extension:rs))) / ((parentdir: &: count()))) :> 1";
        let resolver = crate::query::lens_resolver::Resolver::new(
            query_str,
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let optimized =
            crate::query::lens_optimizer::optimize(resolver.resolved_query);
        let sql = PickNode::new(&Src::OneView, &optimized).build_pick();
        println!(
            "Generated FETCH ITEMS SQL: {}",
            sql.to_string(sea_query::PostgresQueryBuilder)
        );

        if optimized.get_projection().is_some() {
            let resolver2 = crate::query::lens_resolver::Resolver::new(
                query_str,
                &TagRegistry::with_standard(),
            )
            .unwrap();
            let label_sql =
                build_fetch_nest_sql(&Src::OneView, &resolver2, 100, 0)
                    .unwrap();
            println!(
                "Generated LABEL GROUPS SQL: {}",
                label_sql.to_string(sea_query::PostgresQueryBuilder)
            );
        }
    }

    // ── build_pick_sql: 多キー Nest ──────────────────────────────────────────

    #[test]
    fn test_build_pick_sql_multi_key_nest_includes_all_keys() {
        let make_tag_ref = |name: &str| ResolvedOperand::TagRef {
            tag_type: TagType::from(name),
            storage: StorageMapping::Basic {
                column: crate::db::Col::LabelStr,
                tag_type: name.to_string(),
            },
            sql_type: crate::db::SqlType::VARCHAR,
        };

        let nest_two_keys = ResolvedNode::Nest {
            keys: vec![make_tag_ref("tagA"), make_tag_ref("tagB")],
            nvalue: None,
            context: None,
        };

        let sql = build_pick(&Src::OneView, &nest_two_keys)
            .to_string(PostgresQueryBuilder);

        assert!(
            sql.contains("'tagA'"),
            "SQL should filter on tagA, got: {}",
            sql
        );
        assert!(
            sql.contains("'tagB'"),
            "SQL should also filter on tagB (multi-key), got: {}",
            sql
        );
    }

    #[test]
    fn test_build_pick_sql_single_key_nest_no_extra_subquery() {
        let nest_one_key = ResolvedNode::Nest {
            keys: vec![ResolvedOperand::TagRef {
                tag_type: TagType::from("tagA"),
                storage: StorageMapping::Basic {
                    column: crate::db::Col::LabelStr,
                    tag_type: "tagA".to_string(),
                },
                sql_type: crate::db::SqlType::VARCHAR,
            }],
            nvalue: None,
            context: None,
        };

        let sql = build_pick(&Src::OneView, &nest_one_key)
            .to_string(PostgresQueryBuilder);

        assert!(
            sql.contains("'tagA'"),
            "SQL should filter on tagA, got: {}",
            sql
        );
        assert!(
            !sql.contains("'tagB'"),
            "Single-key Nest should not reference tagB, got: {}",
            sql
        );
    }

    // ── label_set_op_sql ─────────────────────────────────────────────────────

    fn make_nest_node(tag: &str) -> ResolvedNode {
        ResolvedNode::Nest {
            keys: vec![ResolvedOperand::TagRef {
                tag_type: TagType::from(tag),
                storage: StorageMapping::Basic {
                    column: crate::db::Col::LabelStr,
                    tag_type: tag.to_string(),
                },
                sql_type: crate::db::SqlType::VARCHAR,
            }],
            nvalue: None,
            context: None,
        }
    }

    #[test]
    fn test_build_fetch_label_set_op_sql_intersect_structure() {
        let node = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![make_nest_node("cat"), make_nest_node("flavor")],
        };

        let sql = label_set_op_sql(&Src::OneView, &node, 100, 0)
            .unwrap()
            .to_string(PostgresQueryBuilder);

        assert!(
            sql.contains("labels_0"),
            "should have labels_0 CTE, got: {}",
            sql
        );
        assert!(
            sql.contains("labels_1"),
            "should have labels_1 CTE, got: {}",
            sql
        );
        assert!(
            sql.contains("op_labels"),
            "should have op_labels CTE, got: {}",
            sql
        );
        assert!(
            sql.to_uppercase().contains("INTERSECT"),
            "should contain INTERSECT, got: {}",
            sql
        );
        assert!(
            sql.contains("'cat'"),
            "should reference 'cat', got: {}",
            sql
        );
        assert!(
            sql.contains("'flavor'"),
            "should reference 'flavor', got: {}",
            sql
        );
        assert!(
            sql.contains("struct_pack"),
            "should contain struct_pack, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_fetch_label_set_op_sql_label_from_first_operand() {
        let node = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![make_nest_node("cat"), make_nest_node("flavor")],
        };

        let sql = label_set_op_sql(&Src::OneView, &node, 100, 0)
            .unwrap()
            .to_string(PostgresQueryBuilder);

        let labels_cte_pos = sql.find("labels").expect("labels CTE missing");
        let after_labels = &sql[labels_cte_pos..];
        assert!(
            after_labels.contains("'cat'"),
            "labels CTE should use first operand tag type 'cat', got: {}",
            sql
        );
    }
}
