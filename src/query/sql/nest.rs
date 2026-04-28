use crate::db::{Col, CustomFunc, SqlType, Tbl};
use crate::query::ast::{ArithmeticAggOp, ComparisonOp};
use crate::query::lens_resolver::{
    LabelSetOpKind, NestMatchCondition, NestMatchOp,
    ResolvedAggregationNode, ResolvedNode, ResolvedOperand,
};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{Label, SType, TagType};
use sea_query::{Alias, Condition, Expr, ExprTrait, Func, IntoIden, Query, SelectStatement, SimpleExpr};
use super::{
    apply_arithmetic_agg, apply_arithmetic_op,
    build_agg_calc_eav_expr, build_agg_calc_expr,
    build_agg_operand_eav_expr,
    build_aggregation_context_for_operand,
    build_calculation_eav_expr, build_calculation_expr,
    build_nest_pivot_cte, build_nvalue_standalone_subquery,
    build_nvalue_cte, build_nvalue_cte_nest,
    build_pick,
    label_to_simple_expr, label_to_unit_aware_expr,
    wrap_to_item_ids,
    AggregationContext, BuildPick, NestContext, PickNode,
};

// ── Projection SQL ─────────────────────────────────────────────────────────

pub(super) fn build_resolved_projection_sql(
    op: &ResolvedOperand,
    view: &str,
) -> SelectStatement {
    op.fold(&|op, child_results: Vec<SelectStatement>| match op {
        ResolvedOperand::TagRef { tag_type, .. } => {
            let mut q = Query::select();
            q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
                .distinct()
                .from(Alias::new(view));
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
            let [mut l, r]: [SelectStatement; 2] = child_results.try_into().unwrap();
            l.union(sea_query::UnionType::Intersect, r);
            l
        }
        _ => Query::select()
            .columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .distinct()
            .from(Alias::new(view))
            .to_owned(),
    })
}

pub(super) fn filter(
    keys: &[ResolvedOperand],
    ctx_sql: Option<SelectStatement>,
    view: &str,
) -> SelectStatement {
    let mut stmt = build_resolved_projection_sql(keys.first().unwrap(), view);
    for key in keys.iter().skip(1) {
        let key_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(build_resolved_projection_sql(key, view), Alias::new("_key"))
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(key_sub));
    }
    if let Some(ctx) = ctx_sql {
        let ctx_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(ctx, Alias::new("_ctx"))
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(ctx_sub));
    }
    stmt
}

// ── LabelSetOp helpers ─────────────────────────────────────────────────────

pub(super) fn extract_primary_label_tag_type_from_node(
    node: &ResolvedNode,
) -> Option<(String, Col)> {
    node.walk().into_iter().find_map(|n| match n {
        ResolvedNode::Nest { keys, .. } => match keys.first()? {
            ResolvedOperand::TagRef {
                storage: StorageMapping::RowTag { tag_type, column },
                ..
            } => Some((tag_type.clone(), *column)),
            _ => None,
        },
        _ => None,
    })
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
    keys: &[ResolvedOperand],
    ids_sql: SelectStatement,
    view: &str,
) -> anyhow::Result<SelectStatement> {
    use std::collections::HashSet;
    let mut pivot = Query::select();
    pivot.column(Col::ItemId);
    let mut type_filters: HashSet<String> = HashSet::new();
    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef {
                storage: StorageMapping::RowTag { tag_type, column },
                ..
            } => {
                type_filters.insert(tag_type.as_str().to_string());
                let case_expr =
                    Expr::case(Expr::col(Col::Type).eq(tag_type.as_str()), Expr::col(*column));
                let max_expr = Expr::cust_with_exprs("MAX($1)", [case_expr.into()]);
                pivot.expr_as(max_expr.clone(), Alias::new(&format!("key{}", i)));
                pivot.and_having(max_expr.is_not_null());
            }
            ResolvedOperand::TagRef {
                storage: StorageMapping::Column(col),
                ..
            } => {
                let max_expr = Expr::col(*col).max();
                pivot.expr_as(max_expr.clone(), Alias::new(&format!("key{}", i)));
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
    pivot.from(Alias::new(view));
    if !type_filters.is_empty() {
        pivot.and_where(Expr::col(Col::Type).is_in(type_filters));
    }
    pivot
        .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql))
        .group_by_col(Col::ItemId);

    let composite_str = (0..keys.len())
        .map(|i| format!("CAST(\"key{}\" AS VARCHAR)", i))
        .collect::<Vec<_>>()
        .join(" || ' &: ' || ");
    let mut outer = Query::select();
    outer
        .expr_as(Expr::cust(&composite_str), Alias::new("label_value_cast"))
        .column(Col::ItemId)
        .from_subquery(pivot, Alias::new("pivot_sub"));
    Ok(outer)
}

// ── Nest match SQL ─────────────────────────────────────────────────────────

fn get_required_row_tags(node: &ResolvedNode) -> Vec<String> {
    node.walk()
        .into_iter()
        .filter_map(|n| {
            if let ResolvedNode::Match {
                storage: StorageMapping::RowTag { tag_type, .. },
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

fn resolve_simple_filter_condition(
    node: &ResolvedNode,
    table: Alias,
) -> Option<Condition> {
    node.fold(&|n, child_results: Vec<Option<Condition>>| match n {
        ResolvedNode::Match { storage, label, .. } => {
            if let StorageMapping::RowTag { tag_type, .. } = storage {
                let s_val = label.as_str();
                let mut cond = Condition::all();
                if tag_type.as_str() != "*" {
                    cond = cond
                        .add(Expr::col((table.clone(), Col::Type)).eq(tag_type.as_str()));
                }
                if s_val != "*" && s_val != "" {
                    cond = cond
                        .add(Expr::col((table.clone(), Col::LabelStr)).eq(s_val));
                }
                Some(cond)
            } else {
                None
            }
        }
        ResolvedNode::ColumnMatch { tag, label } => {
            let s_val = label.as_str();
            Some(Condition::all().add(Expr::col((table.clone(), *tag)).eq(s_val)))
        }
        ResolvedNode::And(_) => {
            let mut required_tags = get_required_row_tags(n);
            required_tags.sort();
            required_tags.dedup();
            if required_tags.len() > 1 {
                return None;
            }
            child_results
                .into_iter()
                .try_fold(Condition::all(), |acc, c| c.map(|cond| acc.add(cond)))
        }
        _ => None,
    })
}

fn build_merged_nvalue_agg_expr(
    nvalue: &ResolvedOperand,
    tbl_alias: &str,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    nvalue.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            let (inner_tag, inner_filter, _) = inner.extract_agg_parts();
            if let Some(filter_node) = inner_filter {
                let case_expr: SimpleExpr = if let Some(cond) =
                    resolve_simple_filter_condition(&filter_node, Alias::new(tbl_alias))
                {
                    if cond.is_empty() {
                        Expr::col((Alias::new(tbl_alias), Col::ItemId)).into()
                    } else {
                        Expr::case(cond, Expr::col((Alias::new(tbl_alias), Col::ItemId)))
                            .finally(Expr::val(None::<i32>))
                            .into()
                    }
                } else {
                    let inner_ptr = inner.as_ref() as *const ResolvedNode as usize;
                    let filter_sql = agg_ctx
                        .agg_filters
                        .get(&inner_ptr)
                        .expect("filter SQL must be pre-computed")
                        .clone();
                    let filter_sub = Query::select()
                        .column(Col::ItemId)
                        .from_subquery(filter_sql, Alias::new("nv_filter"))
                        .to_owned();
                    let in_expr = Expr::col((Alias::new(tbl_alias), Col::ItemId))
                        .in_subquery(filter_sub);
                    Expr::case(in_expr, Expr::col((Alias::new(tbl_alias), Col::ItemId)))
                        .finally(Expr::val(None::<i32>))
                        .into()
                };
                Expr::expr(case_expr).count_distinct().into()
            } else if let Some(StorageMapping::RowTag { tag_type, .. }) = inner_tag {
                let cond = Condition::all().add(
                    Expr::col((Alias::new(tbl_alias), Col::Type)).eq(tag_type.as_str()),
                );
                let case_expr = Expr::case(cond, Expr::col((Alias::new(tbl_alias), Col::ItemId)))
                    .finally(Expr::val(None::<i32>));
                Expr::expr(case_expr).count_distinct().into()
            } else {
                Expr::col((Alias::new(tbl_alias), Col::ItemId))
                    .count_distinct()
                    .into()
            }
        }
        ResolvedOperand::Aggregation(agg @ ResolvedAggregationNode::Arithmetic { op, inner }) => {
            let is_string = agg.is_string_type();
            let (inner_tag, inner_filter, _operand) = inner.extract_agg_parts();

            let val_expr: SimpleExpr = if is_string {
                Expr::col((Alias::new(tbl_alias), Col::LabelStr)).into()
            } else {
                Expr::cust_with_exprs(
                    "COALESCE($1, $2, TRY_CAST($3 AS DOUBLE))",
                    [
                        Expr::col((Alias::new(tbl_alias), Col::LabelInt)).into(),
                        Expr::col((Alias::new(tbl_alias), Col::LabelDouble)).into(),
                        Expr::col((Alias::new(tbl_alias), Col::LabelStr)).into(),
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
                    .from_subquery(filter_sql, Alias::new("nv_filter"))
                    .to_owned();
                let in_expr =
                    Expr::col((Alias::new(tbl_alias), Col::ItemId)).in_subquery(filter_sub);
                let case_expr: SimpleExpr =
                    if let Some(StorageMapping::RowTag { tag_type, .. }) = inner_tag {
                        let combined_cond = Condition::all()
                            .add(
                                Expr::col((Alias::new(tbl_alias), Col::Type))
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
            } else if let Some(StorageMapping::RowTag { tag_type, .. }) = inner_tag {
                let cond = Condition::all().add(
                    Expr::col((Alias::new(tbl_alias), Col::Type)).eq(tag_type.as_str()),
                );
                let case_expr = Expr::case(cond, val_expr).finally(Expr::val(None::<f64>));
                apply_arithmetic_agg(op, Expr::expr(case_expr).into(), is_string)
            } else {
                apply_arithmetic_agg(op, Expr::expr(val_expr).into(), is_string)
            }
        }
        ResolvedOperand::Calculation(calc) => {
            let [left_expr, right_expr]: [SimpleExpr; 2] = child_results.try_into().unwrap();
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
    key: &ResolvedOperand,
    conditions: &[(&ResolvedOperand, ComparisonOp, &ResolvedOperand)],
    is_or: bool,
    view: &str,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let (proj_col, proj_tag_type) = match key.get_storage() {
        Some(StorageMapping::RowTag { column, tag_type }) => (*column, Some(tag_type.as_str())),
        Some(StorageMapping::Column(col)) => (*col, None),
        _ => panic!("NestMatch key must have RowTag or Column storage"),
    };

    let mut nfilter = Query::select();
    nfilter.expr_as(
        Expr::col((Alias::new("proj"), proj_col)),
        Alias::new("group_label"),
    );
    nfilter.from_as(Alias::new(view), Alias::new("proj"));
    nfilter.join_as(
        sea_query::JoinType::InnerJoin,
        Alias::new(view),
        Alias::new("c"),
        Expr::col((Alias::new("proj"), Col::ItemId))
            .equals((Alias::new("c"), Col::ItemId)),
    );
    if let Some(tag_type) = proj_tag_type {
        nfilter.and_where(Expr::col((Alias::new("proj"), Col::Type)).eq(tag_type));
    }
    nfilter.group_by_col((Alias::new("proj"), proj_col));

    let mut having_cond = if is_or {
        Condition::any()
    } else {
        Condition::all()
    };
    for (nvalue, cmp_op, right) in conditions {
        let bin_op = to_bin_op(*cmp_op);
        let lhs = build_merged_nvalue_agg_expr(nvalue, "c", agg_ctx);
        let rhs = build_merged_nvalue_agg_expr(right, "c", agg_ctx);
        having_cond = having_cond.add(Expr::expr(lhs).binary(bin_op, rhs));
    }
    nfilter.cond_having(having_cond);

    let group_label_sub = Query::select()
        .column(Alias::new("group_label"))
        .from_subquery(nfilter, Alias::new("nfilter"))
        .to_owned();

    let mut stmt = Query::select();
    stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
    stmt.distinct();
    stmt.from(Alias::new(view));
    if let Some(tag_type) = proj_tag_type {
        stmt.and_where(Expr::col(Col::Type).eq(tag_type));
    }
    stmt.and_where(Expr::col(proj_col).in_subquery(group_label_sub));
    stmt
}

pub(super) fn build_nest_match_sql(
    keys: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    comparison_op: ComparisonOp,
    label: &Label,
    context: &Option<Box<ResolvedNode>>,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    if keys.len() == 1 {
        let nvalue_sub = build_nvalue_standalone_subquery(
            &keys[0],
            nvalue,
            context.as_deref(),
            view,
            false,
            agg_ctx,
            Some(nest_ctx),
        );
        let bin_op = to_bin_op(comparison_op);
        let label_expr = label_to_unit_aware_expr(label);

        let mut nfilter = Query::select();
        nfilter.column(Alias::new("group_label"));
        nfilter.from_subquery(nvalue_sub, Alias::new("nfilter"));
        nfilter.and_where(Expr::col(Alias::new("nvalue")).binary(bin_op, label_expr));

        let (proj_col, proj_tag_type) = match keys[0].get_storage() {
            Some(StorageMapping::RowTag { column, tag_type }) => (*column, Some(tag_type.as_str())),
            Some(StorageMapping::Column(col)) => (*col, None),
            _ => panic!("NestMatch key must have RowTag or Column storage"),
        };
        let mut stmt = Query::select();
        stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        stmt.distinct();
        stmt.from(Alias::new(view));
        if let Some(tag_type) = proj_tag_type {
            stmt.and_where(Expr::col(Col::Type).eq(tag_type));
        }
        stmt.and_where(Expr::col(proj_col).in_subquery(nfilter));
        stmt
    } else {
        let pivot_sub = build_nest_pivot_cte(keys, Some(nvalue), view, agg_ctx);
        let pivot_alias = Alias::new("pivot_match");

        let mut stmt = Query::select();
        stmt.column(Col::ItemId);
        stmt.column(Alias::new("rank"));
        stmt.column(Alias::new("item_kind"));

        let partition_keys: Vec<SimpleExpr> = (0..keys.len())
            .map(|i| Expr::col(Alias::new(&format!("key{}", i))).into())
            .collect();

        let bin_op = to_bin_op(comparison_op);
        let label_expr = label_to_unit_aware_expr(label);

        let agg_func = match nvalue {
            ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_)) => {
                Func::sum(Expr::col(Alias::new("nvalue")))
            }
            ResolvedOperand::Aggregation(ResolvedAggregationNode::Arithmetic { op, .. }) => {
                match op {
                    ArithmeticAggOp::Sum => Func::sum(Expr::col(Alias::new("nvalue"))),
                    ArithmeticAggOp::Avg => Func::avg(Expr::col(Alias::new("nvalue"))),
                    ArithmeticAggOp::Max => Func::max(Expr::col(Alias::new("nvalue"))),
                    ArithmeticAggOp::Min => Func::min(Expr::col(Alias::new("nvalue"))),
                }
            }
            _ => Func::cust(Alias::new("MAX")).arg(Expr::col(Alias::new("nvalue"))),
        };

        use sea_query::{OverStatement, SelectExpr, WindowSelectType, WindowStatement};
        let mut window = WindowStatement::new();
        for pk in &partition_keys {
            window.add_partition_by(pk.clone());
        }
        stmt.expr(SelectExpr {
            expr: agg_func.into(),
            alias: Some(Alias::new("group_nvalue").into_iden()),
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
            psub.column(Col::ItemId)
                .from_subquery(ctx_sql, Alias::new("ctx_p"));
            stmt.and_where(Expr::col(Col::ItemId).in_subquery(psub));
        }
        stmt.from_subquery(pivot_sub, pivot_alias);

        let mut final_stmt = Query::select();
        final_stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        final_stmt.distinct();
        final_stmt.from_subquery(stmt, Alias::new("filtered_items"));
        final_stmt.and_where(
            Expr::col(Alias::new("group_nvalue")).binary(bin_op, label_expr),
        );
        final_stmt
    }
}

pub(super) fn build_nest_nest_match_sql(
    left_keys: &[ResolvedOperand],
    left_nvalue: &ResolvedOperand,
    left_context: &Option<Box<ResolvedNode>>,
    op: &NestMatchOp,
    right_keys: &[ResolvedOperand],
    right_nvalue: &ResolvedOperand,
    right_context: &Option<Box<ResolvedNode>>,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    match op {
        NestMatchOp::Comparison(cmp_op) => {
            let is_agg_or_calc = matches!(
                right_nvalue,
                ResolvedOperand::Aggregation(_) | ResolvedOperand::Calculation(_)
            );
            if is_agg_or_calc {
                let conditions = [(*cmp_op, right_nvalue)];
                let conds: Vec<_> = conditions
                    .iter()
                    .map(|(op, rhs)| (left_nvalue, *op, *rhs))
                    .collect();
                return build_nest_having_sql(&left_keys[0], &conds, false, view, agg_ctx);
            }

            let mut stmt = build_resolved_projection_sql(&left_keys[0], view);
            let sub_l = build_nvalue_standalone_subquery(
                &left_keys[0],
                left_nvalue,
                left_context.as_deref(),
                view,
                true,
                agg_ctx,
                Some(nest_ctx),
            );
            let sub_r = build_nvalue_standalone_subquery(
                &right_keys[0],
                right_nvalue,
                right_context.as_deref(),
                view,
                true,
                agg_ctx,
                Some(nest_ctx),
            );
            let bin_op = to_bin_op(*cmp_op);
            let join_sql = Query::select()
                .column((Alias::new("L"), Alias::new("group_label")))
                .from_subquery(sub_l, Alias::new("L"))
                .join_subquery(
                    sea_query::JoinType::InnerJoin,
                    sub_r,
                    Alias::new("R"),
                    Expr::col((Alias::new("L"), Col::ItemId))
                        .eq(Expr::col((Alias::new("R"), Col::ItemId))),
                )
                .and_where(
                    Expr::col((Alias::new("L"), Alias::new("nvalue"))).binary(
                        bin_op,
                        Expr::col((Alias::new("R"), Alias::new("nvalue"))),
                    ),
                )
                .to_owned();
            let proj_col = match left_keys[0].get_storage() {
                Some(StorageMapping::RowTag { column, .. }) => column,
                Some(StorageMapping::Column(col)) => col,
                _ => panic!(
                    "unexpected NestNestMatch with non-TagRef keys: {:?}",
                    left_keys
                ),
            };
            stmt.and_where(Expr::col(*proj_col).in_subquery(join_sql));
            stmt
        }
    }
}

pub(super) fn build_merged_nest_match_sql(
    keys: &[ResolvedOperand],
    matches: &[NestMatchCondition],
    is_or: bool,
    view: &str,
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
        build_nest_having_sql(&keys[0], &conditions, is_or, view, agg_ctx)
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

        let pivot_sub = build_nest_pivot_multi_nv_cte(keys, &all_nv_ops, view, agg_ctx);
        let pivot_alias = Alias::new("pivot_merged");

        let mut stmt = Query::select();
        stmt.column(Col::ItemId);
        stmt.column(Alias::new("rank"));
        stmt.column(Alias::new("item_kind"));

        let mut partition_keys: Vec<SimpleExpr> = Vec::new();
        for i in 0..keys.len() {
            partition_keys.push(Expr::col(Alias::new(&format!("key{}", i))).into());
        }

        use sea_query::{OverStatement, SelectExpr, WindowSelectType, WindowStatement};

        let mut group_nv_aliases = Vec::new();
        for (idx, op) in all_nv_ops.iter().enumerate() {
            let nval_pivot_alias = format!("nv{}", idx);
            let group_nv_alias = format!("group_nv_{}", idx);

            let agg_func = match op {
                ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_)) => {
                    Func::sum(Expr::col(Alias::new(&nval_pivot_alias)))
                }
                ResolvedOperand::Aggregation(ResolvedAggregationNode::Arithmetic { op, .. }) => {
                    match op {
                        ArithmeticAggOp::Sum => Func::sum(Expr::col(Alias::new(&nval_pivot_alias))),
                        ArithmeticAggOp::Avg => Func::avg(Expr::col(Alias::new(&nval_pivot_alias))),
                        ArithmeticAggOp::Max => Func::max(Expr::col(Alias::new(&nval_pivot_alias))),
                        ArithmeticAggOp::Min => Func::min(Expr::col(Alias::new(&nval_pivot_alias))),
                    }
                }
                _ => Func::cust(Alias::new("MAX")).arg(Expr::col(Alias::new(&nval_pivot_alias))),
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
            let left_idx = all_nv_ops.iter().position(|&o| o == &m.nvalue).unwrap();
            let left_group_nv = &group_nv_aliases[left_idx];

            let NestMatchOp::Comparison(cmp_op) = m.op;
            let bin_op = to_bin_op(cmp_op);

            let right_expr = if let ResolvedOperand::Aggregation(_) = &m.right {
                let right_idx = all_nv_ops.iter().position(|&o| o == &m.right).unwrap();
                Expr::col(Alias::new(&group_nv_aliases[right_idx])).into()
            } else {
                build_agg_operand_eav_expr(&m.right, agg_ctx)
            };

            filter_cond =
                filter_cond.add(Expr::col(Alias::new(left_group_nv)).binary(bin_op, right_expr));
        }

        stmt.from_subquery(pivot_sub, pivot_alias);

        let mut final_stmt = Query::select();
        final_stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        final_stmt.distinct();
        final_stmt.from_subquery(stmt, Alias::new("merged_items"));
        final_stmt.and_where(filter_cond.into());
        final_stmt
    }
}

fn build_nest_pivot_multi_nv_cte(
    keys: &[ResolvedOperand],
    nvalues: &[&ResolvedOperand],
    view: &str,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.column(Col::ItemId);
    stmt.expr_as(
        crate::db::CustomFunc::any_value(Expr::col(Col::Rank)),
        Alias::new("rank"),
    );
    stmt.expr_as(
        crate::db::CustomFunc::any_value(Expr::col(Col::ItemKind)),
        Alias::new("item_kind"),
    );
    stmt.from(Alias::new(view));

    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef { storage, .. } => match storage {
                StorageMapping::RowTag { tag_type, column } => {
                    let case_expr = Expr::case(
                        Expr::col(Col::Type).eq(tag_type.as_str()),
                        Expr::col(*column),
                    );
                    stmt.expr_as(
                        Expr::cust_with_exprs("MAX($1)", [case_expr.into()]),
                        Alias::new(&format!("key{}", i)),
                    );
                }
                StorageMapping::Column(col) => {
                    stmt.expr_as(Expr::col(*col).max(), Alias::new(&format!("key{}", i)));
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
    resolver: &crate::query::lens_resolver::Resolver,
    view: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    if let Some(node) = resolver.get_label_set_op_node() {
        label_set_op_sql(node, view, limit, offset)
    } else {
        let pick = PickNode::new(&resolver.resolved_query, view);
        nest(&pick, resolver, limit, offset)
    }
}

fn make_tag_struct_pack(
    type_str: &str,
    sql_type: SqlType,
    value_expr: impl Into<SimpleExpr>,
) -> SimpleExpr {
    CustomFunc::struct_pack_tag(
        Expr::val(type_str).into(),
        CustomFunc::union_value(sql_type, value_expr),
        Expr::val("system").into(),
    )
}

pub(super) fn nest(
    pick: &PickNode<'_>,
    resolver: &crate::query::lens_resolver::Resolver,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    use crate::db::CustomFunc;
    use sea_query::{CommonTableExpression, Iden, WithClause};

    let view = pick.view();
    let pick_sql = pick.build_pick();

    let proj_type = resolver
        .resolved_query
        .get_projection()
        .ok_or_else(|| anyhow::anyhow!("nest: no projection type in resolved query"))?;
    let desc = resolver.lens().look_up_or_default(&proj_type);
    let col_iden = match &desc.storage {
        StorageMapping::Column(col) => *col,
        StorageMapping::RowTag { column, .. } => *column,
        _ => anyhow::bail!(
            "Unsupported storage for projection: {:?}",
            desc.storage
        ),
    };

    let mut with_clause = WithClause::new();

    let picked_ids_cte = CommonTableExpression::new()
        .query(wrap_to_item_ids(pick_sql))
        .table_name(Tbl::PickedIds)
        .to_owned();
    with_clause.cte(picked_ids_cte);

    let is_or_query = matches!(&resolver.resolved_query, ResolvedNode::Or(_));
    let nvalue_condition = resolver.get_nvalue_condition();
    let has_nvalue = if !is_or_query {
        if let Some(nv) = resolver.get_nvalue() {
            let proj_operands = resolver.resolved_query.get_projection_operands().unwrap();
            let context = resolver.resolved_query.get_context();
            let computed_agg_ctx;
            let mut nvalue_sql = if let (Some(agg_ctx), Some(nest_ctx)) = (pick.agg_ctx(), pick.nest_ctx()) {
                build_nvalue_cte_nest(proj_operands, nv, context, view, agg_ctx, nest_ctx)
            } else {
                computed_agg_ctx = build_aggregation_context_for_operand(nv, view);
                build_nvalue_cte(proj_operands, nv, context, view, &computed_agg_ctx)
            };
            if let Some((op, value)) = nvalue_condition {
                let bin_op = to_bin_op(*op);
                let val = label_to_simple_expr(value);
                let cond = Expr::col(Alias::new("nvalue")).binary(bin_op, val);
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

    let (label_col_name, all_hits_source, need_extra_filter) =
        if proj_operands.len() > 1 {
            let agg_ctx = pick.agg_ctx().expect("AggregationContext required for pivot CTE");
            let pivot_q = build_nest_pivot_cte(proj_operands, None, view, agg_ctx);
            let pivot_cte = CommonTableExpression::new()
                .query(pivot_q)
                .table_name(Alias::new("pivot"))
                .to_owned();
            with_clause.cte(pivot_cte);
            ("key0".to_string(), "pivot".to_string(), false)
        } else if let Some(calc) = calc_node {
            if calc.contains_row_tag() {
                let calc_expr = if calc.contains_aggregation() {
                    let agg_ctx = pick.agg_ctx().expect("AggregationContext required for EAV+agg calculation CTE");
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
                        Alias::new("rank"),
                    )
                    .from(Alias::new(view))
                    .and_where(
                        Expr::col(Col::ItemId).in_subquery(
                            Query::select()
                                .column(Col::ItemId)
                                .from(Tbl::PickedIds)
                                .to_owned(),
                        ),
                    )
                    .group_by_col(Col::ItemId);
                let computed_cte = CommonTableExpression::new()
                    .query(computed_q)
                    .table_name(Alias::new("computed"))
                    .to_owned();
                with_clause.cte(computed_cte);
                ("calc_value".to_string(), "computed".to_string(), false)
            } else {
                let calc_expr = if calc.contains_aggregation() {
                    let agg_ctx = pick.agg_ctx().expect("AggregationContext required for calculation CTE");
                    build_agg_calc_expr(&calc, agg_ctx)
                } else {
                    build_calculation_expr(&calc)
                };
                let mut computed_q = Query::select();
                computed_q
                    .column(Col::ItemId)
                    .expr_as(calc_expr, Alias::new("calc_value"))
                    .column(Col::Rank)
                    .from(Alias::new(view))
                    .and_where(
                        Expr::col(Col::ItemId).in_subquery(
                            Query::select()
                                .column(Col::ItemId)
                                .from(Tbl::PickedIds)
                                .to_owned(),
                        ),
                    );
                let computed_cte = CommonTableExpression::new()
                    .query(computed_q)
                    .table_name(Alias::new("computed"))
                    .to_owned();
                with_clause.cte(computed_cte);
                ("calc_value".to_string(), "computed".to_string(), false)
            }
        } else {
            (Iden::to_string(&col_iden), view.to_string(), true)
        };

    let label_col = Alias::new(&label_col_name);

    let partition_sql = if proj_operands.len() > 1 {
        (0..proj_operands.len())
            .map(|i| format!("\"key{}\"", i))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!("\"{}\"", label_col_name)
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
            Expr::cust(format!(
                "row_number() OVER (PARTITION BY {} ORDER BY \"rank\" DESC, \"item_id\" DESC)",
                partition_sql
            )),
            Tbl::Rn,
        )
        .expr_as(
            Expr::cust(format!("count(*) OVER (PARTITION BY {})", partition_sql)),
            Tbl::GroupTotal,
        )
        .distinct()
        .from(Alias::new(&all_hits_source))
        .and_where(
            Expr::col(Col::ItemId).in_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .from(Tbl::PickedIds)
                    .to_owned(),
            ),
        );

    if need_extra_filter {
        all_hits_q.and_where(Expr::col(label_col.clone()).is_not_null());
        if let StorageMapping::RowTag { tag_type, .. } = &desc.storage {
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
                Expr::col(label_col.clone()).in_subquery(
                    Query::select()
                        .column(Alias::new("group_label"))
                        .from(Alias::new("nvalue_agg"))
                        .to_owned(),
                ),
            );
        }
    }

    let all_hits_cte = CommonTableExpression::new()
        .query(all_hits_q)
        .table_name(Tbl::AllHits)
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
        .column(Tbl::GroupTotal)
        .from(Tbl::AllHits)
        .and_where(Expr::col(Tbl::Rn).lte(100));

    let top_items_cte = CommonTableExpression::new()
        .query(top_items_q)
        .table_name(Tbl::TopItems)
        .to_owned();
    with_clause.cte(top_items_cte);

    let label_ref = if proj_operands.len() > 1 {
        (0..proj_operands.len())
            .map(|i| {
                format!(
                    "CAST({}.key{} AS VARCHAR)",
                    Iden::to_string(&Tbl::TopItems),
                    i
                )
            })
            .collect::<Vec<_>>()
            .join(" || ' &: ' || ")
    } else {
        format!(
            "CAST({}.{} AS VARCHAR)",
            Iden::to_string(&Tbl::TopItems),
            label_col_name
        )
    };

    let partition_order = if proj_operands.len() > 1 {
        (0..proj_operands.len())
            .map(|i| format!("{}.key{}", Iden::to_string(&Tbl::TopItems), i))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!("{}.{}", Iden::to_string(&Tbl::TopItems), label_col_name)
    };
    let volatile_id_expr = format!(
        "row_number() OVER (ORDER BY {})",
        partition_order
    );

    let name_sp = make_tag_struct_pack(
        "name",
        SqlType::VARCHAR,
        Expr::cust(format!("({})", label_ref)),
    );

    let name_subquery = format!(
        "(SELECT {} FROM {} WHERE {} = {}.{} AND {} = 'name' LIMIT 1)",
        Iden::to_string(&Col::LabelStr),
        view,
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Col::Type)
    );
    let item_label_str = format!(
        "CONCAT(COALESCE({}, 'unknown'), '#', CAST({}.{} AS VARCHAR))",
        name_subquery,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId)
    );
    let item_sp = make_tag_struct_pack(
        "item",
        SqlType::VARCHAR,
        Expr::cust(item_label_str),
    );
    let item_list_expr = Expr::cust_with_exprs(
        &format!(
            "list($1 ORDER BY {}.{} DESC, {}.{} DESC)",
            Iden::to_string(&Tbl::TopItems),
            Iden::to_string(&Col::Rank),
            Iden::to_string(&Tbl::TopItems),
            Iden::to_string(&Col::ItemId),
        ),
        [item_sp],
    );

    let proj_label_sp = make_tag_struct_pack(
        "projected_label",
        SqlType::BIGINT,
        Expr::cust(format!(
            "ANY_VALUE({}.{})::BIGINT",
            Iden::to_string(&Tbl::TopItems),
            Iden::to_string(&Tbl::GroupTotal),
        )),
    );

    let mut tags_expr: SimpleExpr = Expr::cust_with_exprs(
        "list_value($1) || $2 || list_value($3)",
        [name_sp, item_list_expr, proj_label_sp],
    );

    if has_nvalue {
        let nvalue_subq = if proj_operands.len() > 1 {
            let mut join_cond = "TRUE".to_string();
            for i in 0..proj_operands.len() {
                join_cond.push_str(&format!(
                    " AND \"nvalue_agg\".\"key{}\" = {}.\"key{}\"",
                    i,
                    Iden::to_string(&Tbl::TopItems),
                    i
                ));
            }
            format!("(SELECT \"nvalue\" FROM \"nvalue_agg\" WHERE {})", join_cond)
        } else {
            format!(
                "(SELECT \"nvalue\" FROM \"nvalue_agg\" WHERE \"group_label\" = {}.{})",
                Iden::to_string(&Tbl::TopItems),
                &label_col_name,
            )
        };
        let nvalue_sp = make_tag_struct_pack(
            "nvalue",
            SqlType::DOUBLE,
            Expr::cust(format!("CAST(({}) AS DOUBLE)", nvalue_subq)),
        );
        tags_expr =
            Expr::cust_with_exprs("$1 || list_value($2)", [tags_expr, nvalue_sp]);
    }

    let mut q = Query::select();
    q.with_cte(with_clause);
    q.expr_as(Expr::cust(volatile_id_expr), Col::ItemId);
    q.expr_as(
        Expr::cust(format!(
            "ANY_VALUE({}.{})",
            Iden::to_string(&Tbl::TopItems),
            Iden::to_string(&Col::Rank)
        )),
        Col::Rank,
    );
    q.expr_as(Expr::cust("'volatile'"), Col::ItemKind);
    q.expr_as(tags_expr, crate::db::QueryResultCol::Tags);
    q.from(Tbl::TopItems);

    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            q.group_by_col((Tbl::TopItems, Alias::new(&format!("key{}", i))));
        }
        for i in (0..proj_operands.len()).rev() {
            q.order_by(
                (Tbl::TopItems, Alias::new(&format!("key{}", i))),
                sea_query::Order::Asc,
            );
        }
    } else {
        q.group_by_col((Tbl::TopItems, label_col.clone()));
        q.order_by((Tbl::TopItems, label_col), sea_query::Order::Asc);
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
    label_set_op: &ResolvedNode,
    view: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    use sea_query::{CommonTableExpression, Iden, WithClause};

    let (op, operands) = match label_set_op {
        ResolvedNode::LabelSetOp { op, operands } => (op, operands),
        _ => anyhow::bail!(
            "label_set_op_sql: expected LabelSetOp node"
        ),
    };
    if operands.is_empty() {
        anyhow::bail!(
            "label_set_op_sql: LabelSetOp with no operands"
        );
    }

    let mut with_clause = WithClause::new();

    let cte_names: Vec<String> = (0..operands.len())
        .map(|i| format!("labels_{}", i))
        .collect();
    for (i, operand) in operands.iter().enumerate() {
        let ids_sql = wrap_to_item_ids(build_pick(operand, view));

        let labels_sql = if matches!(op, LabelSetOpKind::Union) {
            if let Some(keys) = extract_multi_key_nest_operands(operand) {
                build_multi_key_labels_sql(&keys, ids_sql, view)?
            } else {
                let (label_tag_type, label_col) =
                    extract_primary_label_tag_type_from_node(operand).ok_or_else(|| {
                        anyhow::anyhow!(
                            "label_set_op_sql: cannot determine label type from operand {}", i
                        )
                    })?;
                let cast_expr = Expr::cust_with_exprs(
                    "CAST($1 AS VARCHAR)",
                    vec![Expr::col(label_col).into()],
                );
                let mut s = Query::select();
                s.expr_as(cast_expr, Alias::new("label_value_cast"))
                    .column(Col::ItemId)
                    .from(Alias::new(view))
                    .and_where(Expr::col(Col::Type).eq(label_tag_type.as_str()))
                    .and_where(Expr::col(label_col).is_not_null())
                    .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
                s
            }
        } else {
            let (label_tag_type, label_col) =
                extract_primary_label_tag_type_from_node(operand).ok_or_else(|| {
                    anyhow::anyhow!(
                        "label_set_op_sql: cannot determine label type from operand {}", i
                    )
                })?;
            let cast_expr = Expr::cust_with_exprs(
                "CAST($1 AS VARCHAR)",
                vec![Expr::col(label_col).into()],
            );
            let mut s = Query::select();
            s.expr_as(cast_expr, Alias::new("label_value_cast"))
                .column(Col::ItemId)
                .from(Alias::new(view))
                .and_where(Expr::col(Col::Type).eq(label_tag_type.as_str()))
                .and_where(Expr::col(label_col).is_not_null())
                .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
            s
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
        let right_ids_sql = wrap_to_item_ids(build_pick(&operands[1], view));

        let mut labels_sql = Query::select();
        labels_sql
            .expr_as(
                Expr::col(Alias::new("label_value_cast")),
                Alias::new("label_value"),
            )
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
            .column(Alias::new("label_value_cast"))
            .from(Alias::new(&cte_names[0]))
            .to_owned();
        for name in &cte_names[1..] {
            let other = Query::select()
                .column(Alias::new("label_value_cast"))
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
            .column(Alias::new("label_value_cast"))
            .column(Col::ItemId)
            .from(Alias::new(&cte_names[0]))
            .to_owned();
        for name in &cte_names[1..] {
            let other = Query::select()
                .column(Alias::new("label_value_cast"))
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
            .expr_as(
                Expr::col(Alias::new("label_value_cast")),
                Alias::new("label_value"),
            )
            .column(Col::ItemId)
            .from(Alias::new("all_op_items"))
            .and_where(
                Expr::col(Alias::new("label_value_cast")).in_subquery(
                    Query::select()
                        .column(Alias::new("label_value_cast"))
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
        .column(Alias::new("label_value"))
        .expr_as(
            Expr::cust(
                "row_number() OVER (PARTITION BY \"label_value\" ORDER BY \"item_id\" DESC)",
            ),
            Tbl::Rn,
        )
        .expr_as(
            Expr::cust("count(*) OVER (PARTITION BY \"label_value\")"),
            Tbl::GroupTotal,
        )
        .from(Alias::new("labels"))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(all_hits_sql)
            .table_name(Tbl::AllHits)
            .to_owned(),
    );

    let top_items_sql = Query::select()
        .column(Col::ItemId)
        .column(Alias::new("label_value"))
        .column(Tbl::GroupTotal)
        .from(Tbl::AllHits)
        .and_where(Expr::col(Tbl::Rn).lte(100))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(top_items_sql)
            .table_name(Tbl::TopItems)
            .to_owned(),
    );

    let label_ref = format!(
        "CAST({}.label_value AS VARCHAR)",
        Iden::to_string(&Tbl::TopItems)
    );
    let volatile_id_expr = format!(
        "row_number() OVER (ORDER BY {}.label_value)",
        Iden::to_string(&Tbl::TopItems)
    );

    let name_sp = make_tag_struct_pack(
        "name",
        SqlType::VARCHAR,
        Expr::cust(format!("({})", label_ref)),
    );

    let name_subquery = format!(
        "(SELECT {} FROM {} WHERE {} = {}.{} AND {} = 'name' LIMIT 1)",
        Iden::to_string(&Col::LabelStr),
        view,
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Col::Type),
    );
    let item_label_str = format!(
        "CONCAT(COALESCE({}, 'unknown'), '#', CAST({}.{} AS VARCHAR))",
        name_subquery,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
    );
    let item_sp = make_tag_struct_pack(
        "item",
        SqlType::VARCHAR,
        Expr::cust(item_label_str),
    );
    let item_list_expr = Expr::cust_with_exprs(
        &format!(
            "list($1 ORDER BY {}.{} DESC)",
            Iden::to_string(&Tbl::TopItems),
            Iden::to_string(&Col::ItemId),
        ),
        [item_sp],
    );

    let proj_label_sp = make_tag_struct_pack(
        "projected_label",
        SqlType::BIGINT,
        Expr::cust(format!(
            "ANY_VALUE({}.{})::BIGINT",
            Iden::to_string(&Tbl::TopItems),
            Iden::to_string(&Tbl::GroupTotal),
        )),
    );

    let tags_expr: SimpleExpr = Expr::cust_with_exprs(
        "list_value($1) || $2 || list_value($3)",
        [name_sp, item_list_expr, proj_label_sp],
    );

    let mut q = Query::select();
    q.with_cte(with_clause);
    q.expr_as(Expr::cust(volatile_id_expr), Col::ItemId);
    q.expr_as(Expr::cust("0::BIGINT"), Col::Rank);
    q.expr_as(Expr::cust("'volatile'"), Col::ItemKind);
    q.expr_as(tags_expr, crate::db::QueryResultCol::Tags);
    q.from(Tbl::TopItems)
        .group_by_col((Tbl::TopItems, Alias::new("label_value")))
        .order_by(
            (Tbl::TopItems, Alias::new("label_value")),
            sea_query::Order::Asc,
        );

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
    use sea_query::PostgresQueryBuilder;

    #[test]
    fn test_build_fetch_nest_sql_generates_concat() {
        let query_str = "extension:";
        let resolver = Resolver::new(query_str).expect("Failed to resolve");

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0)
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
            sql_str.contains("projected_label"),
            "SQL should contain projected_label tag: {}",
            sql_str
        );
    }

    #[test]
    fn test_nvalue_count_projection_sql() {
        let resolver =
            Resolver::new("parentdir: &: count(extension:jpg)").unwrap();

        assert!(
            resolver.get_nvalue().is_some(),
            "Should have nvalue for nest query"
        );

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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
        let resolver = Resolver::new("parentdir: &: sum(size:)").unwrap();
        assert!(
            resolver.get_nvalue().is_some(),
            "Should have nvalue for nest query"
        );

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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
        let resolver = Resolver::new("extension:").unwrap();

        assert!(
            resolver.get_nvalue().is_none(),
            "Normal projection should NOT have nvalue"
        );

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(
            !sql_str.contains("nvalue_agg"),
            "Normal projection should NOT contain nvalue_agg: {}",
            sql_str
        );
    }

    #[test]
    fn test_nvalue_condition_having_sql() {
        let resolver =
            Resolver::new("parentdir: &: (count(extension:jpg) > 1)").unwrap();

        assert!(resolver.get_nvalue_condition().is_some());

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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
            let resolver = Resolver::new(&query).unwrap();
            let proj_operand = resolver
                .resolved_query
                .get_projection_operand()
                .expect("Should have projection");
            let nvalue = resolver
                .resolved_query
                .get_nvalue_combined()
                .expect("Should have nvalue");

            let sql = build_nvalue_standalone_subquery(
                proj_operand,
                &nvalue,
                resolver.resolved_query.get_context(),
                "oneview",
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
            let resolver = Resolver::new(&query).unwrap();

            let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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
            let resolver = Resolver::new(&query).unwrap();
            let proj_operand = resolver
                .resolved_query
                .get_projection_operand()
                .expect("Should have projection");
            let nvalue = resolver
                .resolved_query
                .get_nvalue_combined()
                .expect("Should have nvalue");

            let sql = build_nvalue_standalone_subquery(
                proj_operand,
                &nvalue,
                resolver.resolved_query.get_context(),
                "oneview",
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
        let resolver =
            crate::query::lens_resolver::Resolver::new(query_str).unwrap();
        let optimized =
            crate::query::lens_optimizer::optimize(resolver.resolved_query);
        let sql = PickNode::new(&optimized, "oneview").build_pick();
        println!(
            "Generated FETCH ITEMS SQL: {}",
            sql.to_string(sea_query::PostgresQueryBuilder)
        );

        if optimized.get_projection().is_some() {
            let resolver2 =
                crate::query::lens_resolver::Resolver::new(query_str).unwrap();
            let label_sql = build_fetch_nest_sql(&resolver2, "oneview", 100, 0).unwrap();
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
            storage: StorageMapping::RowTag {
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

        let sql = build_pick(&nest_two_keys, "oneview")
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
                storage: StorageMapping::RowTag {
                    column: crate::db::Col::LabelStr,
                    tag_type: "tagA".to_string(),
                },
                sql_type: crate::db::SqlType::VARCHAR,
            }],
            nvalue: None,
            context: None,
        };

        let sql = build_pick(&nest_one_key, "oneview")
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
                storage: StorageMapping::RowTag {
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

        let sql = label_set_op_sql(&node, "oneview", 100, 0)
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

        let sql = label_set_op_sql(&node, "oneview", 100, 0)
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
