use crate::db::{Col, CustomFunc, SqlType, Tbl};
use crate::query::ast::{ArithmeticAggOp, ComparisonOp, QueryNode};
use crate::query::lens_resolver::{LabelSetOpKind, ResolvedAggregationNode, ResolvedNode, ResolvedOperand};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{ItemKind, Label, SType, TagType};
use sea_query::{Alias, BinOper, Condition, Expr, ExprTrait, Func, Query, SelectStatement, SimpleExpr};
use super::{
    subquery, wrap_in_subquery, wrap_to_item_ids,
    label_to_simple_expr, label_to_unit_aware_expr,
    build_resolved_literal_expr, build_storage_column_expr,
    apply_arithmetic_op, apply_arithmetic_agg,
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

/// StorageMapping から集計式を生成します（EAV 構造用の MAX CASE WHEN 形式）。
pub(super) fn build_tag_value_agg_expr(
    storage: &StorageMapping,
    _sql_type: crate::db::SqlType,
) -> SimpleExpr {
    match storage {
        StorageMapping::Column(col) => CustomFunc::any_value(Expr::col(*col)).into(),
        StorageMapping::RowTag { column, tag_type } => {
            let cast_expr = Expr::cust_with_exprs(
                "TRY_CAST($1 AS DOUBLE)",
                [Expr::col(*column).into()],
            );
            Expr::cust_with_exprs(
                "MAX(CASE WHEN $1 = $2 THEN $3 END)",
                [
                    Expr::col(Col::Type).into(),
                    Expr::val(tag_type.as_str()).into(),
                    cast_expr.into(),
                ],
            )
        }
        StorageMapping::Virtual => CustomFunc::any_value(Expr::val(0)).into(),
    }
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

/// 算術演算のオペランドをSQL式に変換します。
pub(super) fn build_resolved_operand_expr(
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

/// 算術演算ノードをSQL式に変換します。
pub(super) fn build_calculation_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_expr(&calc.left, agg_ctx);
    let right_expr = build_resolved_operand_expr(&calc.right, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
}

/// 集約を含まない純粋な算術演算ノードをカラム参照式に変換します（agg_ctx 不要）。
pub(super) fn build_calculation_expr_pure(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_expr_pure(&calc.left);
    let right_expr = build_resolved_operand_expr_pure(&calc.right);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
}

fn build_resolved_operand_expr_pure(operand: &ResolvedOperand) -> SimpleExpr {
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
        ResolvedOperand::Aggregation(_) => {
            panic!("build_calculation_expr_pure called with aggregation operand; use build_calculation_expr instead")
        }
    })
}

/// EAV 構造用のオペランドを集約式として構築します。
pub(super) fn build_resolved_operand_eav_expr(
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

/// EAV 構造用の算術演算ノードを集約式に変換します。
pub(super) fn build_calculation_eav_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left = build_resolved_operand_eav_expr(&calc.left, agg_ctx);
    let right = build_resolved_operand_eav_expr(&calc.right, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

/// 集約を含まない純粋な EAV 算術演算ノードを集約式に変換します（agg_ctx 不要）。
pub(super) fn build_calculation_eav_expr_pure(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
) -> SimpleExpr {
    let left = build_resolved_operand_eav_expr_pure(&calc.left);
    let right = build_resolved_operand_eav_expr_pure(&calc.right);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

fn build_resolved_operand_eav_expr_pure(operand: &ResolvedOperand) -> SimpleExpr {
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
        ResolvedOperand::Aggregation(_) => {
            panic!("build_calculation_eav_expr_pure called with aggregation operand; use build_calculation_eav_expr instead")
        }
    })
}

/// Literal / TagRef / Calculation の各アームを処理します。
/// Aggregation は呼び出し元が個別に処理するため None を返します。
fn fold_simple_operand(op: &ResolvedOperand, child_results: Vec<SimpleExpr>) -> Option<SimpleExpr> {
    match op {
        ResolvedOperand::Literal(lab) => {
            let expr = if let Some(bytes) = crate::util::parse_size(&lab.as_str()) {
                Expr::val(bytes).cast_as(crate::db::SqlType::DOUBLE).into()
            } else {
                match lab.value() {
                    crate::types::LabelValue::Integer(i) => {
                        Expr::val(i).cast_as(crate::db::SqlType::DOUBLE).into()
                    }
                    crate::types::LabelValue::String(s)
                    | crate::types::LabelValue::Literal(s) => Expr::val(s.clone()).into(),
                    crate::types::LabelValue::Boolean(b) => Expr::val(b).into(),
                    crate::types::LabelValue::Double(bits) => {
                        Expr::val(f64::from_bits(bits)).into()
                    }
                    crate::types::LabelValue::Null => Expr::val(None::<i32>).into(),
                }
            };
            Some(expr)
        }
        ResolvedOperand::TagRef { .. } => Some(Expr::val(0).into()),
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] = child_results.try_into().unwrap();
            let is_string = calc.left.is_string_type() && calc.right.is_string_type();
            Some(apply_arithmetic_op(&calc.op, left, right, is_string))
        }
        ResolvedOperand::Aggregation(_) => None,
    }
}

/// オペランドをサブクエリ形式で構築します。
pub(super) fn build_resolved_operand_subquery(
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
pub(super) fn build_resolved_operand_subquery_nest(
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
pub(super) fn build_calculation_subquery(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left = build_resolved_operand_subquery(&calc.left, view, agg_ctx);
    let right = build_resolved_operand_subquery(&calc.right, view, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

/// Nest コンテキストを参照する集約を含む算術演算ノードをサブクエリ形式で構築します。
pub(super) fn build_calculation_subquery_nest(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SimpleExpr {
    let left = build_resolved_operand_subquery_nest(&calc.left, view, agg_ctx, nest_ctx);
    let right = build_resolved_operand_subquery_nest(&calc.right, view, agg_ctx, nest_ctx);
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
            return build_resolved_operand_expr(op, agg_ctx);
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

// ── Boolean SQL ────────────────────────────────────────────────────────────

/// 比較結果を直接SELECTで計算するSQL
pub(super) fn build_direct_boolean_select(
    left: SimpleExpr,
    op: ComparisonOp,
    right: SimpleExpr,
    _view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    let bin_op = to_bin_op(op);
    let comparison = Expr::expr(left).binary(bin_op, right);
    q.expr_as(
        Expr::case(comparison.clone(), Expr::val(1i64))
            .case(comparison.is_null(), Expr::cust("NULL"))
            .finally(Expr::val(0i64)),
        Col::ItemId,
    )
    .expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::ItemKind,
    )
    .expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::Type,
    )
    .expr_as(Expr::val(0i64), Col::Rank)
    .expr_as(Expr::cust("[]"), crate::db::QueryResultCol::Tags);
    q
}

/// ブーリアン結果をラップするSQL
pub(super) fn wrap_boolean_collider(sql: SelectStatement) -> SelectStatement {
    let mut q = Query::select();
    q.expr_as(
        Expr::case(
            CustomFunc::any_value(Expr::col((Alias::new("pk"), Col::ItemId))).is_not_null(),
            Expr::val(1i64),
        )
        .finally(Expr::val(0i64)),
        Col::ItemId,
    )
    .expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::ItemKind,
    )
    .expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::Type,
    )
    .expr_as(Expr::val(0i64), Col::Rank)
    .expr_as(Expr::cust("[]"), crate::db::QueryResultCol::Tags)
    .from_subquery(sql, Alias::new("pk"));
    q
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

/// nvalue 付き Nest に対する集約 SQL を生成します（コンテキストなし）。
pub(super) fn build_agg(
    agg: &ResolvedAggregationNode,
    view: &str,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    build_agg_inner(agg, view, agg_ctx, None)
}

/// nvalue 付き Nest に対する集約 SQL を生成します（Nest コンテキストあり）。
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

/// nvalue 付き Nest に対する集約 SQL を生成する内部ヘルパー。
/// agg_ctx / nest_ctx は呼び出し元が事前計算済みのものを渡す。
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
                let calc_expr = build_calculation_eav_expr(calc, agg_ctx);
                stmt.expr_as(calc_expr.clone(), Alias::new(&format!("key{}", i)));
                stmt.and_having(calc_expr.is_not_null());
            }
            _ => {}
        }
    }

    if let Some(nv) = nvalue {
        let nv_expr = build_resolved_operand_eav_expr(nv, agg_ctx);
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

/// Calculation nvalue を持つ異種キー Nest の nvalue 集計 SQL を構築します。
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

// ── LabelSetOp helpers ─────────────────────────────────────────────────────

/// LabelSetOp の先頭オペランドからプライマリラベルのタグ型文字列とカラムを取得します。
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

/// 多キー Nest ノードからキー列を抽出します。単一キーまたは非 Nest の場合は None を返します。
pub(super) fn extract_multi_key_nest_operands(
    node: &ResolvedNode,
) -> Option<Vec<ResolvedOperand>> {
    node.walk().into_iter().find_map(|n| match n {
        ResolvedNode::Nest { keys, .. } if keys.len() > 1 => Some(keys.clone()),
        _ => None,
    })
}

/// 多キー Nest の labels_i CTE 用 SQL を生成します。
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
                storage: StorageMapping::RowTag { tag_type, column }, ..
            } => {
                type_filters.insert(tag_type.as_str().to_string());
                let case_expr = Expr::case(Expr::col(Col::Type).eq(tag_type.as_str()), Expr::col(*column));
                let max_expr = Expr::cust_with_exprs("MAX($1)", [case_expr.into()]);
                pivot.expr_as(max_expr.clone(), Alias::new(&format!("key{}", i)));
                pivot.and_having(max_expr.is_not_null());
            }
            ResolvedOperand::TagRef { storage: StorageMapping::Column(col), .. } => {
                let max_expr = Expr::col(*col).max();
                pivot.expr_as(max_expr.clone(), Alias::new(&format!("key{}", i)));
                pivot.and_having(max_expr.is_not_null());
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "build_multi_key_labels_sql: unsupported key type at index {}", i
                ));
            }
        }
    }
    pivot.from(Alias::new(view));
    if !type_filters.is_empty() {
        pivot.and_where(Expr::col(Col::Type).is_in(type_filters));
    }
    pivot.and_where(Expr::col(Col::ItemId).in_subquery(ids_sql)).group_by_col(Col::ItemId);

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

// ── Tag condition ──────────────────────────────────────────────────────────

/// クエリに使用されている、または投影されている型のリストを元に
/// OneView から特定のタグ行のみを抽出するための Condition を生成します。
pub fn to_tag_condition(node: &QueryNode) -> sea_query::Condition {
    let mut types = node.get_all_types();
    if types.iter().any(|t| t == "*") {
        return sea_query::Condition::all();
    }
    let defaults = [
        "name", "path", "size", "mtime", "rank", "item_kind",
        "content", "value", "tag", "filename", "is_dir",
    ];
    for def in defaults {
        if !types.iter().any(|t| t == def) {
            types.push(def.to_string());
        }
    }
    if types.iter().any(|t| t == "*" || t == "tag") {
        return sea_query::Condition::all();
    }
    let mut cond = sea_query::Condition::any();
    let mut fixed_types = Vec::new();
    let glob_op = sea_query::BinOper::Custom("GLOB");
    for t in types {
        if t.contains('*') || t.contains('?') || t.contains('[') {
            cond = cond.add(Expr::col(Col::Type).binary(glob_op, Expr::val(t)));
        } else {
            fixed_types.push(t);
        }
    }
    if !fixed_types.is_empty() {
        cond = cond.add(Expr::col(Col::Type).is_in(fixed_types));
    }
    cond
}

// ── pick/filter 共通 SQL ビルダー ──────────────────────────────────────────

/// `child_sqls` を `union_type` で結合します。空の場合は `empty_fallback` を返します。
pub(super) fn reduce_with_union(
    child_sqls: Vec<SelectStatement>,
    union_type: sea_query::UnionType,
    empty_fallback: SelectStatement,
) -> SelectStatement {
    child_sqls.into_iter()
        .map(wrap_in_subquery)
        .reduce(|mut acc, next| { acc.union(union_type, next); acc })
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

pub(super) fn build_resolved_comp_sql(
    is_boolean: bool,
    c_sql: SelectStatement,
    view: &str,
) -> SelectStatement {
    let mut q = if is_boolean {
        Query::select()
            .expr_as(Expr::val(1i64), Col::ItemId)
            .expr_as(Expr::val(0i64), Col::Rank)
            .expr_as(
                Expr::val(<&'static str>::from(ItemKind::Volatile)),
                Col::ItemKind,
            )
            .to_owned()
    } else {
        Query::select()
            .columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .distinct()
            .from(Alias::new(view))
            .and_where(Expr::col(Col::ItemKind).is_not_in(vec!["type", "tag"]))
            .to_owned()
    };
    let mut eq = Query::select();
    eq.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from_subquery(c_sql, Tbl::NotSide);
    q.union(sea_query::UnionType::Except, eq);
    q
}

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
            let val_str = if s.starts_with('^') { format!("{}*", &s[1..]) } else { s.clone() };
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

pub(super) fn build_label_set_op_pick_sql(
    op: &LabelSetOpKind,
    child_sqls: Vec<SelectStatement>,
) -> SelectStatement {
    match op {
        LabelSetOpKind::Union => {
            reduce_with_union(child_sqls, sea_query::UnionType::Distinct, Query::select().to_owned())
        }
        LabelSetOpKind::Intersect => {
            reduce_with_union(child_sqls, sea_query::UnionType::Intersect, Query::select().to_owned())
        }
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
        _ => {
            Query::select()
                .columns([Col::ItemId, Col::Rank, Col::ItemKind])
                .distinct()
                .from(Alias::new(view))
                .to_owned()
        }
    })
}

pub(super) fn build_nest_sql(
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

pub(super) fn try_dispatch_common(
    node: &ResolvedNode,
    child_sqls: Vec<SelectStatement>,
    view: &str,
) -> Result<SelectStatement, Vec<SelectStatement>> {
    match node {
        ResolvedNode::And(_) => Ok(build_resolved_and_sql(child_sqls, view)),
        ResolvedNode::Or(_) => Ok(build_resolved_or_sql(child_sqls, view)),
        ResolvedNode::Difference(_, _) => {
            let [l, r]: [SelectStatement; 2] = child_sqls.try_into().unwrap();
            Ok(build_resolved_diff_sql(l, r))
        }
        ResolvedNode::Complement(c) => {
            let [c_sql]: [SelectStatement; 1] = child_sqls.try_into().unwrap();
            Ok(build_resolved_comp_sql(c.is_boolean_result(), c_sql, view))
        }
        ResolvedNode::LabelSetOp { op, .. } => Ok(build_label_set_op_pick_sql(op, child_sqls)),
        ResolvedNode::Nest { keys, .. } => {
            Ok(build_nest_sql(keys, child_sqls.into_iter().next(), view))
        }
        ResolvedNode::ColumnMatch { tag, label } => Ok(build_column_match_sql(*tag, label, view)),
        ResolvedNode::Match { storage, sql_type, op, label, .. } => {
            Ok(build_resolved_match_sql(storage, *sql_type, *op, label, view))
        }
        ResolvedNode::TagTagMatch {
            left_storage, left_sql_type, op, right_storage, right_sql_type,
        } => Ok(build_resolved_tag_tag_match_sql(
            left_storage, *left_sql_type, *op, right_storage, *right_sql_type, view,
        )),
        ResolvedNode::ScalarMatch { left, op, right } => Ok(build_scalar_match_sql(left, *op, right, view)),
        _ => Err(child_sqls),
    }
}
