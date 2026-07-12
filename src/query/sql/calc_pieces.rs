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
    apply_arithmetic_op, build_resolved_literal_expr,
    build_storage_column_expr, build_tag_value_agg_expr,
};
use crate::query::lens_resolver::{ResolvedCalculationNode, ResolvedOperand};
use sea_query::SimpleExpr;

/// 集約を含まない純粋な算術演算ノードをカラム参照式に変換します（agg_ctx 不要）。
pub(super) fn build_calculation_expr(
    calc: &ResolvedCalculationNode,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_expr(&calc.left);
    let right_expr = build_resolved_operand_expr(&calc.right);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
}

fn build_resolved_operand_expr(operand: &ResolvedOperand) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef { storage, bitical_type, .. } => {
            build_storage_column_expr(storage, *bitical_type)
        }
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] = child_results.try_into().unwrap();
            let is_string = calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(_) => {
            panic!("build_calculation_expr called with aggregation operand; use build_agg_calc_expr instead")
        }
    })
}

/// 集約を含まない純粋な EAV 算術演算ノードを集約式に変換します（agg_ctx 不要）。
pub(super) fn build_calculation_eav_expr(
    calc: &ResolvedCalculationNode,
) -> SimpleExpr {
    let left = build_resolved_operand_eav_expr(&calc.left);
    let right = build_resolved_operand_eav_expr(&calc.right);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left, right, is_string)
}

fn build_resolved_operand_eav_expr(operand: &ResolvedOperand) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef { storage, bitical_type, .. } => {
            build_tag_value_agg_expr(storage, *bitical_type)
        }
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] = child_results.try_into().unwrap();
            let is_string = calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(_) => {
            panic!("build_calculation_eav_expr called with aggregation operand; use build_agg_calc_eav_expr instead")
        }
    })
}

/// Literal / TagRef / Calculation の各アームを処理します。
/// Aggregation は呼び出し元が個別に処理するため None を返します。
pub(super) fn fold_simple_operand(
    op: &ResolvedOperand,
    child_results: Vec<SimpleExpr>,
) -> Option<SimpleExpr> {
    match op {
        ResolvedOperand::Literal(lab) => {
            let expr =
                if let Some(bytes) = crate::util::parse_size(&lab.as_str()) {
                    sea_query::Expr::val(bytes)
                        .cast_as(crate::db::BiticalType::Double)
                        .into()
                } else {
                    match lab.value() {
                        crate::types::Bitical::Integer(i) => {
                            sea_query::Expr::val(i)
                                .cast_as(crate::db::BiticalType::Double)
                                .into()
                        }
                        other => other.to_simple_expr(),
                    }
                };
            Some(expr)
        }
        ResolvedOperand::TagRef { .. } => Some(sea_query::Expr::val(0).into()),
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] =
                child_results.try_into().unwrap();
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            Some(apply_arithmetic_op(&calc.op, left, right, is_string))
        }
        ResolvedOperand::Aggregation(_) => None,
    }
}
