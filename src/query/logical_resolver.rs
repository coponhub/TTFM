//! # 論理解決器（Logical Resolver）
//!
//! Query AST の論理的な意味解決を担当します。
//!
//! ## 責務
//!
//! 1. **Virtual tag の具体化**: `directory:` → `is_dir:true`
//! 2. **日付範囲の展開**: `mtime:today` → `mtime >= X AND mtime <= Y`
//! 3. **型チェック**: 算術演算は数値型のみ、など
//!
//! ## 処理フロー
//!
//! ```text
//! QueryNode (AST)
//!   ↓ expand_query_node()
//! QueryNode (論理展開済み・型検証済み)
//! ```

use crate::db::SqlType;
use crate::query::ast::{AggregationNode, CalculationNode, ComparisonNode, Operand, QueryNode};
use crate::query::functions::{expand_comparison_node, QueryFunctionRegistry};
use crate::query::lens_schema::Lens;
use crate::types::LabelValue;
use anyhow::{bail, Result};

/// QueryNodeを論理的に展開します（Virtual tag展開、日付範囲化など）
///
/// pub(crate) でテストからのアクセスを確保
pub(crate) fn expand_query_node(
    lens: &Lens,
    node: QueryNode,
) -> Result<QueryNode> {
    match node {
        QueryNode::TypedTag(tt) => {
            if let Some(desc) = lens.look_up(&tt.label.tag_type()) {
                if let Some(func) = &desc.logical_function {
                    return Ok(func.expand(&tt.label));
                }
            }
            Ok(QueryNode::TypedTag(tt))
        }
        QueryNode::Projection(op) => {
            if let Operand::TypeRef(tagtype) = &op {
                if let Some(desc) = lens.look_up(tagtype) {
                    if let Some(func) = &desc.logical_function {
                        return Ok(func.expand_projection(tagtype.clone()));
                    }
                }
            }
            Ok(QueryNode::Projection(op))
        }
        QueryNode::And(nodes) => {
            let mut expanded = Vec::new();
            for n in nodes {
                expanded.push(expand_query_node(lens, n)?);
            }
            Ok(QueryNode::And(expanded))
        }
        QueryNode::Or(nodes) => {
            let mut expanded = Vec::new();
            for n in nodes {
                expanded.push(expand_query_node(lens, n)?);
            }
            Ok(QueryNode::Or(expanded))
        }
        QueryNode::Difference(l, r) => Ok(QueryNode::Difference(
            Box::new(expand_query_node(lens, *l)?),
            Box::new(expand_query_node(lens, *r)?),
        )),
        QueryNode::Complement(c) => Ok(QueryNode::Complement(Box::new(
            expand_query_node(lens, *c)?,
        ))),
        QueryNode::Comparison(cmp) => {
            expand_comparison_with_recursion(lens, cmp)
        }
        QueryNode::Aggregation(agg) => {
            Ok(QueryNode::Aggregation(expand_aggregation(lens, agg)?))
        }
        other => Ok(other),
    }
}

fn expand_aggregation(
    lens: &Lens,
    agg: AggregationNode,
) -> Result<AggregationNode> {
    match agg {
        AggregationNode::Count(node) => Ok(AggregationNode::Count(Box::new(
            expand_query_node(lens, *node)?,
        ))),
        AggregationNode::Arithmetic { op, inner } => {
            Ok(AggregationNode::Arithmetic {
                op,
                inner: Box::new(expand_query_node(lens, *inner)?),
            })
        }
    }
}

fn expand_comparison_with_recursion(
    lens: &Lens,
    mut cmp: ComparisonNode,
) -> Result<QueryNode> {
    // Firstオペランドの展開
    if let Operand::Aggregation(agg) = &mut cmp.first {
        *agg = Box::new(expand_aggregation(lens, (**agg).clone())?);
    }

    // Restオペランドの展開
    for (_op, operand) in &mut cmp.rest {
        if let Operand::Aggregation(agg) = operand {
            *agg = Box::new(expand_aggregation(lens, (**agg).clone())?);
        }
    }

    // 標準の比較ノード展開（日付範囲化など）を実行
    let reg = QueryFunctionRegistry::with_standard();
    let expanded_node = expand_comparison_node(cmp, &reg);
    Ok(expanded_node)
}

/// 算術演算の型チェック（論理レベル）
pub fn validate_calculation(
    calc: &CalculationNode,
    lens: &Lens,
) -> Result<()> {
    let left_type = infer_type(&calc.left, lens)?;
    let right_type = infer_type(&calc.right, lens)?;

    if !is_numeric(&left_type) || !is_numeric(&right_type) {
        bail!("算術演算は数値型に対してのみ可能です。");
    }
    Ok(())
}

/// オペランドの型を推論（Lensから取得）
fn infer_type(operand: &Operand, lens: &Lens) -> Result<SqlType> {
    match operand {
        Operand::Literal(label) => Ok(infer_sql_type_from_label(label)),
        Operand::TypeRef(tag_type) => {
            let desc = lens.look_up_or_default(tag_type);
            Ok(desc.sql_type)
        }
        Operand::Calculation(_) => Ok(SqlType::BIGINT),
        Operand::Aggregation(_) => Ok(SqlType::BIGINT),
    }
}

/// Label の値から SqlType を推論します
fn infer_sql_type_from_label(label: &crate::types::Label) -> SqlType {
    match label.value() {
        LabelValue::Integer(_) => SqlType::BIGINT,
        LabelValue::Boolean(_) => SqlType::BOOLEAN,
        LabelValue::String(_) | LabelValue::Literal(_) => SqlType::VARCHAR,
    }
}

fn is_numeric(t: &SqlType) -> bool {
    matches!(t, SqlType::BIGINT | SqlType::DOUBLE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::QueryNode;
    use crate::types::TypedTag;

    #[test]
    fn test_expand_simple_and() {
        let lens = Lens::base_standard();
        let node = QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new("size", "100")),
            QueryNode::TypedTag(TypedTag::new("mtime", "today")),
        ]);
        let expanded = expand_query_node(&lens, node).unwrap();
        // and(size:100, mtime:today) -> and(size:100, and(mtime>=..., mtime<=...))
        assert!(matches!(expanded, QueryNode::And(_)));
    }
}
