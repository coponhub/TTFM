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

use crate::query::ast::{
    AggregationNode, CalculationNode, ComparisonNode, Operand, QueryNode,
};
use crate::query::functions::{expand_comparison_node, QueryFunctionRegistry};
use crate::types::{Label, LabelValue, TagType};
use anyhow::{bail, Result};

// ========== Logical Representation ==========

/// クエリエンジンの論理レイヤーで扱う型。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LogicalType {
    Integer,
    Float,
    String,
    Boolean,
    Any,
}

impl LogicalType {
    /// 数値計算が可能な型かどうかを返します。
    pub fn is_numeric(&self) -> bool {
        matches!(self, Self::Integer | Self::Float)
    }
}

/// 論理的なスキーマ情報を提供するインターフェース。
pub trait LogicalSchema {
    /// 指定されたタグの論理型を返します。
    fn get_logical_type(&self, tag: &TagType) -> LogicalType;

    /// タグの論理展開（Virtualタグの具体化など）を行います。
    fn expand_tag(&self, tag_type: &TagType, label: &Label) -> QueryNode;

    /// プロジェクションの論理展開を行います。
    fn expand_projection(&self, tag_type: &TagType) -> QueryNode;
}

/// QueryNodeを論理的に展開します（Virtual tag展開、日付範囲化など）
///
/// pub(crate) でテストからのアクセスを確保
pub(crate) fn expand_query_node(
    schema: &impl LogicalSchema,
    node: QueryNode,
) -> Result<QueryNode> {
    match node {
        QueryNode::TypedTag(tt) => Ok(schema.expand_tag(&tt.label.tag_type(), &tt.label)),
        QueryNode::Projection(op) => {
            if let Operand::TypeRef(tag_type) = &op {
                Ok(schema.expand_projection(tag_type))
            } else {
                Ok(QueryNode::Projection(op))
            }
        }
        QueryNode::And(nodes) => {
            let mut expanded = Vec::new();
            for n in nodes {
                expanded.push(expand_query_node(schema, n)?);
            }
            Ok(QueryNode::And(expanded))
        }
        QueryNode::Or(nodes) => {
            let mut expanded = Vec::new();
            for n in nodes {
                expanded.push(expand_query_node(schema, n)?);
            }
            Ok(QueryNode::Or(expanded))
        }
        QueryNode::Difference(l, r) => Ok(QueryNode::Difference(
            Box::new(expand_query_node(schema, *l)?),
            Box::new(expand_query_node(schema, *r)?),
        )),
        QueryNode::Complement(c) => Ok(QueryNode::Complement(Box::new(
            expand_query_node(schema, *c)?,
        ))),
        QueryNode::Comparison(cmp) => {
            expand_comparison_with_recursion(schema, cmp)
        }
        QueryNode::Aggregation(agg) => {
            Ok(QueryNode::Aggregation(expand_aggregation(schema, agg)?))
        }
        other => Ok(other),
    }
}

fn expand_aggregation(
    schema: &impl LogicalSchema,
    agg: AggregationNode,
) -> Result<AggregationNode> {
    match agg {
        AggregationNode::Count(node) => Ok(AggregationNode::Count(Box::new(
            expand_query_node(schema, *node)?,
        ))),
        AggregationNode::Arithmetic { op, inner } => {
            Ok(AggregationNode::Arithmetic {
                op,
                inner: Box::new(expand_query_node(schema, *inner)?),
            })
        }
    }
}

fn expand_comparison_with_recursion(
    schema: &impl LogicalSchema,
    mut cmp: ComparisonNode,
) -> Result<QueryNode> {
    // 演算のバリデーションを実施
    validate_comparison_operands(&cmp, schema)?;

    // 連鎖比較 (a < b < c) の展開
    if cmp.rest.len() > 1 {
        let mut nodes = Vec::new();
        let mut left = cmp.first.clone();
        for (op, right) in cmp.rest {
            let single_cmp = ComparisonNode {
                first: left.clone(),
                rest: vec![(op, right.clone())],
            };
            // 各比較を再帰的に展開
            nodes.push(expand_comparison_with_recursion(schema, single_cmp)?);
            left = right;
        }
        return Ok(QueryNode::And(nodes));
    }

    // Firstオペランドの展開
    if let Operand::Aggregation(agg) = &mut cmp.first {
        *agg = Box::new(expand_aggregation(schema, (**agg).clone())?);
    }

    // Restオペランドの展開
    // ここに来る時は rest.len() == 1 または 0 のはず
    for (_op, operand) in &mut cmp.rest {
        if let Operand::Aggregation(agg) = operand {
            *agg = Box::new(expand_aggregation(schema, (**agg).clone())?);
        }
    }

    // 標準の比較ノード展開（日付範囲化など）を実行
    let reg = QueryFunctionRegistry::with_standard();
    let expanded_node = expand_comparison_node(cmp, &reg);
    Ok(expanded_node)
}

/// 全てのオペランド（Calculation/Aggregation 含む）の妥当性をチェックします
fn validate_comparison_operands(
    cmp: &ComparisonNode,
    schema: &impl LogicalSchema,
) -> Result<()> {
    validate_operand(&cmp.first, schema)?;
    for (_, op) in &cmp.rest {
        validate_operand(op, schema)?;
    }
    Ok(())
}

fn validate_operand(operand: &Operand, schema: &impl LogicalSchema) -> Result<()> {
    match operand {
        Operand::Calculation(calc) => validate_calculation(calc, schema),
        Operand::Aggregation(agg) => match agg.as_ref() {
            AggregationNode::Count(node) => {
                let _ = node;
                Ok(())
            }
            AggregationNode::Arithmetic { inner, .. } => {
                // inner の妥当性は expand_query_node でチェックされる
                let _ = inner;
                Ok(())
            }
        },
        _ => Ok(()),
    }
}

/// 算術演算の型チェック（論理レベル）
pub fn validate_calculation(
    calc: &CalculationNode,
    schema: &impl LogicalSchema,
) -> Result<()> {
    let left_type = infer_type(&calc.left, schema)?;
    let right_type = infer_type(&calc.right, schema)?;

    if !left_type.is_numeric() || !right_type.is_numeric() {
        bail!("Arithmetic operations are only possible for numeric types.");
    }
    Ok(())
}

/// オペランドの型を推論
fn infer_type(operand: &Operand, schema: &impl LogicalSchema) -> Result<LogicalType> {
    match operand {
        Operand::Literal(label) => Ok(infer_logical_type_from_label(label)),
        Operand::TypeRef(tag_type) => Ok(schema.get_logical_type(tag_type)),
        Operand::Calculation(calc) => {
            validate_calculation(calc, schema)?;
            Ok(LogicalType::Integer) // 暫定：演算結果は一旦数値
        }
        Operand::Aggregation(_) => Ok(LogicalType::Integer),
    }
}

fn infer_logical_type_from_label(label: &crate::types::Label) -> LogicalType {
    match label.value() {
        LabelValue::Integer(_) => LogicalType::Integer,
        LabelValue::Boolean(_) => LogicalType::Boolean,
        LabelValue::String(s) | LabelValue::Literal(s) => {
            // "1MB" などのサイズ単位付きリテラルは数値として扱う
            if crate::util::parse_size(&s).is_some() {
                LogicalType::Integer
            } else {
                LogicalType::String
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{CalculationNode, ComparisonNode, Operand, QueryNode};
    use crate::query::lens_schema::Lens;
    use crate::types::{TagType, TypedTag};

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

    #[test]
    fn test_expand_query_node_invalid_calc() {
        let lens = Lens::base_standard();
        // (path: + 10) > 100 はエラーになるべき
        let calc = CalculationNode {
            left: Operand::TypeRef(TagType::from("path")),
            op: crate::query::ast::ArithmeticOp::Add,
            right: Operand::Literal(crate::types::Label::from(10i64)),
        };
        let cmp = ComparisonNode {
            first: Operand::Calculation(Box::new(calc)),
            rest: vec![(
                crate::query::ast::ComparisonOp::Scalar(crate::query::ast::BasicOp::Gt),
                Operand::Literal(crate::types::Label::from(100i64)),
            )],
        };
        let node = QueryNode::Comparison(cmp);
        let result = expand_query_node(&lens, node);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Arithmetic operations are only possible for numeric types"));
    }

    #[test]
    fn test_validate_calculation() {
        let lens = Lens::base_standard();
        // size + 100 (Integer + Integer) -> OK
        let calc_ok = CalculationNode {
            left: Operand::TypeRef(TagType::from("size")),
            op: crate::query::ast::ArithmeticOp::Add,
            right: Operand::Literal(crate::types::Label::from(100i64)),
        };
        assert!(validate_calculation(&calc_ok, &lens).is_ok());

        // path + 100 (String + Integer) -> Err
        let calc_err = CalculationNode {
            left: Operand::TypeRef(TagType::from("path")),
            op: crate::query::ast::ArithmeticOp::Add,
            right: Operand::Literal(crate::types::Label::from(100i64)),
        };
        assert!(validate_calculation(&calc_err, &lens).is_err());
    }

    #[test]
    fn test_logical_type_numeric() {
        assert!(LogicalType::Integer.is_numeric());
        assert!(LogicalType::Float.is_numeric());
        assert!(!LogicalType::String.is_numeric());
        assert!(!LogicalType::Boolean.is_numeric());
        assert!(!LogicalType::Any.is_numeric());
    }

    #[test]
    fn test_expand_chain_comparison() {
        use crate::query::ast::{BasicOp, ComparisonOp};
        let lens = Lens::base_standard();

        // 10 < size: <= 100
        let cmp = ComparisonNode {
            first: Operand::Literal(crate::types::Label::from(10i64)),
            rest: vec![
                (ComparisonOp::Scalar(BasicOp::Lt), Operand::TypeRef(TagType::from("size"))),
                (ComparisonOp::Scalar(BasicOp::Le), Operand::Literal(crate::types::Label::from(100i64))),
            ],
        };

        let node = QueryNode::Comparison(cmp);
        let expanded = expand_query_node(&lens, node).unwrap();

        // 期待値: And([Comparison(10 < size:), Comparison(size: <= 100)])
        if let QueryNode::And(nodes) = expanded {
            assert_eq!(nodes.len(), 2);
            // 内部の各 ComparisonNode は再帰的に展開されているはず
        } else {
            panic!("Expected QueryNode::And for chain comparison, got {:?}", expanded);
        }
    }
}
