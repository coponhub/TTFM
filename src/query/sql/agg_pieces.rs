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
    apply_arithmetic_agg, apply_arithmetic_op, build_calculation_eav_expr,
    build_resolved_literal_expr, build_storage_column_expr,
    build_tag_value_agg_expr, fold_simple_operand, label_to_simple_expr,
    subquery, wrap_to_item_ids, AggregationContext, NestContext,
};
use crate::db::{Col, CustomFunc, Pronoun::*, Src, Tbl};
use crate::query::ast::ArithmeticAggOp;
use crate::query::lens_resolver::{
    ResolvedAggregationNode, ResolvedNode, ResolvedOperand,
};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use sea_query::{
    Alias, Condition, Expr, ExprTrait, Func, Query, SelectStatement, SimpleExpr,
};

// ── 低レベルユーティリティ ──────────────────────────────────────────────────

/// Count の引数ノードから、カウント対象のカラムと内部タグタイプを決定する。
pub(super) fn resolve_count_target(
    inner: &ResolvedNode,
) -> (Col, Option<String>) {
    inner
        .get_nested_projection()
        .and_then(|op| match op.get_storage() {
            Some(StorageMapping::Basic {
                column, tag_type, ..
            }) => Some((*column, Some(tag_type.clone()))),
            Some(StorageMapping::Fixed(col)) => Some((*col, None)),
            _ => None,
        })
        .unwrap_or((Col::ItemId, None))
}

/// nvalue サブクエリ結果に item_id をアタッチしてラップします。
pub(super) fn wrap_with_item_id(
    src: &Src,
    agg_sub: SelectStatement,
    proj_col: Col,
    proj_tag_type: Option<&str>,
) -> SelectStatement {
    let mut wrapped = Query::select();
    wrapped
        .column((View, Col::ItemId))
        .expr_as(CustomFunc::as_representative(Expr::col(proj_col)), Group)
        .column((Agg, Nvalue))
        .from_as(src, View)
        .join_subquery(
            sea_query::JoinType::InnerJoin,
            agg_sub,
            Agg,
            CustomFunc::as_representative(Expr::col((View, proj_col)))
                .eq(Expr::col((Agg, Group))),
        );
    if let Some(tag_type) = proj_tag_type {
        wrapped.and_where(Expr::col((View, Col::Type)).eq(tag_type));
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
                    StorageMapping::Fixed(c) => Col::from(*c),
                    StorageMapping::Basic { column, .. } => *column,
                    _ => Col::LabelInt,
                }
            } else {
                Col::ItemId
            };
            let base_expr: SimpleExpr = if cond.is_some() {
                let inner_ptr = inner.as_ref() as *const ResolvedNode as usize;
                let pick_q = agg_ctx
                    .agg_filters
                    .get(&inner_ptr)
                    .expect("filter SQL must be pre-computed")
                    .clone();
                let mut pick_ids = Query::select();
                pick_ids.column(Col::ItemId).from_subquery(pick_q, Filter);
                Expr::case(
                    Expr::col(Col::ItemId).in_subquery(pick_ids),
                    Expr::col(col),
                )
                .into()
            } else {
                Expr::col(col).into()
            };
            Expr::expr(base_expr).count_distinct().into()
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let (storage, cond, _) = inner.extract_agg_parts();
            let col = if let Some(s) = storage {
                match s {
                    StorageMapping::Fixed(c) => Col::from(*c),
                    StorageMapping::Basic { column, .. } => *column,
                    _ => Col::LabelInt,
                }
            } else {
                Col::LabelInt
            };
            let base_expr: SimpleExpr = if cond.is_some() {
                let inner_ptr = inner.as_ref() as *const ResolvedNode as usize;
                let pick_q = agg_ctx
                    .agg_filters
                    .get(&inner_ptr)
                    .expect("filter SQL must be pre-computed")
                    .clone();
                let mut pick_ids = Query::select();
                pick_ids.column(Col::ItemId).from_subquery(pick_q, Filter);
                Expr::case(
                    Expr::col(Col::ItemId).in_subquery(pick_ids),
                    Expr::col(col),
                )
                .into()
            } else {
                Expr::col(col).into()
            };
            match op {
                ArithmeticAggOp::Sum => Func::sum(base_expr).into(),
                ArithmeticAggOp::Avg => Func::avg(base_expr).into(),
                ArithmeticAggOp::Max => Func::max(base_expr).into(),
                ArithmeticAggOp::Min => Func::min(base_expr).into(),
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
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => build_storage_column_expr(storage, *sql_type),
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
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => build_tag_value_agg_expr(storage, *sql_type),
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] =
                child_results.try_into().unwrap();
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(agg) => match agg {
            ResolvedAggregationNode::Arithmetic { inner, .. } => {
                let (_, cond, operand) = inner.extract_agg_parts();
                if operand.is_some() {
                    let base_expr = child_results.into_iter().next().unwrap();
                    if cond.is_some() {
                        let inner_ptr =
                            inner.as_ref() as *const ResolvedNode as usize;
                        let pick_sql = agg_ctx
                            .agg_filters
                            .get(&inner_ptr)
                            .expect("filter SQL must be pre-computed")
                            .clone();
                        let mut sub = Query::select();
                        sub.column(Col::ItemId).from_subquery(pick_sql, Filter);
                        Expr::case(
                            Expr::col(Col::ItemId).in_subquery(sub),
                            base_expr,
                        )
                        .into()
                    } else {
                        base_expr
                    }
                } else {
                    agg_expr(agg, agg_ctx)
                }
            }
            _ => agg_expr(agg, agg_ctx),
        },
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
    src: &Src,
    operand: &ResolvedOperand,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| {
        if let Some(expr) = fold_simple_operand(op, child_results) {
            return expr;
        }
        let ResolvedOperand::Aggregation(agg) = op else {
            unreachable!()
        };
        subquery(build_agg(src, agg, agg_ctx))
    })
}

/// Nest コンテキストを参照する集約を含むオペランドをサブクエリ形式で構築します。
pub(super) fn build_agg_operand_subquery_nest(
    src: &Src,
    operand: &ResolvedOperand,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| {
        if let Some(expr) = fold_simple_operand(op, child_results) {
            return expr;
        }
        let ResolvedOperand::Aggregation(agg) = op else {
            unreachable!()
        };
        subquery(build_agg_nest(src, agg, agg_ctx, nest_ctx))
    })
}

/// 算術演算ノードをサブクエリ形式で構築します。
pub(super) fn build_agg_calc_subquery(
    src: &Src,
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
) -> SimpleExpr {
    let left = build_agg_operand_subquery(src, &calc.left, agg_ctx);
    let right = build_agg_operand_subquery(src, &calc.right, agg_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

/// Nest コンテキストを参照する集約を含む算術演算ノードをサブクエリ形式で構築します。
pub(super) fn build_agg_calc_subquery_nest(
    src: &Src,
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SimpleExpr {
    let left =
        build_agg_operand_subquery_nest(src, &calc.left, agg_ctx, nest_ctx);
    let right =
        build_agg_operand_subquery_nest(src, &calc.right, agg_ctx, nest_ctx);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

/// 算術演算用のオペランドをSQL式に変換します。
/// LabelStr (VARCHAR) は TRY_CAST で DOUBLE に変換されます。
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
            ResolvedOperand::TagRef {
                storage, sql_type, ..
            } => {
                if *sql_type == crate::db::SqlType::BOOLEAN {
                    return build_storage_column_expr(storage, *sql_type)
                        .cast_as(crate::db::SqlType::BIGINT)
                        .into();
                }
                match storage {
                    StorageMapping::Basic { column, .. }
                        if *column == Col::LabelStr =>
                    {
                        CustomFunc::try_cast_double(Expr::col(*column))
                    }
                    StorageMapping::Basic { column, .. } => {
                        Expr::col(*column).into()
                    }
                    StorageMapping::Fixed(col) => Expr::col(*col).into(),
                    StorageMapping::Composite => {
                        Expr::col(Col::LabelStr).into()
                    }
                }
            }
            ResolvedOperand::Calculation(calc) => {
                let [left, right]: [SimpleExpr; 2] =
                    child_results.try_into().unwrap();
                let is_string =
                    calc.left.is_string_type() && calc.right.is_string_type();
                apply_arithmetic_op(&calc.op, left, right, is_string)
            }
            ResolvedOperand::Aggregation(agg) => agg_expr(agg, agg_ctx),
        }
    })
}

/// オペランド内に含まれるタグのキーをすべて抽出します。
pub(super) fn collect_tag_types(
    operand: &ResolvedOperand,
    keys: &mut Vec<String>,
) {
    for op in operand.walk() {
        match op {
            ResolvedOperand::TagRef {
                storage: StorageMapping::Basic { tag_type, .. },
                ..
            } => keys.push(tag_type.clone()),
            ResolvedOperand::Aggregation(agg) => match agg {
                ResolvedAggregationNode::Count(inner) => {
                    let (storage, _, _) = inner.extract_agg_parts();
                    if let Some(StorageMapping::Basic { tag_type, .. }) =
                        storage
                    {
                        keys.push(tag_type.clone());
                    }
                }
                ResolvedAggregationNode::Arithmetic { inner, .. } => {
                    let (storage, _, _) = inner.extract_agg_parts();
                    if let Some(StorageMapping::Basic { tag_type, .. }) =
                        storage
                    {
                        keys.push(tag_type.clone());
                    }
                }
            },
            _ => {}
        }
    }
}

/// オペランド内に含まれるタグ型を HashSet に収集します。
pub(super) fn collect_tag_types_from_operand(
    operand: &ResolvedOperand,
    set: &mut std::collections::HashSet<String>,
) {
    for op in operand.walk() {
        if let ResolvedOperand::TagRef {
            storage: StorageMapping::Basic { tag_type, .. },
            ..
        } = op
        {
            set.insert(tag_type.clone());
        }
    }
}

// ── nvalue SQL ─────────────────────────────────────────────────────────────

/// Count 集約の nvalue SQL を生成する共通ヘルパー。
pub(super) fn build_count_nvalue_sql(
    src: &Src,
    proj_col: Col,
    proj_tag_type: Option<&str>,
    inner: &ResolvedNode,
    context: Option<&ResolvedNode>,
    item_scope: Option<SelectStatement>,
    include_item_id: bool,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    let (count_col, inner_tag_type) = resolve_count_target(inner);
    let mut stmt = Query::select();

    if let Some(tag_type) = inner_tag_type {
        stmt.expr_as(
            CustomFunc::as_representative(Expr::col((Proj, proj_col))),
            Group,
        );
        stmt.expr_as(Expr::col((Tags, count_col)).count_distinct(), Nvalue);
        stmt.from_as(src, Proj);
        stmt.join_as(
            sea_query::JoinType::InnerJoin,
            Tbl::OneView,
            Tags,
            Expr::col((Proj, Col::ItemId)).equals((Tags, Col::ItemId)),
        );
        if let Some(tt) = proj_tag_type {
            stmt.and_where(Expr::col((Proj, Col::Type)).eq(tt));
        }
        stmt.and_where(Expr::col((Tags, Col::Type)).eq(tag_type));
        if let Some(scope) = item_scope {
            stmt.and_where(Expr::col((Proj, Col::ItemId)).in_subquery(scope));
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
                Expr::col((Proj, Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from_subquery(filter_pick, NvFilter)
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
                Expr::col((Proj, Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from_subquery(context_pick, Ctx)
                        .to_owned(),
                ),
            );
        }
        stmt.group_by_col(Alias::new("Group"));
    } else {
        stmt.expr_as(CustomFunc::as_representative(Expr::col(proj_col)), Group);
        stmt.expr_as(Expr::col(Col::ItemId).count_distinct(), Nvalue);
        stmt.from(src);
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
                    .from_subquery(inner_pick, Sub)
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
                        .from_subquery(context_pick, Ctx)
                        .to_owned(),
                ),
            );
        }
        stmt.group_by_col(Alias::new("Group"));
    }

    if include_item_id {
        wrap_with_item_id(src, stmt, proj_col, proj_tag_type)
    } else {
        stmt
    }
}

/// 同一アイテムの重複を排除した集計用サブクエリを構築します。
pub(super) fn build_unique_agg(
    src: &Src,
    inner: &ResolvedNode,
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
        .expr_as(CustomFunc::any_value(operand_expr), Val)
        .from(src);
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
    src: &Src,
    proj_operand: &ResolvedOperand,
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    include_item_id: bool,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    let (proj_col, proj_tag_type) = match proj_operand {
        ResolvedOperand::TagRef { storage, .. } => match storage {
            StorageMapping::Basic {
                column, tag_type, ..
            } => (*column, Some(tag_type.as_str())),
            StorageMapping::Fixed(col) => (*col, None),
            _ => return SelectStatement::default(),
        },
        _ => return SelectStatement::default(),
    };

    nvalue.fold(&|op, child_results: Vec<SelectStatement>| match op {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            build_count_nvalue_sql(
                src,
                proj_col,
                proj_tag_type,
                inner,
                context,
                None,
                include_item_id,
                agg_ctx,
                nest_ctx,
            )
        }
        ResolvedOperand::Aggregation(
            agg @ ResolvedAggregationNode::Arithmetic { op, inner },
        ) => {
            let is_string = agg.is_string_type();
            let deduped =
                build_unique_agg(src, inner, context, agg_ctx, nest_ctx);

            let mut stmt = Query::select();
            stmt.expr_as(
                CustomFunc::as_representative(Expr::col((Proj, proj_col))),
                Group,
            );
            stmt.expr_as(
                apply_arithmetic_agg(
                    op,
                    Expr::col((Deduped, Val)).into(),
                    is_string,
                ),
                Nvalue,
            );
            stmt.from_as(src, Proj);
            stmt.join_subquery(
                sea_query::JoinType::InnerJoin,
                deduped,
                Deduped,
                Expr::col((Proj, Col::ItemId)).equals((Deduped, Col::ItemId)),
            );
            if let Some(tt) = proj_tag_type {
                stmt.and_where(Expr::col((Proj, Col::Type)).eq(tt));
            }
            stmt.group_by_col(Alias::new("Group"));
            if include_item_id {
                wrap_with_item_id(src, stmt, proj_col, proj_tag_type)
            } else {
                stmt
            }
        }
        ResolvedOperand::Literal(label) => {
            let val = label_to_simple_expr(label);
            let mut stmt = Query::select();
            stmt.expr_as(
                CustomFunc::as_representative(Expr::col(proj_col)),
                Group,
            );
            stmt.expr_as(val, Nvalue);
            stmt.from(src);
            if let Some(tt) = proj_tag_type {
                stmt.and_where(Expr::col(Col::Type).eq(tt));
            }
            if include_item_id {
                stmt.column(Col::ItemId);
                stmt.group_by_col(Alias::new("Group"));
                stmt.group_by_col(Col::ItemId);
            } else {
                stmt.group_by_col(Alias::new("Group"));
            }
            stmt
        }
        ResolvedOperand::Calculation(calc) => {
            let [sub_l, sub_r]: [SelectStatement; 2] =
                child_results.try_into().unwrap();
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            let r_nvalue_expr: SimpleExpr = if is_string {
                Func::coalesce([
                    Expr::col((R, Nvalue)).into(),
                    Expr::val("").into(),
                ])
                .into()
            } else {
                Func::coalesce([
                    Expr::col((R, Nvalue)).into(),
                    Expr::val(0.0f64).into(),
                ])
                .into()
            };
            let mut stmt = Query::select();
            stmt.expr_as(Expr::col((L, Group)), Group);
            stmt.expr_as(
                apply_arithmetic_op(
                    &calc.op,
                    Expr::col((L, Nvalue)).into(),
                    r_nvalue_expr,
                    is_string,
                ),
                Nvalue,
            );
            if include_item_id {
                stmt.column((L, Col::ItemId));
            }
            stmt.from_subquery(sub_l, L);
            if include_item_id {
                stmt.join_subquery(
                    sea_query::JoinType::LeftJoin,
                    sub_r,
                    R,
                    Expr::col((L, Col::ItemId)).eq(Expr::col((R, Col::ItemId))),
                );
            } else {
                stmt.join_subquery(
                    sea_query::JoinType::LeftJoin,
                    sub_r,
                    R,
                    Expr::col((L, Group)).eq(Expr::col((R, Group))),
                );
            }
            let mut outer = Query::select();
            outer.column(Group).column(Nvalue);
            if include_item_id {
                outer.column(Col::ItemId);
            }
            outer.from_subquery(stmt, Sub).to_owned()
        }
        ResolvedOperand::TagRef {
            storage: nval_storage,
            ..
        } => {
            let mut stmt = Query::select();
            stmt.from_as(src, Proj);
            match nval_storage {
                StorageMapping::Fixed(nv_col) => {
                    stmt.expr_as(
                        CustomFunc::as_representative(Expr::col((
                            Proj, proj_col,
                        ))),
                        Group,
                    );
                    stmt.expr_as(
                        CustomFunc::any_value(Expr::col((Proj, *nv_col))),
                        Nvalue,
                    );
                }
                StorageMapping::Basic {
                    column: nv_col,
                    tag_type: nv_tag_type,
                } => {
                    let nv_sub = Query::select()
                        .column(Col::ItemId)
                        .expr_as(
                            CustomFunc::try_cast_double(Expr::col(*nv_col)),
                            Val,
                        )
                        .from(src)
                        .and_where(
                            Expr::col(Col::Type).eq(nv_tag_type.as_str()),
                        )
                        .to_owned();
                    stmt.join_subquery(
                        sea_query::JoinType::LeftJoin,
                        nv_sub,
                        Sub,
                        Expr::col((Proj, Col::ItemId))
                            .equals((Sub, Col::ItemId)),
                    );

                    stmt.expr_as(
                        CustomFunc::as_representative(Expr::col((
                            Proj, proj_col,
                        ))),
                        Group,
                    );
                    stmt.expr_as(
                        CustomFunc::any_value(Func::coalesce([
                            Expr::col((Sub, Val)).into(),
                            Expr::val(0.0f64).into(),
                        ])),
                        Nvalue,
                    );
                }
                StorageMapping::Composite => {
                    stmt.expr_as(
                        CustomFunc::as_representative(Expr::col((
                            Proj, proj_col,
                        ))),
                        Group,
                    );
                    stmt.expr_as(Expr::val(0.0f64), Nvalue);
                }
            }
            if let Some(tt) = proj_tag_type {
                stmt.and_where(Expr::col((Proj, Col::Type)).eq(tt));
            }
            stmt.group_by_col(Alias::new("Group"));
            if include_item_id {
                wrap_with_item_id(src, stmt, proj_col, proj_tag_type)
            } else {
                stmt
            }
        }
    })
}

/// 集約 SQL を生成します（コンテキストなし）。
pub(super) fn build_agg(
    src: &Src,
    agg: &ResolvedAggregationNode,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    build_agg_inner(src, agg, agg_ctx, None)
}

/// 集約 SQL を生成します（Nest コンテキストあり）。
pub(super) fn build_agg_nest(
    src: &Src,
    agg: &ResolvedAggregationNode,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    build_agg_inner(src, agg, agg_ctx, Some(nest_ctx))
}

fn build_agg_inner(
    src: &Src,
    agg: &ResolvedAggregationNode,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    if let Some(nvalue_agg_sql) =
        build_agg_over_nvalue(src, agg, agg_ctx, nest_ctx)
    {
        return nvalue_agg_sql;
    }
    let mut stmt = Query::select();
    match agg {
        ResolvedAggregationNode::Count(inner) => {
            stmt.from(src);
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
                sub.column(Col::ItemId).from_subquery(pick_sql, Sub);
                final_cond =
                    final_cond.add(Expr::col(Col::ItemId).in_subquery(sub));
            }
            stmt.expr_as(Expr::col(count_col).count_distinct(), Scalar);
            stmt.cond_where(final_cond);
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let is_string = agg.is_string_type();
            let sub = build_unique_agg(src, inner, None, agg_ctx, nest_ctx);
            stmt.expr_as(
                apply_arithmetic_agg(op, Expr::col(Val).into(), is_string),
                Scalar,
            );
            stmt.from_subquery(sub, Deduped);
        }
    }
    stmt
}

fn build_agg_over_nvalue(
    src: &Src,
    agg: &ResolvedAggregationNode,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> Option<SelectStatement> {
    let (outer_agg_op, inner) = match agg {
        ResolvedAggregationNode::Count(inner) => (None, inner.as_ref()),
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            (Some(op), inner.as_ref())
        }
    };
    let proj_operand = inner.get_projection_operands()?;
    let nvalue = inner.get_nvalue()?;
    let context = inner.get_agg_context();
    let nvalue_condition = inner.get_nvalue_condition();

    let source = if proj_operand.len() > 1 {
        let pivot_agg = build_nvalue_pivot_aggregate_sql(
            src,
            proj_operand,
            nvalue,
            context,
            agg_ctx,
            nest_ctx,
        );
        if let Some((op, value)) = nvalue_condition {
            let bin_op = to_bin_op(*op);
            let val = label_to_simple_expr(value);
            Query::select()
                .column(Nvalue)
                .from_subquery(pivot_agg, Sub)
                .and_where(Expr::col(Nvalue).binary(bin_op, val))
                .to_owned()
        } else {
            pivot_agg
        }
    } else {
        let mut nvalue_sub = build_nvalue_standalone_subquery(
            src,
            &proj_operand[0],
            nvalue,
            context,
            true,
            agg_ctx,
            nest_ctx,
        );
        if let Some((op, value)) = nvalue_condition {
            let bin_op = to_bin_op(*op);
            let val = label_to_simple_expr(value);
            nvalue_sub.and_where(Expr::col(Nvalue).binary(bin_op, val));
        }
        Query::select()
            .column(Group)
            .column(Nvalue)
            .from_subquery(nvalue_sub, Sub)
            .group_by_col(Group)
            .group_by_col(Nvalue)
            .to_owned()
    };

    let mut stmt = Query::select();
    match outer_agg_op {
        None => {
            stmt.expr_as(CustomFunc::count_star(), Scalar);
        }
        Some(op) => {
            let is_string = nvalue.is_string_type();
            stmt.expr_as(
                apply_arithmetic_agg(op, Expr::col(Nvalue).into(), is_string),
                Scalar,
            );
        }
    }
    stmt.from_subquery(source, Sub);
    Some(stmt)
}

/// nvalue集計用CTE（picked_ids 参照版）を構築します（コンテキストなし）。
pub(super) fn build_nvalue_cte(
    src: &Src,
    proj_operands: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    build_nvalue_cte_inner(src, proj_operands, nvalue, context, agg_ctx, None)
}

/// nvalue集計用CTE（picked_ids 参照版）を構築します（Nest コンテキストあり）。
pub(super) fn build_nvalue_cte_nest(
    src: &Src,
    proj_operands: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    agg_ctx: &AggregationContext,
    nest_ctx: &NestContext,
) -> SelectStatement {
    build_nvalue_cte_inner(
        src,
        proj_operands,
        nvalue,
        context,
        agg_ctx,
        Some(nest_ctx),
    )
}

fn build_nvalue_cte_inner(
    src: &Src,
    proj_operands: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    if proj_operands.len() > 1 {
        return build_nvalue_pivot_aggregate_sql(
            src,
            proj_operands,
            nvalue,
            context,
            agg_ctx,
            nest_ctx,
        );
    }
    let proj_operand = &proj_operands[0];
    let (proj_col, proj_storage) = match proj_operand {
        ResolvedOperand::TagRef { storage, .. } => match storage {
            StorageMapping::Basic { column, .. } => (*column, storage),
            StorageMapping::Fixed(col) => (*col, storage),
            _ => return SelectStatement::default(),
        },
        _ => return SelectStatement::default(),
    };
    let proj_tag_type =
        if let StorageMapping::Basic { tag_type, .. } = &proj_storage {
            Some(tag_type.as_str())
        } else {
            None
        };

    let inner_q = match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            build_count_nvalue_sql(
                src,
                proj_col,
                proj_tag_type,
                inner,
                context,
                Some(
                    Query::select()
                        .column(Col::ItemId)
                        .from(PickedIds)
                        .to_owned(),
                ),
                true,
                agg_ctx,
                nest_ctx,
            )
        }
        ResolvedOperand::Aggregation(
            agg @ ResolvedAggregationNode::Arithmetic { op, inner },
        ) => {
            let is_string = agg.is_string_type();
            let deduped =
                build_unique_agg(src, inner, context, agg_ctx, nest_ctx);
            let mut stmt = Query::select();
            stmt.expr_as(
                CustomFunc::as_representative(Expr::col((Proj, proj_col))),
                Group,
            );
            stmt.expr_as(
                apply_arithmetic_agg(
                    op,
                    Expr::col((Deduped, Val)).into(),
                    is_string,
                ),
                Nvalue,
            );
            stmt.from_as(src, Proj);
            stmt.join_subquery(
                sea_query::JoinType::InnerJoin,
                deduped,
                Deduped,
                Expr::col((Proj, Col::ItemId)).equals((Deduped, Col::ItemId)),
            );
            stmt.and_where(
                Expr::col((Proj, Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from(PickedIds)
                        .to_owned(),
                ),
            );
            if let Some(tt) = proj_tag_type {
                stmt.and_where(Expr::col((Proj, Col::Type)).eq(tt));
            }
            stmt.group_by_col((Proj, proj_col));
            wrap_with_item_id(src, stmt, proj_col, proj_tag_type)
        }
        _ => build_nvalue_standalone_subquery(
            src,
            proj_operand,
            nvalue,
            context,
            true,
            agg_ctx,
            nest_ctx,
        ),
    };

    Query::select()
        .column(Group)
        .column(Nvalue)
        .from_subquery(inner_q, Sub)
        .group_by_col(Group)
        .group_by_col(Nvalue)
        .to_owned()
}

// ── Nest / Pivot SQL ───────────────────────────────────────────────────────

/// 多キー Nest の Pivot CTE を構築します。
/// Pivot CTE の stmt 初期化とキーループ処理を共通化するヘルパー。
/// Calculation キーの式生成だけをクロージャで差し替え可能にする。
/// 戻り値: (構築途中の stmt, type_filters)
fn build_pivot_keys_into_stmt(
    src: &Src,
    keys: &[ResolvedOperand],
    calc_expr_fn: impl Fn(
        &crate::query::lens_resolver::ResolvedCalculationNode,
    ) -> SimpleExpr,
) -> (SelectStatement, std::collections::HashSet<String>) {
    let mut stmt = Query::select();
    stmt.column(Col::ItemId);
    stmt.expr_as(CustomFunc::any_value(Expr::col(Col::Rank)), Col::Rank);
    stmt.expr_as(
        CustomFunc::any_value(Expr::col(Col::ItemKind)),
        Col::ItemKind,
    );
    stmt.from(src);

    let mut type_filters = std::collections::HashSet::new();

    let union_type = "UNION(v VARCHAR, i BIGINT, d DOUBLE, b BOOLEAN, u UUID)";
    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef { storage, .. } => match storage {
                StorageMapping::Basic { tag_type, column } => {
                    type_filters.insert(tag_type.as_str().to_string());
                    let case_expr = Expr::case(
                        Expr::col(Col::Type).eq(tag_type.as_str()),
                        Expr::col(*column),
                    );
                    let max_expr: SimpleExpr = Func::max(case_expr).into();
                    stmt.expr_as(
                        CustomFunc::as_representative(max_expr.clone()),
                        Alias::new(&format!("key{}", i)),
                    );
                    stmt.and_having(max_expr.is_not_null());
                }
                StorageMapping::Fixed(col) => {
                    let max_expr = Expr::col(*col).max();
                    stmt.expr_as(
                        CustomFunc::as_representative(max_expr.clone()),
                        Alias::new(&format!("key{}", i)),
                    );
                    stmt.and_having(max_expr.is_not_null());
                }
                StorageMapping::Composite => {
                    // Representative は既にリストなのでそのまま（または UNION[] キャスト）
                    stmt.expr_as(
                        Expr::cust(format!(
                            "CAST(\"{}\" AS {}[])",
                            sea_query::Iden::to_string(
                                &crate::db::Pronoun::Representative
                            ),
                            union_type
                        )),
                        Alias::new(&format!("key{}", i)),
                    );
                    // NULLチェック
                    stmt.and_having(
                        Expr::col(crate::db::Pronoun::Representative)
                            .is_not_null(),
                    );
                }
            },
            ResolvedOperand::Calculation(calc) => {
                collect_tag_types_from_operand(&calc.left, &mut type_filters);
                collect_tag_types_from_operand(&calc.right, &mut type_filters);
                let calc_expr = calc_expr_fn(calc);
                stmt.expr_as(
                    CustomFunc::as_representative(calc_expr.clone()),
                    Alias::new(&format!("key{}", i)),
                );
                stmt.and_having(calc_expr.is_not_null());
            }
            _ => {}
        }
    }

    (stmt, type_filters)
}

/// 集約あり多キー Nest 用の Pivot CTE を構築します。
pub(super) fn build_nest_pivot_cte(
    src: &Src,
    keys: &[ResolvedOperand],
    nvalue: Option<&ResolvedOperand>,
    agg_ctx: &AggregationContext,
) -> SelectStatement {
    let (mut stmt, type_filters) =
        build_pivot_keys_into_stmt(src, keys, |calc| {
            build_agg_calc_eav_expr(calc, agg_ctx)
        });

    if let Some(nv) = nvalue {
        let nv_expr = build_agg_operand_eav_expr(nv, agg_ctx);
        stmt.expr_as(nv_expr, Nvalue);
    } else if !type_filters.is_empty() {
        stmt.and_where(Expr::col(Col::Type).is_in(type_filters));
    }

    stmt.group_by_col(Col::ItemId);
    stmt
}

/// 集約なし多キー Nest 用の Pivot CTE を構築します。
/// `agg_ctx` 不要。Calculation キーには `build_calculation_eav_expr` を使用します。
pub(super) fn build_nest_pivot_cte_no_agg(
    src: &Src,
    keys: &[ResolvedOperand],
) -> SelectStatement {
    let (mut stmt, type_filters) =
        build_pivot_keys_into_stmt(src, keys, |calc| {
            build_calculation_eav_expr(calc)
        });

    if !type_filters.is_empty() {
        stmt.and_where(Expr::col(Col::Type).is_in(type_filters));
    }

    stmt.group_by_col(Col::ItemId);
    stmt
}

/// 多キー Nest の nvalue Pivot 集計 SQL を構築します。
pub(super) fn build_nvalue_pivot_aggregate_sql(
    src: &Src,
    keys: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    if let ResolvedOperand::Calculation(calc) = nvalue {
        return build_mixed_key_calc_nvalue_sql(
            src, keys, calc, context, agg_ctx, nest_ctx,
        );
    }

    let pivot_q = build_nest_pivot_cte(src, keys, Some(nvalue), agg_ctx);
    let mut stmt = Query::select();
    for i in 0..keys.len() {
        stmt.column(Alias::new(&format!("key{}", i)));
    }

    let is_string = nvalue.is_string_type();
    let nval_expr = match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_)) => {
            Expr::col(Nvalue).count().into()
        }
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Arithmetic {
            op,
            ..
        }) => apply_arithmetic_agg(op, Expr::col(Nvalue).into(), is_string),
        _ => CustomFunc::any_value(Expr::col(Nvalue)).into(),
    };

    stmt.expr_as(nval_expr, Nvalue);
    stmt.from_subquery(pivot_q, Pivot);
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
                Ctx,
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
    src: &Src,
    keys: &[ResolvedOperand],
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    context: Option<&ResolvedNode>,
    agg_ctx: &AggregationContext,
    nest_ctx: Option<&NestContext>,
) -> SelectStatement {
    let n_left = count_nvalue_keys(&calc.left).max(1).min(keys.len() - 1);
    let left_sub = build_nvalue_standalone_subquery(
        src, &keys[0], &calc.left, context, false, agg_ctx, nest_ctx,
    );
    let right_sub = build_nvalue_standalone_subquery(
        src,
        &keys[n_left],
        &calc.right,
        context,
        false,
        agg_ctx,
        nest_ctx,
    );
    let pivot_sub = build_nest_pivot_cte(src, keys, None, agg_ctx);

    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    let l_nvalue: SimpleExpr = Func::coalesce([
        Expr::col((L, Nvalue)).into(),
        if is_string {
            Expr::val("").into()
        } else {
            Expr::val(0.0f64).into()
        },
    ])
    .into();
    let r_nvalue: SimpleExpr = Func::coalesce([
        Expr::col((R, Nvalue)).into(),
        if is_string {
            Expr::val("").into()
        } else {
            Expr::val(0.0f64).into()
        },
    ])
    .into();

    let mut stmt = Query::select();
    stmt.distinct();
    for i in 0..keys.len() {
        stmt.expr_as(
            Expr::col((Pivot, Alias::new(&format!("key{}", i)))),
            Alias::new(&format!("key{}", i)),
        );
    }
    stmt.expr_as(
        apply_arithmetic_op(&calc.op, l_nvalue, r_nvalue, is_string),
        Nvalue,
    );
    stmt.from_subquery(pivot_sub, Pivot);
    stmt.join_subquery(
        sea_query::JoinType::LeftJoin,
        left_sub,
        L,
        Expr::col((Pivot, Alias::new("key0"))).equals((L, Group)),
    );
    stmt.join_subquery(
        sea_query::JoinType::LeftJoin,
        right_sub,
        R,
        Expr::col((Pivot, Alias::new(&format!("key{}", n_left))))
            .equals((R, Group)),
    );
    for i in 0..keys.len() {
        stmt.and_where(
            Expr::col((Pivot, Alias::new(&format!("key{}", i)))).is_not_null(),
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
                Ctx,
            )
            .to_owned();
        stmt.and_where(Expr::col((Pivot, Col::ItemId)).in_subquery(ctx_sub));
    }
    stmt
}
