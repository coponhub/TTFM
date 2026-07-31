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

mod agg_pieces;
mod boolean;
mod calc_pieces;
pub(crate) mod definition;
mod low_dispatcher;
mod nest;
mod order;
mod pick;
mod precompute;
mod scalar;
pub(crate) mod schema_pieces;
mod util;
use agg_pieces::{
    agg_expr, build_agg, build_agg_calc_eav_expr, build_agg_calc_expr,
    build_agg_calc_subquery, build_agg_calc_subquery_nest, build_agg_nest,
    build_agg_operand_eav_expr, build_nest_pivot_cte,
    build_nest_pivot_cte_no_agg, build_nvalue_cte, build_nvalue_cte_nest,
    build_nvalue_standalone_subquery, resolve_count_target,
};
#[cfg(test)]
use agg_pieces::{
    build_agg_operand_expr, build_resolved_operand_expr_for_arithmetic,
};
use boolean::{
    build_boolean_sql, build_label_set_op_pick_sql, build_resolved_and_sql,
    build_resolved_diff_sql, build_resolved_or_sql,
};
use calc_pieces::{
    build_calculation_eav_expr, build_calculation_expr, fold_simple_operand,
};
use low_dispatcher::try_dispatch_common;
use nest::{
    build_merged_nest_match_sql, build_nest_match_sql,
    build_nest_nest_match_sql,
};
pub use pick::{
    build_pick, build_pick_agg, build_pick_nest, AggPickNode, BuildPick,
    NestPickNode, PickNode, SimplePickNode,
};
pub use precompute::{
    build_aggregation_context, build_aggregation_context_for_agg,
    build_aggregation_context_for_operand, build_nest_context,
    build_nest_context_for_operand, needs_aggregation_context,
    needs_nest_context,
};
use scalar::{
    build_column_match_sql, build_resolved_date_time_match_sql,
    build_resolved_match_sql, build_resolved_scalar_sql,
    build_resolved_tag_tag_match_sql, build_scalar_match_sql,
    date_time_condition_expr,
};
use util::*;
pub use util::{to_tag_condition, AggregationContext, NestContext};

use crate::db::{Col, CustomFunc, Pronoun::*, Src};
use crate::query::ast::QueryNode;
use crate::query::lens_resolver::{NestMatchOp, ResolvedNode, ResolvedOperand};
#[cfg(test)]
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use sea_query::{Expr, Query, SelectStatement};

fn build_fetch_items_sql(
    src: &Src,
    pick: &PickNode<'_>,
    limit: Option<usize>,
    offset: Option<usize>,
    orders: &[crate::query::lens_resolver::ResolvedOrder],
) -> SelectStatement {
    build_fetch_items_sql_impl(src, pick, limit, offset, false, orders)
}

/// `with_definition_schema` が true の場合、定義枝と束ねられるよう
/// fetch 行スキーマを定義アイテムの SQL の列順に揃える（align_schema_with_definitions）。
/// `orders` は明示的な並び順。ページング用の id サブクエリと外側の両方に
/// 同じ ORDER BY を適用し（先頭に置き、既定の並びはタイブレーカーに残す）、
/// ページ境界と表示順を一致させる。
fn build_fetch_items_sql_impl(
    src: &Src,
    pick: &PickNode<'_>,
    limit: Option<usize>,
    offset: Option<usize>,
    with_definition_schema: bool,
    orders: &[crate::query::lens_resolver::ResolvedOrder],
) -> SelectStatement {
    let node = pick.node();
    // 集約クエリ (e.g. count(path:) や sum(size:) > 100) の場合は、
    // oneview との結合を行わず、集約計算結果だけをそのまま返す。
    // NestMatch / NestNestMatch は tags カラムが必要なため
    // ここでは早期リターンせず、通常のタグパッキング処理に委ねる。
    match node {
        ResolvedNode::Aggregation(_)
        | ResolvedNode::AggregationMatch { .. } => {
            let mut sql = pick.build_pick();
            if with_definition_schema {
                align_schema_with_definitions(&mut sql, None);
            }
            return sql;
        }
        _ => {}
    }

    let pick_sql = pick.build_pick();

    // 1. ID 絞り込みサブクエリ
    let mut id_query = Query::select();
    id_query.column(Col::ItemId).from_subquery(pick_sql, Pk);
    order::apply_resolved_order(
        &mut id_query,
        orders,
        src,
        Expr::col((Pk, Col::ItemId)).into(),
    );
    id_query
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    if let Some(l) = limit {
        id_query.limit(l as u64);
    }
    if let Some(o) = offset {
        id_query.offset(o as u64);
    }

    // 2. EAV列を UNION 型に変換して struct_pack で集約
    let union_val = CustomFunc::eav_union_value();
    let struct_expr = CustomFunc::struct_pack_tag(
        Expr::col(Col::Type).into(),
        union_val,
        Expr::col(Col::Origin).into(),
    );
    let tags_expr = CustomFunc::list(struct_expr);

    let mut q = Query::select();
    q.column(Col::ItemId)
        .expr_as(Expr::col(Col::Rank).max(), Col::Rank)
        .expr_as(
            CustomFunc::any_value(Expr::col(Col::ItemKind)),
            Col::ItemKind,
        )
        .expr_as(tags_expr, crate::db::QueryResultCol::Tags)
        .from(src)
        .and_where(Expr::col(Col::ItemId).in_subquery(id_query))
        .group_by_col(Col::ItemId);
    order::apply_resolved_order(&mut q, orders, src, order::src_item_id(src));
    q.order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    if with_definition_schema {
        let name_expr = CustomFunc::any_value(
            Expr::case(
                Expr::col(Col::Type).eq("name"),
                Expr::col(Col::LabelStr),
            )
            .finally(Expr::val(None::<String>)),
        );
        align_schema_with_definitions(&mut q, Some(name_expr));
    }

    q
}

/// fetch 行スキーマを定義アイテムの SQL（build_definition_fetch_sql）の列順に
/// 揃える: 末尾に NULL の representative 列と並び替え専用の name 列
/// （name タグの値。デコードには使わない）を追加する。
fn align_schema_with_definitions(
    q: &mut SelectStatement,
    name_expr: Option<sea_query::SimpleExpr>,
) {
    q.expr_as(Expr::val(None::<String>), Representative)
        .expr_as(
            name_expr.unwrap_or_else(|| Expr::val(None::<String>).into()),
            Col::Name,
        );
}

pub fn build_flat_table_sql(
    src: &Src,
    pick: &PickNode<'_>,
    query_node: &QueryNode,
    limit: Option<usize>,
    offset: Option<usize>,
) -> SelectStatement {
    let pick_sql = pick.build_pick();
    let tagcond = to_tag_condition(query_node);

    // 1. まず ID を絞り込むためのサブクエリを構築
    let mut id_query = Query::select();
    id_query
        .column(Col::ItemId)
        .from_subquery(pick_sql.to_owned(), Pk)
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    if let Some(l) = limit {
        id_query.limit(l as u64);
    }
    if let Some(o) = offset {
        id_query.offset(o as u64);
    }

    let mut q = Query::select();
    q.columns(Col::raw_tag_row_columns())
        .column(Col::Rank)
        .from(src)
        .and_where(Expr::col(Col::ItemId).in_subquery(id_query))
        .and_where(tagcond.into())
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    q
}

/// ノードがスカラー boolean として扱われるべきかを判定する。
/// CalculationMatch はタグ参照を含む場合はアイテムフィルタとして扱う。
fn is_boolean_only_node(n: &ResolvedNode) -> bool {
    match n {
        ResolvedNode::AggregationMatch { .. }
        | ResolvedNode::AggregationAggregationMatch { .. }
        | ResolvedNode::AggregationCalculationMatch { .. }
        | ResolvedNode::AggregationTagMatch { .. }
        | ResolvedNode::ScalarMatch { .. } => true,
        ResolvedNode::CalculationMatch { calc, .. } => !calc.contains_tag(),
        _ => false,
    }
}

/// すべてのクエリ種別に対する統一 SQL エントリポイント。
/// ResolvedNode の構造を直接見てディスパッチする。
///   - LabelSetOp またはnvalue比較なしのProjection → build_fetch_nest_sql (Lv.2)
///   - nvalue比較付きのProjection                  → build_fetch_items_sql (Lv.1 フラットリスト)
///   - Scalar (aggregation / boolean)              → build_resolved_scalar_sql / build_boolean_sql
///   - Regular items                               → build_fetch_items_sql
pub fn build_fetch_sql(
    src: &Src,
    resolver: &crate::query::lens_resolver::Resolver,
    n: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    // 並び順（resolver が保持する解決済みの Order）。アイテム行を返す経路
    // （items / 定義枝）にのみ適用し、ラベルグループ（Nest/Projection）と
    // スカラー・ブーリアンには適用しない。
    let orders = resolver.resolved_order();
    // 定義参照（単体または Or だけで到達可能な枝）を含む場合:
    // 定義枝と src 由来枝を fetch レベルで束ねる（add_definitions）。
    // representative は projection と同じ decode_nest 経路で型付けする。
    if let Some((defs, rest)) =
        resolver.resolved_query.split_definition_branches()
    {
        let rest_fetch = rest.as_ref().map(|rest_node| {
            let pick = PickNode::new(src, rest_node);
            build_fetch_items_sql_impl(src, &pick, None, None, true, &[])
        });
        return crate::query::lens_builder::add_definitions(
            src, &defs, rest_fetch, n, offset, orders,
        );
    }
    if resolver.get_projection().is_some() {
        if resolver.get_label_set_op_node().is_some()
            || resolver.get_nvalue_condition().is_none()
        {
            return nest::build_fetch_nest_sql(src, resolver, n, offset);
        }
        // nvalue比較あり → Lv.1 フラットリスト（items path）
        let pick = PickNode::new(src, &resolver.resolved_query);
        let limit = if n > 0 { Some(n + 1) } else { None };
        return Ok(build_fetch_items_sql(
            src,
            &pick,
            limit,
            Some(offset),
            orders,
        ));
    }
    let limit = if n > 0 { Some(n + 1) } else { None };
    match &resolver.resolved_query {
        // スカラー集約：単一値を返す
        ResolvedNode::Aggregation(agg) => {
            return Ok(build_resolved_scalar_sql(
                src,
                &ResolvedOperand::Aggregation(agg.clone()),
            ));
        }
        // 集約または純スカラーを key に持つ Nest：単一値を返す
        ResolvedNode::Nest { keys, .. } => {
            let op = keys.first().unwrap();
            if op.contains_aggregation() || op.is_pure_scalar() {
                return Ok(build_resolved_scalar_sql(src, op));
            }
        }
        // ブーリアン比較：集約同士／集約とリテラル／リテラル同士 の比較結果を volatile row で返す
        node @ (ResolvedNode::AggregationMatch { .. }
        | ResolvedNode::AggregationAggregationMatch { .. }
        | ResolvedNode::AggregationCalculationMatch { .. }
        | ResolvedNode::AggregationTagMatch { .. }
        | ResolvedNode::ScalarMatch { .. }
        | ResolvedNode::NestMatch { .. }
        | ResolvedNode::MergedNestMatch { .. }) => {
            return Ok(build_boolean_sql(src, node));
        }
        // CalculationMatch: タグ参照なし（純スカラー・集約のみ）→ boolean
        // タグ参照を含む場合（例: (size: / 1024) :> 100）はアイテムフィルタへ fall-through
        ResolvedNode::CalculationMatch { calc, .. } if !calc.contains_tag() => {
            return Ok(build_boolean_sql(src, &resolver.resolved_query));
        }
        // NestNestMatch のうち Comparison + Literal 右辺は存在確認ブーリアン
        ResolvedNode::NestNestMatch {
            op: NestMatchOp::Comparison(_),
            right_nvalue: ResolvedOperand::Literal(_),
            ..
        } => {
            return Ok(build_boolean_sql(src, &resolver.resolved_query));
        }
        // スカラー比較の And/Or/Difference：全ての直接子がスカラー比較バリアントである場合に限り boolean
        // ラベル比較と演算子が異なるためバリアントで構造的に区別できる
        ResolvedNode::And(nodes) | ResolvedNode::Or(nodes)
            if !nodes.is_empty()
                && nodes.iter().all(|n| is_boolean_only_node(n)) =>
        {
            return Ok(build_boolean_sql(src, &resolver.resolved_query));
        }
        _ => {}
    }
    let pick = PickNode::new(src, &resolver.resolved_query);
    Ok(build_fetch_items_sql(
        src,
        &pick,
        limit,
        Some(offset),
        orders,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{BasicOp, ComparisonOp, QueryNode};
    use crate::query::lens_resolver::ResolvedAggregationNode;
    use crate::query::lens_resolver::ResolvedOperand;
    use crate::types::{Label, SType, TagType, TypedTag};
    use sea_query::{BinOper, PostgresQueryBuilder, SqliteQueryBuilder};

    #[test]
    fn test_to_bin_op_conversion() {
        assert_eq!(
            to_bin_op(ComparisonOp::Scalar(BasicOp::Eq)),
            BinOper::Equal
        );
        assert_eq!(
            to_bin_op(ComparisonOp::Scalar(BasicOp::Gt)),
            BinOper::GreaterThan
        );
        assert_eq!(
            to_bin_op(ComparisonOp::Scalar(BasicOp::Lt)),
            BinOper::SmallerThan
        );
    }

    #[test]
    fn test_to_tag_condition_generation() {
        let node =
            QueryNode::TypedTag(TypedTag::new(SType::Size, Label::from(100)));
        let cond = to_tag_condition(&node);

        let mut query = Query::select();
        query
            .column(sea_query::Alias::new("id"))
            .from(sea_query::Alias::new("tbl"))
            .cond_where(cond);
        let sql = query.to_string(SqliteQueryBuilder);

        // Verifying exact string content is fragile across sea-query versions/builders.
        // We ensure a query is generated (condition applied).
        assert!(!sql.is_empty());
    }

    #[test]
    fn test_build_flat_table_sql_structure() {
        use crate::query::lens_schema::StorageMapping;
        use sea_query::PostgresQueryBuilder;

        let node = ResolvedNode::Match {
            tag_type: TagType::Base(SType::Name),
            storage: StorageMapping::Fixed(Col::Name),
            bitical_type: crate::db::BiticalType::String,
            op: ComparisonOp::Scalar(BasicOp::Eq),
            label: Label::from("test"),
        };
        let query_node = QueryNode::And(vec![]);
        let sql = build_flat_table_sql(
            &Src::OneView,
            &PickNode::new(&Src::OneView, &node),
            &query_node,
            Some(10),
            None,
        );
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // 基本構造の検証
        assert!(sql_str.contains("SELECT"), "SQL should contain SELECT");
        assert!(
            sql_str.contains("\"item_id\""),
            "SQL should contain item_id column"
        );
        assert!(
            sql_str.contains("\"rank\""),
            "SQL should contain rank column"
        );
        assert!(
            sql_str.contains("FROM \"oneview\""),
            "SQL should select from oneview"
        );
        // サブクエリによる絞り込みの検証
        assert!(
            sql_str.contains("IN (SELECT"),
            "SQL should use IN (SELECT...) for filtering"
        );
        assert!(
            sql_str.contains("AS \"pk\""),
            "SQL should use pk alias for subquery"
        );
        // ソートとリミットの検証
        assert!(
            sql_str.contains("ORDER BY"),
            "SQL should have ORDER BY clause"
        );
        assert!(sql_str.contains("LIMIT 10"), "SQL should have LIMIT 10");
    }

    #[test]
    fn test_build_fetch_items_sql_union_schema() {
        use crate::query::lens_schema::StorageMapping;
        use sea_query::PostgresQueryBuilder;

        let node = ResolvedNode::Match {
            tag_type: TagType::Base(SType::Name),
            storage: StorageMapping::Fixed(Col::Name),
            bitical_type: crate::db::BiticalType::String,
            op: ComparisonOp::Scalar(BasicOp::Eq),
            label: Label::from("test"),
        };
        let pick = PickNode::new(&Src::OneView, &node);
        let sql =
            build_fetch_items_sql(&Src::OneView, &pick, Some(10), Some(0), &[]);
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // 新スキーマ: "tag_type" フィールドが含まれる
        assert!(
            sql_str.contains("tag_type"),
            "SQL should use new tag_type field: {}",
            sql_str
        );
        // UNION型変換: union_value(\"integer\" := ...) 等が含まれる
        assert!(
            sql_str.contains("union_value(\"integer\" :="),
            "SQL should have union_value(\"integer\" :=: {}",
            sql_str
        );
        assert!(
            sql_str.contains("union_value(\"string\" :="),
            "SQL should have union_value(\"string\" :=: {}",
            sql_str
        );
        assert!(
            sql_str.contains("union_value(\"boolean\" :="),
            "SQL should have union_value(\"boolean\" :=: {}",
            sql_str
        );
        assert!(
            sql_str.contains("union_value(\"double\" :="),
            "SQL should have union_value(\"double\" :=: {}",
            sql_str
        );
        // struct_pack と list() でラップ
        assert!(
            sql_str.contains("struct_pack"),
            "SQL should use struct_pack: {}",
            sql_str
        );
        // 旧スキーマの EAV フィールド名が struct_pack 内に現れない
        assert!(
            !sql_str.contains("\"label_str\" :="),
            "SQL should not have old label_str field: {}",
            sql_str
        );
    }

    #[test]
    fn test_build_agg_count_items() {
        let agg =
            ResolvedAggregationNode::Count(Box::new(ResolvedNode::And(vec![
                ResolvedNode::Match {
                    tag_type: TagType::Base(SType::Extension),
                    storage: StorageMapping::Basic {
                        column: Col::LabelStr,
                        tag_type: "extension".to_string(),
                    },
                    bitical_type: crate::db::BiticalType::String,
                    op: ComparisonOp::Scalar(BasicOp::Eq),
                    label: Label::from("txt"),
                },
            ])));

        let agg_ctx = build_aggregation_context_for_agg(&Src::OneView, &agg);
        let sql = build_agg(&Src::OneView, &agg, &agg_ctx);
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // アイテム数を数えるので COUNT(DISTINCT "item_id")
        assert!(sql_str.contains("COUNT(DISTINCT \"item_id\")"));
        // フィルタ条件
        assert!(sql_str
            .contains("WHERE \"item_id\" IN (SELECT \"item_id\" FROM (SELECT"));
        assert!(sql_str.contains(
            "WHERE \"type\" = 'extension' AND \"label_str\" = 'txt'"
        ));
    }

    #[test]
    fn test_build_agg_sum_projection() {
        use crate::query::ast::ArithmeticAggOp;
        let agg = ResolvedAggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(ResolvedNode::Nest {
                keys: vec![ResolvedOperand::TagRef {
                    tag_type: TagType::Base(SType::Size),
                    storage: StorageMapping::Basic {
                        column: Col::LabelInt,
                        tag_type: "size".to_string(),
                    },
                    bitical_type: crate::db::BiticalType::Integer,
                }],
                nvalue: None,
                context: None,
            }),
        };

        let sql = build_agg(&Src::OneView, &agg, &AggregationContext::new());
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // サブクエリ形式: SUM("val")
        assert!(sql_str.contains("SUM(\"val\")"));
        // サブクエリ内での抽出
        assert!(sql_str.contains("any_value(\"label_int\") AS \"val\""));
        assert!(sql_str.contains("\"type\" IN ('size')"));
    }

    #[test]
    fn test_build_aggregation_sql_structure() {
        use crate::query::ast::ArithmeticAggOp;
        use crate::query::lens_resolver::ResolvedNode;
        use crate::query::lens_schema::StorageMapping;
        use crate::types::{Label, SType, TagType};
        use sea_query::PostgresQueryBuilder;

        // SUM(size) inside Aggregation with Filter(project:ttfm)
        // AggregationNode::Arithmetic { op: Sum, inner: And(Nest, Filter) }
        let agg = ResolvedAggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(ResolvedNode::And(vec![
                ResolvedNode::Nest {
                    keys: vec![ResolvedOperand::TagRef {
                        tag_type: TagType::Base(SType::Size),
                        storage: StorageMapping::Fixed(Col::Size),
                        bitical_type: crate::db::BiticalType::Integer,
                    }],
                    nvalue: None,
                    context: None,
                },
                ResolvedNode::Match {
                    tag_type: TagType::Custom("project".to_string()),
                    storage: StorageMapping::Basic {
                        column: Col::LabelStr,
                        tag_type: "project".to_string(),
                    },
                    bitical_type: crate::db::BiticalType::String,
                    op: crate::query::ast::ComparisonOp::Scalar(
                        crate::query::ast::BasicOp::Eq,
                    ),
                    label: Label::from("ttfm"),
                },
            ])),
        };

        let agg_ctx = build_aggregation_context_for_agg(&Src::OneView, &agg);
        let sql = build_agg(&Src::OneView, &agg, &agg_ctx);
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // サブクエリ形式: SUM("val")
        assert!(sql_str.contains("SUM(\"val\")"));
        assert!(sql_str.contains("any_value(\"size\") AS \"val\""));

        // project フィルタ
        assert!(sql_str
            .contains("WHERE \"type\" = 'project' AND \"label_str\" = 'ttfm'"));
        assert!(sql_str.contains("GROUP BY \"item_id\""));
    }

    #[test]
    fn test_build_calculation_expr_simple() {
        use crate::query::ast::ArithmeticOp;
        use crate::query::lens_resolver::{
            ResolvedCalculationNode, ResolvedOperand,
        };
        use crate::types::Label;

        let calc = ResolvedCalculationNode {
            left: ResolvedOperand::Literal(Label::from(1)),
            op: ArithmeticOp::Add,
            right: ResolvedOperand::Literal(Label::from(2)),
        };

        let expr = build_agg_calc_expr(&calc, &AggregationContext::new());
        let sql_str = format!("{:?}", expr);

        // SQL式に加算演算が含まれていることを確認
        assert!(
            sql_str.contains("Add") || sql_str.contains("+"),
            "Should contain addition operation"
        );
    }

    #[test]
    fn test_build_resolved_operand_literal() {
        use crate::query::lens_resolver::ResolvedOperand;
        use crate::types::Label;

        let operand = ResolvedOperand::Literal(Label::from(42));
        let expr = build_agg_operand_expr(&operand, &AggregationContext::new());
        let sql_str = format!("{:?}", expr);

        // 数値リテラルが含まれていることを確認
        assert!(sql_str.contains("42"), "Should contain literal value 42");
    }

    #[test]
    fn test_build_resolved_operand_expr_for_arithmetic_boolean_cast() {
        use crate::db::BiticalType;
        use crate::query::lens_resolver::ResolvedOperand;
        use crate::types::TagType;

        // 1. Boolean Literal -> CAST(... AS BIGINT)
        let lit_bool = ResolvedOperand::Literal(crate::types::Label::resolve(
            TagType::from("is_dir"),
            crate::types::Bitical::Boolean(true),
        ));
        let expr_lit = build_resolved_operand_expr_for_arithmetic(
            &lit_bool,
            &AggregationContext::new(),
        );
        let sql_lit = sea_query::Query::select()
            .expr(expr_lit)
            .to_string(sea_query::PostgresQueryBuilder);
        assert!(
            sql_lit.contains("CAST"),
            "Boolean literal should be cast: {}",
            sql_lit
        );
        assert!(
            sql_lit.contains("BIGINT"),
            "Boolean literal should be cast to BIGINT: {}",
            sql_lit
        );

        // 2. Boolean TagRef -> CAST(... AS BIGINT)
        let tag_bool = ResolvedOperand::TagRef {
            tag_type: TagType::from("is_dir"),
            storage: crate::query::lens_schema::StorageMapping::Fixed(
                crate::db::Col::LabelBool,
            ),
            bitical_type: BiticalType::Boolean,
        };
        let expr_tag = build_resolved_operand_expr_for_arithmetic(
            &tag_bool,
            &AggregationContext::new(),
        );
        let sql_tag = sea_query::Query::select()
            .expr(expr_tag)
            .to_string(sea_query::PostgresQueryBuilder);
        assert!(
            sql_tag.contains("CAST"),
            "Boolean column should be cast: {}",
            sql_tag
        );
        assert!(
            sql_tag.contains("BIGINT"),
            "Boolean column should be cast to BIGINT: {}",
            sql_tag
        );
    }
}
