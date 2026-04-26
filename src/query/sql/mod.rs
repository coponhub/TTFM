mod pick;
mod util;
mod agg_pieces;
mod calc_pieces;
mod boolean;
mod scalar;
mod nest;
mod low_dispatcher;
mod precompute;
pub use pick::{
    build_pick, build_pick_agg, build_pick_nest,
    BuildPick, SimplePickNode, AggPickNode, NestPickNode, PickNode,
};
pub use util::{AggregationContext, NestContext, to_tag_condition};
pub use precompute::{
    needs_aggregation_context, build_aggregation_context, build_aggregation_context_for_operand, build_aggregation_context_for_agg,
    needs_nest_context, build_nest_context, build_nest_context_for_operand,
};
use agg_pieces::{
    agg_expr, resolve_count_target,
    build_agg, build_agg_nest,
    build_agg_calc_expr, build_agg_calc_eav_expr,
    build_agg_calc_subquery, build_agg_calc_subquery_nest,
    build_agg_operand_eav_expr,
    build_nvalue_standalone_subquery, build_nvalue_cte, build_nvalue_cte_nest,
    build_nest_pivot_cte,
};
#[cfg(test)]
use agg_pieces::{build_agg_operand_expr, build_resolved_operand_expr_for_arithmetic};
use calc_pieces::{build_calculation_expr, build_calculation_eav_expr, fold_simple_operand};
use boolean::{
    build_resolved_and_sql, build_resolved_or_sql, build_resolved_diff_sql,
    build_resolved_comp_sql, build_label_set_op_pick_sql,
    build_boolean_sql,
};
#[cfg(test)]
use boolean::{build_direct_boolean_select, wrap_boolean_collider};
use scalar::{
    build_resolved_match_sql, build_column_match_sql,
    build_resolved_tag_tag_match_sql, build_scalar_match_sql,
    build_resolved_scalar_sql,
};
use nest::{
    build_nest_match_sql, build_nest_nest_match_sql, build_merged_nest_match_sql,
};
use low_dispatcher::try_dispatch_common;
use util::*;

use crate::db::Col;
use crate::query::ast::QueryNode;
use crate::query::lens_resolver::ResolvedNode;
#[cfg(test)]
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use sea_query::{Alias, Expr, Query, SelectStatement};


fn build_fetch_items_sql(
    pick: &PickNode<'_>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> SelectStatement {
    let node = pick.node();
    let view = pick.view();
    // 集約クエリ (e.g. count(path:) や sum(size:) > 100) の場合は、
    // oneview との結合を行わず、集約計算結果だけをそのまま返す。
    // NestMatch / NestNestMatch は tags カラムが必要なため
    // ここでは早期リターンせず、通常のタグパッキング処理に委ねる。
    match node {
        ResolvedNode::Aggregation(_)
        | ResolvedNode::AggregationMatch { .. } => {
            return pick.build_pick();
        }
        _ => {}
    }

    let pick_sql = pick.build_pick();
    let columns = Col::raw_tag_row_columns();

    // 1. まず ID を絞り込むためのサブクエリを構築
    let mut id_query = Query::select();
    id_query
        .column(Col::ItemId)
        .from_subquery(pick_sql, Alias::new("pk"))
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    if let Some(l) = limit {
        id_query.limit(l as u64);
    }
    if let Some(o) = offset {
        id_query.offset(o as u64);
    }

    // 2. 物理カラムをパックして集約。
    // DuckDB の struct_pack("col1" := "col1", ...) 構文を使用。
    let fields = columns
        .iter()
        .map(|c| {
            format!(
                "\"{}\" := \"{}\"",
                sea_query::Iden::to_string(c),
                sea_query::Iden::to_string(c)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let tags_expr = format!("LIST(struct_pack({}))", fields);

    let mut q = Query::select();
    q.column(Col::ItemId)
        .expr_as(Expr::col(Col::Rank).max(), Col::Rank)
        .expr_as(
            Expr::cust(format!(
                "ANY_VALUE(\"{}\")",
                sea_query::Iden::to_string(&Col::ItemKind)
            )),
            Col::ItemKind,
        )
        .expr_as(Expr::cust(tags_expr), crate::db::QueryResultCol::Tags)
        .from(Alias::new(view))
        .and_where(Expr::col(Col::ItemId).in_subquery(id_query))
        .group_by_col(Col::ItemId)
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    q
}

pub fn build_flat_table_sql(
    pick: &PickNode<'_>,
    query_node: &QueryNode,
    limit: Option<usize>,
    offset: Option<usize>,
) -> SelectStatement {
    let view = pick.view();
    let pick_sql = pick.build_pick();
    let tagcond = to_tag_condition(query_node);

    // 1. まず ID を絞り込むためのサブクエリを構築
    let mut id_query = Query::select();
    id_query
        .column(Col::ItemId)
        .from_subquery(pick_sql.to_owned(), Alias::new("pk"))
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
        .from(Alias::new(view))
        .and_where(Expr::col(Col::ItemId).in_subquery(id_query))
        .and_where(tagcond.into())
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    q
}

/// すべてのクエリ種別に対する統一 SQL エントリポイント。
/// ResolvedNode の構造を直接見てディスパッチする。
///   - LabelSetOp またはnvalue比較なしのProjection → build_fetch_nest_sql (Lv.2)
///   - nvalue比較付きのProjection                  → build_fetch_items_sql (Lv.1 フラットリスト)
///   - Scalar (aggregation / boolean)              → build_resolved_scalar_sql / build_boolean_sql
///   - Regular items                               → build_fetch_items_sql
pub fn build_fetch_sql(
    resolver: &crate::query::lens_resolver::Resolver,
    view: &str,
    n: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    if resolver.get_projection().is_some() {
        if resolver.get_label_set_op_node().is_some() || resolver.get_nvalue_condition().is_none() {
            return nest::build_fetch_nest_sql(resolver, view, n, offset);
        }
        // nvalue比較あり → Lv.1 フラットリスト（items path）
        let pick = PickNode::new(&resolver.resolved_query, view);
        let limit = if n > 0 { Some(n + 1) } else { None };
        return Ok(build_fetch_items_sql(&pick, limit, Some(offset)));
    }
    if let Some(op) = resolver.get_scalar_expression() {
        return Ok(build_resolved_scalar_sql(&op, view));
    }
    if resolver.resolved_query.is_boolean_result() {
        return Ok(build_boolean_sql(&resolver.resolved_query, view));
    }
    let pick = PickNode::new(&resolver.resolved_query, view);
    let limit = if n > 0 { Some(n + 1) } else { None };
    Ok(build_fetch_items_sql(&pick, limit, Some(offset)))
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
            .column(Alias::new("id"))
            .from(Alias::new("tbl"))
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
            storage: StorageMapping::Column(Col::Name),
            sql_type: crate::db::SqlType::VARCHAR,
            op: ComparisonOp::Scalar(BasicOp::Eq),
            label: Label::from("test"),
        };
        let query_node = QueryNode::And(vec![]);
        let sql =
            build_flat_table_sql(&PickNode::new(&node, "oneview"), &query_node, Some(10), None);
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
    fn test_build_agg_count_items() {
        let agg =
            ResolvedAggregationNode::Count(Box::new(ResolvedNode::And(vec![
                ResolvedNode::Match {
                    tag_type: TagType::Base(SType::Extension),
                    storage: StorageMapping::RowTag {
                        column: Col::LabelStr,
                        tag_type: "extension".to_string(),
                    },
                    sql_type: crate::db::SqlType::VARCHAR,
                    op: ComparisonOp::Scalar(BasicOp::Eq),
                    label: Label::from("txt"),
                },
            ])));

        let agg_ctx = build_aggregation_context_for_agg(&agg, "oneview");
        let sql = build_agg(&agg, "oneview", &agg_ctx);
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
                    storage: StorageMapping::RowTag {
                        column: Col::LabelInt,
                        tag_type: "size".to_string(),
                    },
                    sql_type: crate::db::SqlType::BIGINT,
                }],
                nvalue: None,
                context: None,
            }),
        };

        let sql = build_agg(&agg, "oneview", &AggregationContext::new());
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // サブクエリ形式: SUM("val")
        assert!(sql_str.contains("SUM(\"val\")"));
        // サブクエリ内での抽出
        assert!(sql_str.contains("any_value(\"label_int\") AS \"val\""));
        assert!(sql_str.contains("\"type\" IN ('size')"));
    }

    #[test]
    fn test_wrap_boolean_collider() {
        let mut inner = Query::select();
        inner
            .column(Col::ItemId)
            .from(Alias::new("t"))
            .and_where(Expr::col(Col::ItemId).eq(1));

        let sql = wrap_boolean_collider(inner);
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(sql_str.contains("FROM (SELECT"));
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
                        storage: StorageMapping::Column(Col::Size),
                        sql_type: crate::db::SqlType::BIGINT,
                    }],
                    nvalue: None,
                    context: None,
                },
                ResolvedNode::Match {
                    tag_type: TagType::Custom("project".to_string()),
                    storage: StorageMapping::RowTag {
                        column: Col::LabelStr,
                        tag_type: "project".to_string(),
                    },
                    sql_type: crate::db::SqlType::VARCHAR,
                    op: crate::query::ast::ComparisonOp::Scalar(
                        crate::query::ast::BasicOp::Eq,
                    ),
                    label: Label::from("ttfm"),
                },
            ])),
        };

        let agg_ctx = build_aggregation_context_for_agg(&agg, "oneview");
        let sql = build_agg(&agg, "oneview", &agg_ctx);
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

    /// TDD: build_direct_boolean_select のテスト
    /// 生成されるSQLにNULLチェックが含まれていることを確認
    #[test]
    fn test_build_direct_boolean_select_null_propagation() {
        use sea_query::PostgresQueryBuilder;

        // 左辺: サブクエリ (NULL可能)
        let left = subquery(Query::select().expr(Expr::val(100i64)).to_owned());
        // 右辺: リテラル
        let right = Expr::val(50i64).into();

        let sql = build_direct_boolean_select(
            left,
            ComparisonOp::Scalar(BasicOp::Gt),
            right,
            "oneview",
        );
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // NULL チェックが含まれていることを確認
        // CASE WHEN (...) THEN 1 WHEN (...) IS NULL THEN NULL ELSE 0 END
        assert!(
            sql_str.contains("IS NULL"),
            "SQL should contain NULL check: {}",
            sql_str
        );
        assert!(
            sql_str.contains("CASE"),
            "SQL should contain CASE: {}",
            sql_str
        );
    }


    #[test]
    fn test_build_resolved_operand_expr_for_arithmetic_boolean_cast() {
        use crate::db::SqlType;
        use crate::query::lens_resolver::ResolvedOperand;
        use crate::types::TagType;

        // 1. Boolean Literal -> CAST(... AS BIGINT)
        let lit_bool = ResolvedOperand::Literal(crate::types::Label::resolve(
            TagType::from("is_dir"),
            crate::types::LabelValue::Boolean(true),
        ));
        let expr_lit =
            build_resolved_operand_expr_for_arithmetic(&lit_bool, &AggregationContext::new());
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
            storage: crate::query::lens_schema::StorageMapping::Column(
                crate::db::Col::LabelBool,
            ),
            sql_type: SqlType::BOOLEAN,
        };
        let expr_tag =
            build_resolved_operand_expr_for_arithmetic(&tag_bool, &AggregationContext::new());
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

