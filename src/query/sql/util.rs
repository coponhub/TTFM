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

use crate::db::{BiticalType, Col, CustomFunc, Pronoun::*};
use crate::query::ast::{ArithmeticAggOp, ArithmeticOp, QueryNode};
use crate::query::lens_resolver::ResolvedNode;
use crate::query::lens_schema::StorageMapping;
use crate::types::Label;
use sea_query::{
    BinOper, Condition, Expr, Func, Query, SelectStatement, SimpleExpr,
};
use std::collections::HashMap;

/// `SelectStatement` をインライン副問合せ式 (`SimpleExpr`) に変換します。
pub(super) fn subquery(stmt: SelectStatement) -> SimpleExpr {
    SimpleExpr::SubQuery(None, Box::new(stmt.into_sub_query_statement()))
}

/// SQL を `SELECT item_id FROM (sql) AS sub` でラップします。
pub(super) fn wrap_to_item_ids(sql: SelectStatement) -> SelectStatement {
    Query::select()
        .column(Col::ItemId)
        .from_subquery(sql, Sub)
        .to_owned()
}

/// SQL を `SELECT item_id, rank, item_kind FROM (sql) AS sub` でラップします。
/// UNION / INTERSECT / EXCEPT で子クエリを結合する際に使用します。
pub(super) fn wrap_in_subquery(q: SelectStatement) -> SelectStatement {
    Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from_subquery(q, Sub)
        .to_owned()
}

/// SQL を `SELECT * FROM (sql) AS sub` でラップします。
/// fetch 行スキーマ（全カラム）のまま UNION で結合する際に使用します。
pub(super) fn wrap_in_subquery_star(q: SelectStatement) -> SelectStatement {
    Query::select()
        .column(sea_query::Asterisk)
        .from_subquery(q, Sub)
        .to_owned()
}

/// `Label` の値を単純な SQL 式に変換します。
pub(super) fn label_to_simple_expr(label: &Label) -> SimpleExpr {
    label.resolved_value().to_simple_expr()
}

/// リテラルラベルを SQL 式に変換します。数値リテラルは DOUBLE へキャストします。
pub(super) fn build_resolved_literal_expr(lab: &Label) -> SimpleExpr {
    use crate::types::Bitical;
    match lab.resolved_value() {
        Bitical::Integer(i) => Expr::val(i).cast_as(BiticalType::Double).into(),
        other => other.to_simple_expr(),
    }
}

/// nvalue 比較の右辺制約（`NvalueRhs`）を Condition へ変換する共有ヘルパ。
/// Label は `is_string` に応じて数値/文字列で描画し、文字列 × glob は
/// Eq → GLOB / Ne → NOT GLOB（`check_string_match` と同じ規則）。
/// DateTime は区間/スロット条件（`date_time_condition_expr` に op 行列を集約）。
pub(super) fn nvalue_rhs_condition(
    lhs: SimpleExpr,
    op: crate::query::ast::ComparisonOp,
    rhs: &crate::query::lens_resolver::NvalueRhs,
    is_string: bool,
) -> Condition {
    use crate::query::ast::{BasicOp, ComparisonOp};
    use crate::query::lens_resolver::NvalueRhs;

    let basic_op = match op {
        ComparisonOp::Scalar(b) | ComparisonOp::Label(b) => b,
    };
    match rhs {
        NvalueRhs::DateTime(range) => {
            super::date_time_condition_expr(&lhs, basic_op, range)
        }
        NvalueRhs::Label(label) => {
            let bin_op = crate::query::lens_schema::to_bin_op(op);
            if is_string {
                let s = label.as_str();
                if crate::util::is_glob_pattern(&s) {
                    let glob = Expr::expr(lhs)
                        .binary(BinOper::Custom("GLOB"), Expr::val(s));
                    let expr =
                        if basic_op == BasicOp::Ne { glob.not() } else { glob };
                    Condition::all().add(expr)
                } else {
                    Condition::all().add(Expr::expr(lhs).binary(bin_op, Expr::val(s)))
                }
            } else {
                let rhs_expr = label_to_simple_expr(label);
                Condition::all().add(Expr::expr(lhs).binary(bin_op, rhs_expr))
            }
        }
    }
}

/// `ArithmeticAggOp` を集約 SQL 式に適用します。
pub(super) fn apply_arithmetic_agg(
    op: &ArithmeticAggOp,
    expr: SimpleExpr,
    is_string: bool,
) -> SimpleExpr {
    use ArithmeticAggOp::*;
    match op {
        Sum => {
            if is_string {
                // 文字列の合計はカンマ区切り結合 (DuckDB: string_agg)
                CustomFunc::string_agg(expr, Expr::val(", "))
            } else {
                Func::sum(expr).into()
            }
        }
        Avg => Func::avg(expr).into(),
        Max => Func::max(expr).into(),
        Min => Func::min(expr).into(),
    }
}

pub(super) fn build_storage_column_expr(
    storage: &StorageMapping,
    bitical_type: BiticalType,
) -> SimpleExpr {
    match storage {
        StorageMapping::Fixed(col) => Expr::col(*col).into(),
        StorageMapping::Basic { column, .. } if *column == Col::LabelStr => {
            match bitical_type {
                BiticalType::Integer => {
                    Expr::col(Col::LabelInt).cast_as(BiticalType::Double).into()
                }
                BiticalType::Double => Expr::col(Col::LabelDouble).into(),
                _ => coalesce_label_columns_as_string(),
            }
        }
        StorageMapping::Basic { column, .. } => Expr::col(*column).into(),
        StorageMapping::Composite => coalesce_label_columns_as_string(),
    }
}

pub(super) fn coalesce_label_columns_as_string() -> SimpleExpr {
    Func::coalesce([
        Expr::col(Col::LabelStr).into(),
        Expr::col(Col::LabelInt).cast_as(BiticalType::String).into(),
        Expr::col(Col::LabelDouble).cast_as(BiticalType::String).into(),
        sea_query::CaseStatement::new()
            .case(Expr::col(Col::LabelBool).eq(true), "true")
            .case(Expr::col(Col::LabelBool).eq(false), "false")
            .into(),
    ])
    .into()
}

// ── AggregationContext / NestContext ───────────────────────────────────────

/// 集約ノードのフィルタ/内部SQL を事前計算した結果を保持します。
///
/// `build_pick_sql` に渡すことで fold 内での再帰呼び出しを排除します。
/// `needs_aggregation_context` で必要性を判定し、`build_aggregation_context` で構築します。
pub struct AggregationContext {
    /// inner_node ポインタ → フィルタ SQL (Phase 3 で参照)
    pub agg_filters: HashMap<usize, SelectStatement>,
    /// Count(inner) かつ inner_tag_type が None の場合の inner 全体 SQL (Phase 3 で参照)
    pub agg_inner_sqls: HashMap<usize, SelectStatement>,
    /// Count(inner) が定義枝 (DefinitionRef, Or 経由も含む) を含む場合の
    /// 定義枝 distinct 件数 SQL。キーは inner_node ポインタ（Phase 3 で参照）。
    pub definition_counts: HashMap<usize, SelectStatement>,
    /// Phase 1 収集: フィルタノード (Phase 2 で SQL 化して agg_filters へ移動)
    pub filter_nodes: HashMap<usize, ResolvedNode>,
    /// Phase 1 収集: Count inner ノード (Phase 2 で SQL 化して agg_inner_sqls へ移動)
    pub inner_nodes: HashMap<usize, ResolvedNode>,
    /// Phase 1 収集: 定義枝ノード群 (Phase 2 で SQL 化して definition_counts へ移動)
    pub definition_count_nodes: HashMap<usize, Vec<ResolvedNode>>,
}

impl AggregationContext {
    pub fn new() -> Self {
        Self {
            agg_filters: HashMap::new(),
            agg_inner_sqls: HashMap::new(),
            definition_counts: HashMap::new(),
            filter_nodes: HashMap::new(),
            inner_nodes: HashMap::new(),
            definition_count_nodes: HashMap::new(),
        }
    }
}

/// Nest コンテキストノードの SQL を事前計算した結果を保持します。
///
/// `build_pick_sql` に渡すことで fold 内での再帰呼び出しを排除します。
/// `needs_nest_context` で必要性を判定し、`build_nest_context` で構築します。
pub struct NestContext {
    /// コンテキスト ResolvedNode ポインタ → コンテキスト SQL (Phase 3 で参照)
    pub contexts: HashMap<usize, SelectStatement>,
    /// Phase 1 収集: コンテキストノード (Phase 2 で SQL 化して contexts へ移動)
    pub context_nodes: HashMap<usize, ResolvedNode>,
}

impl NestContext {
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
            context_nodes: HashMap::new(),
        }
    }
}

/// StorageMapping から集計式を生成します（EAV 構造用の MAX CASE WHEN 形式）。
pub(super) fn build_tag_value_agg_expr(
    storage: &StorageMapping,
    _sql_type: BiticalType,
) -> SimpleExpr {
    match storage {
        StorageMapping::Fixed(col) => {
            CustomFunc::any_value(Expr::col(*col)).into()
        }
        StorageMapping::Basic { column, tag_type } => {
            let value_expr = if *column == Col::LabelStr {
                coalesce_label_columns_as_string()
            } else {
                Expr::col(*column).into()
            };
            let cast_expr = CustomFunc::try_cast_double(value_expr);
            let case_expr = Expr::case(
                Expr::col(Col::Type).eq(tag_type.as_str()),
                cast_expr,
            );
            Func::max(case_expr).into()
        }
        StorageMapping::Composite => CustomFunc::any_value(Expr::val(0)).into(),
    }
}

/// クエリに使用されている型のリストを元に OneView から特定のタグ行のみを抽出する Condition を生成します。
pub fn to_tag_condition(node: &QueryNode) -> sea_query::Condition {
    let mut types = node.get_all_types();
    if types.iter().any(|t| t == "*") {
        return sea_query::Condition::all();
    }
    let defaults = [
        "name",
        "path",
        "size",
        "mtime",
        "rank",
        "item_kind",
        "content",
        "value",
        "tag",
        "filename",
        "is_dir",
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
        if crate::util::is_glob_pattern(&t) {
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

/// `ArithmeticOp` を二項演算 SQL 式に適用します。
pub(super) fn apply_arithmetic_op(
    op: &ArithmeticOp,
    left: SimpleExpr,
    right: SimpleExpr,
    is_string: bool,
) -> SimpleExpr {
    use ArithmeticOp::*;
    if is_string {
        return match op {
            Add => Expr::expr(left)
                .binary(BinOper::Custom("||"), Expr::val(", "))
                .binary(BinOper::Custom("||"), right),
            Mul => Expr::expr(left).binary(BinOper::Custom("||"), right),
            _ => Expr::expr(left).binary(BinOper::Custom("||"), right),
        };
    }
    let bin_op = match op {
        Add => BinOper::Add,
        Sub => BinOper::Sub,
        Mul => BinOper::Mul,
        Div => BinOper::Div,
        Mod => BinOper::Custom("%"),
    };
    Expr::expr(left).binary(bin_op, right)
}
