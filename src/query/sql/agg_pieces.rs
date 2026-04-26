use crate::db::{Col, CustomFunc, Tbl};
use crate::query::ast::ArithmeticAggOp;
use crate::query::lens_resolver::{ResolvedAggregationNode, ResolvedNode, ResolvedOperand};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use sea_query::{Alias, Condition, Expr, ExprTrait, Func, Query, SelectStatement, SimpleExpr};
use super::{
    subquery, wrap_to_item_ids,
    label_to_simple_expr,
    build_resolved_literal_expr, build_storage_column_expr, build_tag_value_agg_expr,
    apply_arithmetic_op, apply_arithmetic_agg,
    fold_simple_operand,
    AggregationContext, NestContext,
};

// ── 低レベルユーティリティ ──────────────────────────────────────────────────

/// Count の引数ノードから、カウント対象のカラムと内部タグタイプを決定する。
pub(super) fn resolve_count_target(inner: &ResolvedNode) -> (Col, Option<String>) {
    inner
        .get_nested_projection()
        .and_then(|op| match op.get_storage() {
            Some(StorageMapping::RowTag { column, tag_type, .. }) => {
                Some((*column, Some(tag_type.clone())))
            }
            Some(StorageMapping::Column(col)) => Some((*col, None)),
            _ => None,
        })
        .unwrap_or((Col::ItemId, None))
}

/// nvalue サブクエリ結果に item_id をアタッチしてラップします。
pub(super) fn wrap_with_item_id(
    agg_sub: SelectStatement,
    proj_col: Col,
    proj_tag_type: Option<&str>,
    view: &str,
) -> SelectStatement {
    let mut wrapped = Query::select();
    wrapped
        .column((Alias::new("view"), Col::ItemId))
        .expr_as(
            Expr::col((Alias::new("view"), proj_col)),
            Alias::new("group_label"),
        )
        .column((Alias::new("agg"), Alias::new("nvalue")))
        .from_as(Alias::new(view), Alias::new("view"))
        .join_subquery(
            sea_query::JoinType::InnerJoin,
            agg_sub,
            Alias::new("agg"),
            Expr::col((Alias::new("view"), proj_col))
                .eq(Expr::col((Alias::new("agg"), Alias::new("group_label")))),
        );
    if let Some(tag_type) = proj_tag_type {
        wrapped.and_where(Expr::col((Alias::new("view"), Col::Type)).eq(tag_type));
    }
    wrapped
}

/// 集約関数をSQL式に変換します（算術演算内で使用）。
pub(super) fn agg_expr(
    agg: &ResolvedAggregationNode,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    match agg {
        ResolvedAggregationNode::Count(inner) => {
            let (storage, cond, _) = inner.extract_agg_parts();
            let col = if let Some(s) = storage {
                match s {
                    StorageMapping::Column(c) => Col::from(*c),
                    StorageMapping::RowTag { column, .. } => *column,
                    _ => Col::LabelInt,
                }
            } else {
                Col::ItemId
            };
            let base_expr: SimpleExpr = if cond.is_some() {
                let inner_ptr = inner.as_ref() as *const ResolvedNode as usize;
                let pick_q = agg_ctx.agg_filters.get(&inner_ptr)
                    .expect("filter SQL must be pre-computed")
                    .clone();
                let mut pick_ids = Query::select();
                pick_ids.column(Col::ItemId).from_subquery(pick_q, Alias::new("ctx_agg"));
                Expr::case(Expr::col(Col::ItemId).in_subquery(pick_ids), Expr::col(col)).into()
            } else {
                Expr::col(col).into()
            };
            Expr::expr(base_expr).count_distinct().into()
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let (storage, cond, _) = inner.extract_agg_parts();
            let col = if let Some(s) = storage {
                match s {
                    StorageMapping::Column(c) => Col::from(*c),
                    StorageMapping::RowTag { column, .. } => *column,
                    _ => Col::LabelInt,
                }
            } else {
                Col::LabelInt
            };
            let base_expr: SimpleExpr = if cond.is_some() {
                let inner_ptr = inner.as_ref() as *const ResolvedNode as usize;
                let pick_q = agg_ctx.agg_filters.get(&inner_ptr)
                    .expect("filter SQL must be pre-computed")
                    .clone();
                let mut pick_ids = Query::select();
                pick_ids.column(Col::ItemId).from_subquery(pick_q, Alias::new("ctx_agg"));
                Expr::case(Expr::col(Col::ItemId).in_subquery(pick_ids), Expr::col(col)).into()
            } else {
                Expr::col(col).into()
            };
            match op {
                ArithmeticAggOp::Sum => Func::cust(Alias::new("SUM")).arg(base_expr).into(),
                ArithmeticAggOp::Avg => Func::cust(Alias::new("AVG")).arg(base_expr).into(),
                ArithmeticAggOp::Max => Func::cust(Alias::new("MAX")).arg(base_expr).into(),
                ArithmeticAggOp::Min => Func::cust(Alias::new("MIN")).arg(base_expr).into(),
            }
        }
    }
}

/// 算術演算のオペランドをSQL式に変換します（agg_ctx あり版）。
pub(super) fn build_agg_operand_expr(
    operand: &ResolvedOperand,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef { storage, sql_type, .. } => {
            build_storage_column_expr(storage, *sql_type)
        }
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] = child_results.try_into().unwrap();
            let is_string = calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(agg) => agg_expr(agg, agg_ctx),
    })
}

/// 算術演算ノードをSQL式に変換します（agg_ctx あり版）。
pub(super) fn build_agg_calc_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left_expr = build_agg_operand_expr(&calc.left, agg_ctx);
    let right_expr = build_agg_operand_expr(&calc.right, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
}

/// EAV 構造用のオペランドを集約式として構築します（agg_ctx あり版）。
pub(super) fn build_agg_operand_eav_expr(
    operand: &ResolvedOperand,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef { storage, sql_type, .. } => {
            build_tag_value_agg_expr(storage, *sql_type)
        }
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] = child_results.try_into().unwrap();
            let is_string = calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(agg) => {
            match agg {
                ResolvedAggregationNode::Arithmetic { inner, .. } => {
                    let (_, cond, operand) = inner.extract_agg_parts();
                    if operand.is_some() {
                        let base_expr = child_results.into_iter().next().unwrap();
                        if cond.is_some() {
                            let inner_ptr = inner.as_ref() as *const ResolvedNode as usize;
                            let pick_sql = agg_ctx.agg_filters.get(&inner_ptr)
                                .expect("filter SQL must be pre-computed")
                                .clone();
                            let mut sub = Query::select();
                            sub.column(Col::ItemId).from_subquery(pick_sql, Alias::new("f"));
                            Expr::cust_with_exprs(
                                "CASE WHEN item_id IN ($1) THEN $2 END",
                                [subquery(sub), base_expr],
                            )
                        } else {
                            base_expr
                        }
                    } else {
                        agg_expr(agg, agg_ctx)
                    }
                }
                _ => agg_expr(agg, agg_ctx),
            }
        }
    })
}

/// EAV 構造用の算術演算ノードを集約式に変換します（agg_ctx あり版）。
pub(super) fn build_agg_calc_eav_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left = build_agg_operand_eav_expr(&calc.left, agg_ctx);
    let right = build_agg_operand_eav_expr(&calc.right, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

/// オペランドをサブクエリ形式で構築します。
pub(super) fn build_agg_operand_subquery(
    operand: &ResolvedOperand,
    view: &str,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| {
        if let Some(expr) = fold_simple_operand(op, child_results) {
            return expr;
        }
        let ResolvedOperand::Aggregation(agg) = op else { unreachable!() };
        subquery(build_agg(agg, view, agg_ctx))
    })
}

/// Nest コンテキストを参照する集約を含むオペランドをサブクエリ形式で構築します。
pub(super) fn build_agg_operand_subquery_nest(
    operand: &ResolvedOperand,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| {
        if let Some(expr) = fold_simple_operand(op, child_results) {
            return expr;
        }
        let ResolvedOperand::Aggregation(agg) = op else { unreachable!() };
        subquery(build_agg_nest(agg, view, agg_ctx, nest_ctx))
    })
}

/// 算術演算ノードをサブクエリ形式で構築します。
pub(super) fn build_agg_calc_subquery(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left = build_agg_operand_subquery(&calc.left, view, agg_ctx);
    let right = build_agg_operand_subquery(&calc.right, view, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

/// Nest コンテキストを参照する集約を含む算術演算ノードをサブクエリ形式で構築します。
pub(super) fn build_agg_calc_subquery_nest(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SimpleExpr {
    let left = build_agg_operand_subquery_nest(&calc.left, view, agg_ctx, nest_ctx);
    let right = build_agg_operand_subquery_nest(&calc.right, view, agg_ctx, nest_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

/// 算術演算用のオペランドをSQL式に変換します。
/// RowTag の LabelStr (VARCHAR) は TRY_CAST で DOUBLE に変換されます。
pub(super) fn build_resolved_operand_expr_for_arithmetic(
    operand: &ResolvedOperand,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| {
        if op.is_string_type() {
            return build_agg_operand_expr(op, agg_ctx);
        }
        match op {
            ResolvedOperand::Literal(lab) => {
                let expr = build_resolved_literal_expr(lab);
                if matches!(lab.value(), crate::types::LabelValue::Boolean(_)) {
                    expr.cast_as(crate::db::SqlType::BIGINT).into()
                } else {
                    expr
                }
            }
            ResolvedOperand::TagRef { storage, sql_type, .. } => {
                if *sql_type == crate::db::SqlType::BOOLEAN {
                    return build_storage_column_expr(storage, *sql_type)
                        .cast_as(crate::db::SqlType::BIGINT)
                        .into();
                }
                match storage {
                    StorageMapping::RowTag { column, .. } if *column == Col::LabelStr => {
                        Expr::cust_with_exprs("TRY_CAST($1 AS DOUBLE)", [Expr::col(*column).into()])
                    }
                    StorageMapping::RowTag { column, .. } => Expr::col(*column).into(),
                    StorageMapping::Column(col) => Expr::col(*col).into(),
                    StorageMapping::Virtual => Expr::col(Col::LabelStr).into(),
                }
            }
            ResolvedOperand::Calculation(calc) => {
                let [left, right]: [SimpleExpr; 2] = child_results.try_into().unwrap();
                let is_string = calc.left.is_string_type() && calc.right.is_string_type();
                apply_arithmetic_op(&calc.op, left, right, is_string)
            }
            ResolvedOperand::Aggregation(agg) => agg_expr(agg, agg_ctx),
        }
    })
}

/// オペランド内に含まれる RowTag のキーをすべて抽出します。
pub(super) fn collect_tag_types(
    operand: &ResolvedOperand,
    keys: &mut Vec<String>,
) {
    for op in operand.walk() {
        match op {
            ResolvedOperand::TagRef {
                storage: StorageMapping::RowTag { tag_type, .. }, ..
            } => keys.push(tag_type.clone()),
            ResolvedOperand::Aggregation(agg) => match agg {
                ResolvedAggregationNode::Count(inner) => {
                    let (storage, _, _) = inner.extract_agg_parts();
                    if let Some(StorageMapping::RowTag { tag_type, .. }) = storage {
                        keys.push(tag_type.clone());
                    }
                }
                ResolvedAggregationNode::Arithmetic { inner, .. } => {
                    let (storage, _, _) = inner.extract_agg_parts();
                    if let Some(StorageMapping::RowTag { tag_type, .. }) = storage {
                        keys.push(tag_type.clone());
                    }
                }
            },
            _ => {}
        }
    }
}

/// オペランド内に含まれる RowTag 型を HashSet に収集します。
pub(super) fn collect_tag_types_from_operand(
    operand: &ResolvedOperand,
    set: &mut std::collections::HashSet<String>,
) {
    for op in operand.walk() {
        if let ResolvedOperand::TagRef {
            storage: StorageMapping::RowTag { tag_type, .. }, ..
        } = op
        {
            set.insert(tag_type.clone());
        }
    }
}

// ── nvalue SQL ─────────────────────────────────────────────────────────────

/// Count 集約の nvalue SQL を生成する共通ヘルパー。
pub(super) fn build_count_nvalue_sql(
    proj_col: Col,
    proj_tag_type: Option<&str>,
    inner: &ResolvedNode,
    context: Option<&ResolvedNode>,
    item_scope: Option<SelectStatement>,
    view: &str,
    include_item_id: bool,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    let (count_col, inner_tag_type) = resolve_count_target(inner);
    let mut stmt = Query::select();

    if let Some(tag_type) = inner_tag_type {
        stmt.expr_as(
            Expr::col((Alias::new("proj"), proj_col)),
            Alias::new("group_label"),
        );
        stmt.expr_as(
            Expr::col((Alias::new("inner_tags"), count_col)).count_distinct(),
            Alias::new("nvalue"),
        );
        stmt.from_as(Alias::new(view), Alias::new("proj"));
        stmt.join_as(
            sea_query::JoinType::InnerJoin,
            Alias::new(view),
            Alias::new("inner_tags"),
            Expr::col((Alias::new("proj"), Col::ItemId))
                .equals((Alias::new("inner_tags"), Col::ItemId)),
        );
        if let Some(tt) = proj_tag_type {
            stmt.and_where(Expr::col((Alias::new("proj"), Col::Type)).eq(tt));
        }
        stmt.and_where(Expr::col((Alias::new("inner_tags"), Col::Type)).eq(tag_type));
        if let Some(scope) = item_scope {
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(scope),
            );
        }
        let (_storage, inner_filter, _operand) = inner.extract_agg_parts();
        if inner_filter.is_some() {
            let inner_ptr = inner as *const ResolvedNode as usize;
            let filter_pick = agg_ctx
                .agg_filters
                .get(&inner_ptr)
                .expect("filter SQL must be pre-computed")
                .clone();
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from_subquery(filter_pick, Alias::new("nv_filter"))
                        .to_owned(),
                ),
            );
        }
        if let Some(ctx) = context {
            let ctx_ptr = ctx as *const ResolvedNode as usize;
            let context_pick = nest_ctx
                .expect("NestContext required for context lookup")
                .contexts
                .get(&ctx_ptr)
                .expect("context SQL must be pre-computed")
                .clone();
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from_subquery(context_pick, Alias::new("nv_context"))
                        .to_owned(),
                ),
            );
        }
        stmt.group_by_col((Alias::new("proj"), proj_col));
    } else {
        stmt.expr_as(Expr::col(proj_col), Alias::new("group_label"));
        stmt.expr_as(Expr::col(Col::ItemId).count_distinct(), Alias::new("nvalue"));
        stmt.from(Alias::new(view));
        if let Some(tt) = proj_tag_type {
            stmt.and_where(Expr::col(Col::Type).eq(tt));
        }
        if let Some(scope) = item_scope {
            stmt.and_where(Expr::col(Col::ItemId).in_subquery(scope));
        }
        let inner_ptr = inner as *const ResolvedNode as usize;
        let inner_pick = agg_ctx
            .agg_inner_sqls
            .get(&inner_ptr)
            .expect("inner SQL must be pre-computed")
            .clone();
        stmt.and_where(
            Expr::col(Col::ItemId).in_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .from_subquery(inner_pick, Alias::new("nv_inner"))
                    .to_owned(),
            ),
        );
        if let Some(ctx) = context {
            let ctx_ptr = ctx as *const ResolvedNode as usize;
            let context_pick = nest_ctx
                .expect("NestContext required for context lookup")
                .contexts
                .get(&ctx_ptr)
                .expect("context SQL must be pre-computed")
                .clone();
            stmt.and_where(
                Expr::col(Col::ItemId).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from_subquery(context_pick, Alias::new("nv_context"))
                        .to_owned(),
                ),
            );
        }
        stmt.group_by_col(proj_col);
    }

    if include_item_id {
        wrap_with_item_id(stmt, proj_col, proj_tag_type, view)
    } else {
        stmt
    }
}

/// 同一アイテムの重複を排除した集計用サブクエリを構築します。
pub(super) fn build_unique_agg(
    inner: &ResolvedNode,
    view: &str,
    context: Option<&ResolvedNode>,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    let (_storage, cond, operand) = inner.extract_agg_parts();
    let operand_expr = if let Some(op_node) = operand {
        build_resolved_operand_expr_for_arithmetic(op_node, agg_ctx)
    } else {
        Expr::val(0).into()
    };
    let mut sub = Query::select();
    sub.column(Col::ItemId)
        .expr_as(CustomFunc::any_value(operand_expr), Alias::new("val"))
        .from(Alias::new(view));
    if let Some(op_node) = operand {
        let mut keys = Vec::new();
        collect_tag_types(op_node, &mut keys);
        if !keys.is_empty() {
            sub.and_where(Expr::col(Col::Type).is_in(keys));
        }
    }
    if let Some(ctx) = context {
        let ctx_ptr = ctx as *const ResolvedNode as usize;
        sub.and_where(
            Expr::col(Col::ItemId).in_subquery(wrap_to_item_ids(
                nest_ctx
                    .expect("NestContext required for context lookup")
                    .contexts
                    .get(&ctx_ptr)
                    .expect("context SQL must be pre-computed")
                    .clone(),
            )),
        );
    }
    if let Some(_filter_node) = cond {
        let inner_ptr = inner as *const ResolvedNode as usize;
        sub.and_where(
            Expr::col(Col::ItemId).in_subquery(wrap_to_item_ids(
                agg_ctx
                    .agg_filters
                    .get(&inner_ptr)
                    .expect("filter SQL must be pre-computed")
                    .clone(),
            )),
        );
    }
    sub.group_by_col(Col::ItemId);
    sub
}

/// nvalue サブクエリ（スタンドアロン版）。
pub(super) fn build_nvalue_standalone_subquery(
    proj_operand: &ResolvedOperand,
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
    include_item_id: bool,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    let (proj_col, proj_tag_type) = match proj_operand {
        ResolvedOperand::TagRef { storage, .. } => match storage {
            StorageMapping::RowTag { column, tag_type, .. } => (*column, Some(tag_type.as_str())),
            StorageMapping::Column(col) => (*col, None),
            _ => return SelectStatement::default(),
        },
        _ => return SelectStatement::default(),
    };

    nvalue.fold(&|op, child_results: Vec<SelectStatement>| match op {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            build_count_nvalue_sql(proj_col, proj_tag_type, inner, context, None, view, include_item_id, agg_ctx, nest_ctx)
        }
        ResolvedOperand::Aggregation(agg @ ResolvedAggregationNode::Arithmetic { op, inner }) => {
            let is_string = agg.is_string_type();
            let deduped = build_unique_agg(inner, view, context, agg_ctx, nest_ctx);
            let mut stmt = Query::select();
            stmt.expr_as(Expr::col((Alias::new("proj"), proj_col)), Alias::new("group_label"));
            stmt.expr_as(
                apply_arithmetic_agg(
                    op,
                    Expr::col((Alias::new("deduped"), Alias::new("val"))).into(),
                    is_string,
                ),
                Alias::new("nvalue"),
            );
            stmt.from_as(Alias::new(view), Alias::new("proj"));
            stmt.join_subquery(
                sea_query::JoinType::InnerJoin,
                deduped,
                Alias::new("deduped"),
                Expr::col((Alias::new("proj"), Col::ItemId))
                    .equals((Alias::new("deduped"), Col::ItemId)),
            );
            if let Some(tt) = proj_tag_type {
                stmt.and_where(Expr::col((Alias::new("proj"), Col::Type)).eq(tt));
            }
            stmt.group_by_col((Alias::new("proj"), proj_col));
            if include_item_id { wrap_with_item_id(stmt, proj_col, proj_tag_type, view) } else { stmt }
        }
        ResolvedOperand::Literal(label) => {
            let val = label_to_simple_expr(label);
            let mut stmt = Query::select();
            stmt.expr_as(Expr::col(proj_col), Alias::new("group_label"));
            stmt.expr_as(val, Alias::new("nvalue"));
            stmt.from(Alias::new(view));
            if let Some(tt) = proj_tag_type {
                stmt.and_where(Expr::col(Col::Type).eq(tt));
            }
            if include_item_id {
                stmt.column(Col::ItemId);
                stmt.group_by_col(proj_col);
                stmt.group_by_col(Col::ItemId);
            } else {
                stmt.group_by_col(proj_col);
            }
            stmt
        }
        ResolvedOperand::Calculation(calc) => {
            let [sub_l, sub_r]: [SelectStatement; 2] = child_results.try_into().unwrap();
            let is_string = calc.left.is_string_type() && calc.right.is_string_type();
            let r_nvalue_expr: SimpleExpr = if is_string {
                Func::coalesce([
                    Expr::col((Alias::new("R"), Alias::new("nvalue"))).into(),
                    Expr::val("").into(),
                ]).into()
            } else {
                Func::coalesce([
                    Expr::col((Alias::new("R"), Alias::new("nvalue"))).into(),
                    Expr::val(0.0f64).into(),
                ]).into()
            };
            let mut stmt = Query::select();
            stmt.expr_as(Expr::col((Alias::new("L"), Alias::new("group_label"))), Alias::new("group_label"));
            stmt.expr_as(
                apply_arithmetic_op(
                    &calc.op,
                    Expr::col((Alias::new("L"), Alias::new("nvalue"))).into(),
                    r_nvalue_expr,
                    is_string,
                ),
                Alias::new("nvalue"),
            );
            if include_item_id { stmt.column((Alias::new("L"), Col::ItemId)); }
            stmt.from_subquery(sub_l, Alias::new("L"));
            if include_item_id {
                stmt.join_subquery(
                    sea_query::JoinType::LeftJoin, sub_r, Alias::new("R"),
                    Expr::col((Alias::new("L"), Col::ItemId))
                        .eq(Expr::col((Alias::new("R"), Col::ItemId))),
                );
            } else {
                stmt.join_subquery(
                    sea_query::JoinType::LeftJoin, sub_r, Alias::new("R"),
                    Expr::col((Alias::new("L"), Alias::new("group_label")))
                        .eq(Expr::col((Alias::new("R"), Alias::new("group_label")))),
                );
            }
            let mut outer = Query::select();
            outer.column(Alias::new("group_label")).column(Alias::new("nvalue"));
            if include_item_id { outer.column(Col::ItemId); }
            outer.from_subquery(stmt, Alias::new("calc_sub")).to_owned()
        }
        ResolvedOperand::TagRef { storage: nval_storage, .. } => {
            let mut stmt = Query::select();
            stmt.from_as(Alias::new(view), Alias::new("proj"));
            match nval_storage {
                StorageMapping::Column(nv_col) => {
                    stmt.expr_as(Expr::col((Alias::new("proj"), proj_col)), Alias::new("group_label"));
                    stmt.expr_as(
                        CustomFunc::any_value(Expr::col((Alias::new("proj"), *nv_col))),
                        Alias::new("nvalue"),
                    );
                }
                StorageMapping::RowTag { column: nv_col, tag_type: nv_tag_type } => {
                    let nv_sub = Query::select()
                        .column(Col::ItemId)
                        .expr_as(
                            Expr::cust_with_exprs("TRY_CAST($1 AS DOUBLE)", [Expr::col(*nv_col).into()]),
                            Alias::new("nval"),
                        )
                        .from(Alias::new(view))
                        .and_where(Expr::col(Col::Type).eq(nv_tag_type.as_str()))
                        .to_owned();
                    stmt.join_subquery(
                        sea_query::JoinType::LeftJoin, nv_sub, Alias::new("nv"),
                        Expr::col((Alias::new("proj"), Col::ItemId))
                            .equals((Alias::new("nv"), Col::ItemId)),
                    );
                    stmt.expr_as(Expr::col((Alias::new("proj"), proj_col)), Alias::new("group_label"));
                    stmt.expr_as(
                        CustomFunc::any_value(Func::coalesce([
                            Expr::col((Alias::new("nv"), Alias::new("nval"))).into(),
                            Expr::val(0.0f64).into(),
                        ])),
                        Alias::new("nvalue"),
                    );
                }
                StorageMapping::Virtual => {
                    stmt.expr_as(Expr::col((Alias::new("proj"), proj_col)), Alias::new("group_label"));
                    stmt.expr_as(Expr::val(0.0f64), Alias::new("nvalue"));
                }
            }
            if let Some(tt) = proj_tag_type {
                stmt.and_where(Expr::col((Alias::new("proj"), Col::Type)).eq(tt));
            }
            stmt.group_by_col((Alias::new("proj"), proj_col));
            if include_item_id { wrap_with_item_id(stmt, proj_col, proj_tag_type, view) } else { stmt }
        }
    })
}

/// 集約 SQL を生成します（コンテキストなし）。
pub(super) fn build_agg(
    agg: &ResolvedAggregationNode,
    view: &str,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    build_agg_inner(agg, view, agg_ctx, None)
}

/// 集約 SQL を生成します（Nest コンテキストあり）。
pub(super) fn build_agg_nest(
    agg: &ResolvedAggregationNode,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    build_agg_inner(agg, view, agg_ctx, Some(nest_ctx))
}

fn build_agg_inner(
    agg: &ResolvedAggregationNode,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    if let Some(nvalue_agg_sql) = build_agg_over_nvalue(agg, view, agg_ctx, nest_ctx) {
        return nvalue_agg_sql;
    }
    let mut stmt = Query::select();
    match agg {
        ResolvedAggregationNode::Count(inner) => {
            stmt.from(Alias::new(view));
            let (_, cond, _) = inner.extract_agg_parts();
            let mut final_cond = Condition::all();
            let (count_col, inner_tag_type) = resolve_count_target(inner);
            if let Some(key) = inner_tag_type {
                final_cond = final_cond.add(Expr::col(Col::Type).eq(key));
            }
            if let Some(_filter_node) = cond {
                let inner_ptr = inner.as_ref() as *const ResolvedNode as usize;
                let pick_sql = agg_ctx
                    .agg_filters
                    .get(&inner_ptr)
                    .expect("filter SQL must be pre-computed")
                    .clone();
                let mut sub = Query::select();
                sub.column(Col::ItemId).from_subquery(pick_sql, Alias::new("sub"));
                final_cond = final_cond.add(Expr::col(Col::ItemId).in_subquery(sub));
            }
            stmt.expr_as(Expr::col(count_col).count_distinct(), Alias::new("scalar_value"));
            stmt.cond_where(final_cond);
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let is_string = agg.is_string_type();
            let sub = build_unique_agg(inner, view, None, agg_ctx, nest_ctx);
            stmt.expr_as(
                apply_arithmetic_agg(op, Expr::col(Alias::new("val")).into(), is_string),
                Alias::new("scalar_value"),
            );
            stmt.from_subquery(sub, Alias::new("deduped_items"));
        }
    }
    stmt
}

fn build_agg_over_nvalue(
    agg: &ResolvedAggregationNode,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> Option<SelectStatement> {
    let (outer_agg_op, inner) = match agg {
        ResolvedAggregationNode::Count(inner) => (None, inner.as_ref()),
        ResolvedAggregationNode::Arithmetic { op, inner } => (Some(op), inner.as_ref()),
    };
    let proj_operand = inner.get_projection_operands()?;
    let nvalue = inner.get_nvalue()?;
    let context = inner.get_agg_context();
    let nvalue_condition = inner.get_nvalue_condition();

    let source = if proj_operand.len() > 1 {
        let pivot_agg = build_nvalue_pivot_aggregate_sql(proj_operand, nvalue, context, view, agg_ctx, nest_ctx);
        if let Some((op, value)) = nvalue_condition {
            let bin_op = to_bin_op(*op);
            let val = label_to_simple_expr(value);
            Query::select()
                .column(Alias::new("nvalue"))
                .from_subquery(pivot_agg, Alias::new("pivot_agg"))
                .and_where(Expr::col(Alias::new("nvalue")).binary(bin_op, val))
                .to_owned()
        } else {
            pivot_agg
        }
    } else {
        let mut nvalue_sub = build_nvalue_standalone_subquery(
            &proj_operand[0], nvalue, context, view, true, agg_ctx, nest_ctx,
        );
        if let Some((op, value)) = nvalue_condition {
            let bin_op = to_bin_op(*op);
            let val = label_to_simple_expr(value);
            nvalue_sub.and_where(Expr::col(Alias::new("nvalue")).binary(bin_op, val));
        }
        Query::select()
            .column(Alias::new("group_label"))
            .column(Alias::new("nvalue"))
            .from_subquery(nvalue_sub, Alias::new("nv_items"))
            .group_by_col(Alias::new("group_label"))
            .group_by_col(Alias::new("nvalue"))
            .to_owned()
    };

    let mut stmt = Query::select();
    match outer_agg_op {
        None => { stmt.expr_as(Expr::cust("COUNT(*)"), Alias::new("scalar_value")); }
        Some(op) => {
            let is_string = nvalue.is_string_type();
            stmt.expr_as(
                apply_arithmetic_agg(op, Expr::col(Alias::new("nvalue")).into(), is_string),
                Alias::new("scalar_value"),
            );
        }
    }
    stmt.from_subquery(source, Alias::new("nv_groups"));
    Some(stmt)
}

/// nvalue集計用CTE（picked_ids 参照版）を構築します（コンテキストなし）。
pub(super) fn build_nvalue_cte(
    proj_operands: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    build_nvalue_cte_inner(proj_operands, nvalue, context, view, agg_ctx, None)
}

/// nvalue集計用CTE（picked_ids 参照版）を構築します（Nest コンテキストあり）。
pub(super) fn build_nvalue_cte_nest(
    proj_operands: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    build_nvalue_cte_inner(proj_operands, nvalue, context, view, agg_ctx, Some(nest_ctx))
}

fn build_nvalue_cte_inner(
    proj_operands: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    if proj_operands.len() > 1 {
        return build_nvalue_pivot_aggregate_sql(proj_operands, nvalue, context, view, agg_ctx, nest_ctx);
    }
    let proj_operand = &proj_operands[0];
    let (proj_col, proj_storage) = match proj_operand {
        ResolvedOperand::TagRef { storage, .. } => match storage {
            StorageMapping::RowTag { column, .. } => (*column, storage),
            StorageMapping::Column(col) => (*col, storage),
            _ => return SelectStatement::default(),
        },
        _ => return SelectStatement::default(),
    };
    let proj_tag_type = if let StorageMapping::RowTag { tag_type, .. } = &proj_storage {
        Some(tag_type.as_str())
    } else {
        None
    };

    let inner_q = match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            build_count_nvalue_sql(
                proj_col, proj_tag_type, inner, context,
                Some(Query::select().column(Col::ItemId).from(Tbl::PickedIds).to_owned()),
                view, true, agg_ctx, nest_ctx,
            )
        }
        ResolvedOperand::Aggregation(agg @ ResolvedAggregationNode::Arithmetic { op, inner }) => {
            let is_string = agg.is_string_type();
            let deduped = build_unique_agg(inner, view, context, agg_ctx, nest_ctx);
            let mut stmt = Query::select();
            stmt.expr_as(Expr::col((Alias::new("proj"), proj_col)), Alias::new("group_label"));
            stmt.expr_as(
                apply_arithmetic_agg(
                    op,
                    Expr::col((Alias::new("deduped"), Alias::new("val"))).into(),
                    is_string,
                ),
                Alias::new("nvalue"),
            );
            stmt.from_as(Alias::new(view), Alias::new("proj"));
            stmt.join_subquery(
                sea_query::JoinType::InnerJoin, deduped, Alias::new("deduped"),
                Expr::col((Alias::new("proj"), Col::ItemId))
                    .equals((Alias::new("deduped"), Col::ItemId)),
            );
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(
                    Query::select().column(Col::ItemId).from(Tbl::PickedIds).to_owned(),
                ),
            );
            if let Some(tt) = proj_tag_type {
                stmt.and_where(Expr::col((Alias::new("proj"), Col::Type)).eq(tt));
            }
            stmt.group_by_col((Alias::new("proj"), proj_col));
            wrap_with_item_id(stmt, proj_col, proj_tag_type, view)
        }
        _ => build_nvalue_standalone_subquery(proj_operand, nvalue, context, view, true, agg_ctx, nest_ctx),
    };

    Query::select()
        .column(Alias::new("group_label"))
        .column(Alias::new("nvalue"))
        .from_subquery(inner_q, Alias::new("nv_items"))
        .group_by_col(Alias::new("group_label"))
        .group_by_col(Alias::new("nvalue"))
        .to_owned()
}

// ── Nest / Pivot SQL ───────────────────────────────────────────────────────

/// 多キー Nest の Pivot CTE を構築します。
pub(super) fn build_nest_pivot_cte(
    keys: &[ResolvedOperand],
    nvalue: Option<&ResolvedOperand>,
    view: &str,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.column(Col::ItemId);
    stmt.expr_as(CustomFunc::any_value(Expr::col(Col::Rank)), Alias::new("rank"));
    stmt.expr_as(CustomFunc::any_value(Expr::col(Col::ItemKind)), Alias::new("item_kind"));
    stmt.from(Alias::new(view));

    let mut type_filters = std::collections::HashSet::new();

    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef { storage, .. } => match storage {
                StorageMapping::RowTag { tag_type, column } => {
                    type_filters.insert(tag_type.as_str().to_string());
                    let case_expr = Expr::case(Expr::col(Col::Type).eq(tag_type.as_str()), Expr::col(*column));
                    let max_expr = Expr::cust_with_exprs("MAX($1)", [case_expr.into()]);
                    stmt.expr_as(max_expr.clone(), Alias::new(&format!("key{}", i)));
                    stmt.and_having(max_expr.is_not_null());
                }
                StorageMapping::Column(col) => {
                    let max_expr = Expr::col(*col).max();
                    stmt.expr_as(max_expr.clone(), Alias::new(&format!("key{}", i)));
                    stmt.and_having(max_expr.is_not_null());
                }
                _ => {}
            },
            ResolvedOperand::Calculation(calc) => {
                collect_tag_types_from_operand(&calc.left, &mut type_filters);
                collect_tag_types_from_operand(&calc.right, &mut type_filters);
                let calc_expr = build_agg_calc_eav_expr(calc, agg_ctx);
                stmt.expr_as(calc_expr.clone(), Alias::new(&format!("key{}", i)));
                stmt.and_having(calc_expr.is_not_null());
            }
            _ => {}
        }
    }

    if let Some(nv) = nvalue {
        let nv_expr = build_agg_operand_eav_expr(nv, agg_ctx);
        stmt.expr_as(nv_expr, Alias::new("nvalue"));
    } else if !type_filters.is_empty() {
        stmt.and_where(Expr::col(Col::Type).is_in(type_filters.clone()));
    }

    stmt.group_by_col(Col::ItemId);
    stmt
}

/// 多キー Nest の nvalue Pivot 集計 SQL を構築します。
pub(super) fn build_nvalue_pivot_aggregate_sql(
    keys: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    if let ResolvedOperand::Calculation(calc) = nvalue {
        return build_mixed_key_calc_nvalue_sql(keys, calc, context, view, agg_ctx, nest_ctx);
    }

    let pivot_q = build_nest_pivot_cte(keys, Some(nvalue), view, agg_ctx);
    let mut stmt = Query::select();
    for i in 0..keys.len() {
        stmt.column(Alias::new(&format!("key{}", i)));
    }

    let is_string = nvalue.is_string_type();
    let nval_expr = match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_)) => {
            Expr::col(Alias::new("nvalue")).count().into()
        }
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Arithmetic { op, .. }) => {
            apply_arithmetic_agg(op, Expr::col(Alias::new("nvalue")).into(), is_string)
        }
        _ => CustomFunc::any_value(Expr::col(Alias::new("nvalue"))).into(),
    };

    stmt.expr_as(nval_expr, Alias::new("nvalue"));
    stmt.from_subquery(pivot_q, Alias::new("pivot"));
    for i in 0..keys.len() {
        stmt.group_by_col(Alias::new(&format!("key{}", i)));
    }

    if let Some(ctx) = context {
        let ctx_ptr = ctx as *const ResolvedNode as usize;
        let ctx_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(
                nest_ctx
                    .expect("NestContext required for context lookup")
                    .contexts
                    .get(&ctx_ptr)
                    .expect("context SQL must be pre-computed")
                    .clone(),
                Alias::new("_ctx"),
            )
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(ctx_sub));
    }

    stmt
}

/// ResolvedOperand が対応するキー数を返します。
pub(super) fn count_nvalue_keys(nvalue: &ResolvedOperand) -> usize {
    nvalue.fold(&|op, child_results: Vec<usize>| match op {
        ResolvedOperand::Calculation(_) => child_results.into_iter().sum(),
        ResolvedOperand::Literal(_) => 0,
        _ => 1,
    })
}

fn build_mixed_key_calc_nvalue_sql(
    keys: &[ResolvedOperand],
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    context: Option<&ResolvedNode>,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    let n_left = count_nvalue_keys(&calc.left).max(1).min(keys.len() - 1);
    let left_sub = build_nvalue_standalone_subquery(&keys[0], &calc.left, context, view, false, agg_ctx, nest_ctx);
    let right_sub = build_nvalue_standalone_subquery(&keys[n_left], &calc.right, context, view, false, agg_ctx, nest_ctx);
    let pivot_sub = build_nest_pivot_cte(keys, None, view, agg_ctx);

    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    let l_nvalue: SimpleExpr = Func::coalesce([
        Expr::col((Alias::new("L"), Alias::new("nvalue"))).into(),
        if is_string { Expr::val("").into() } else { Expr::val(0.0f64).into() },
    ]).into();
    let r_nvalue: SimpleExpr = Func::coalesce([
        Expr::col((Alias::new("R"), Alias::new("nvalue"))).into(),
        if is_string { Expr::val("").into() } else { Expr::val(0.0f64).into() },
    ]).into();

    let mut stmt = Query::select();
    stmt.distinct();
    for i in 0..keys.len() {
        stmt.expr_as(
            Expr::col((Alias::new("pivot"), Alias::new(&format!("key{}", i)))),
            Alias::new(&format!("key{}", i)),
        );
    }
    stmt.expr_as(apply_arithmetic_op(&calc.op, l_nvalue, r_nvalue, is_string), Alias::new("nvalue"));
    stmt.from_subquery(pivot_sub, Alias::new("pivot"));
    stmt.join_subquery(
        sea_query::JoinType::LeftJoin, left_sub, Alias::new("L"),
        Expr::col((Alias::new("pivot"), Alias::new("key0")))
            .equals((Alias::new("L"), Alias::new("group_label"))),
    );
    stmt.join_subquery(
        sea_query::JoinType::LeftJoin, right_sub, Alias::new("R"),
        Expr::col((Alias::new("pivot"), Alias::new(&format!("key{}", n_left))))
            .equals((Alias::new("R"), Alias::new("group_label"))),
    );
    for i in 0..keys.len() {
        stmt.and_where(
            Expr::col((Alias::new("pivot"), Alias::new(&format!("key{}", i)))).is_not_null(),
        );
    }
    if let Some(ctx) = context {
        let ctx_ptr = ctx as *const ResolvedNode as usize;
        let ctx_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(
                nest_ctx
                    .expect("NestContext required for context lookup")
                    .contexts
                    .get(&ctx_ptr)
                    .expect("context SQL must be pre-computed")
                    .clone(),
                Alias::new("_ctx"),
            )
            .to_owned();
        stmt.and_where(Expr::col((Alias::new("pivot"), Col::ItemId)).in_subquery(ctx_sub));
    }
    stmt
}
