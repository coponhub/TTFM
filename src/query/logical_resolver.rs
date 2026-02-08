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
    AggregationNode, CalculationNode, ComparisonNode, ComparisonOp, Operand,
    QueryNode,
};
use crate::query::error;
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
        QueryNode::TypedTag(tt) => {
            Ok(schema.expand_tag(&tt.label.tag_type(), &tt.label))
        }
        QueryNode::Projection(op) => {
            // TypeRef の場合は schema による展開を適用
            if let Operand::TypeRef(tag_type) = &op {
                Ok(schema.expand_projection(tag_type))
            } else {
                // それ以外の Operand (Calculation, Aggregation, Query) は展開
                let expanded_op = expand_operand(schema, op)?;

                // Calculation の中に Query(And([filter, proj])) がある場合、書き換え
                if let Operand::Calculation(calc) = &expanded_op {
                    if let Some(rewritten) =
                        try_rewrite_filtered_calculation(calc)
                    {
                        return Ok(rewritten);
                    }
                }

                Ok(QueryNode::Projection(expanded_op))
            }
        }
        QueryNode::And(nodes) => {
            validate_set_operation_operands(&nodes, "&")?;
            let mut expanded = Vec::new();
            for n in nodes {
                expanded.push(expand_query_node(schema, n)?);
            }
            Ok(QueryNode::And(expanded))
        }
        QueryNode::Or(nodes) => {
            validate_set_operation_operands(&nodes, "|")?;
            let mut expanded = Vec::new();
            for n in nodes {
                expanded.push(expand_query_node(schema, n)?);
            }
            Ok(QueryNode::Or(expanded))
        }
        QueryNode::Difference(l, r) => {
            validate_set_operation_operands(&[*l.clone(), *r.clone()], "-")?;
            Ok(QueryNode::Difference(
                Box::new(expand_query_node(schema, *l)?),
                Box::new(expand_query_node(schema, *r)?),
            ))
        }
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
    cmp.first = expand_operand(schema, cmp.first)?;

    // Restオペランドの展開
    // ここに来る時は rest.len() == 1 または 0 のはず
    let mut expanded_rest = Vec::new();
    for (op, operand) in cmp.rest {
        let expanded_operand = expand_operand(schema, operand)?;
        expanded_rest.push((op, expanded_operand));
    }
    cmp.rest = expanded_rest;

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

fn validate_operand(
    operand: &Operand,
    schema: &impl LogicalSchema,
) -> Result<()> {
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
        Operand::Query(node) => {
            // 括弧で囲まれた式が算術演算のコンテキストで使用される場合、Projection を返す必要がある
            if !returns_projection(node) {
                bail!(error::PARENTHESIZED_EXPR_IN_COMPARISON_MUST_RETURN_PROJECTION);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// 算術演算の型チェック（論理レベル）
/// Any 型はスキーマに登録されていないカスタムタグを許容するために数値として扱う
pub fn validate_calculation(
    calc: &CalculationNode,
    schema: &impl LogicalSchema,
) -> Result<()> {
    let left_type = infer_type(&calc.left, schema)?;
    let right_type = infer_type(&calc.right, schema)?;

    // Any 型はカスタムタグ（width:, height: など）を許容するために数値として扱う
    // 実際の型チェックはデータベース実行時に行われる
    let left_ok = left_type.is_numeric() || left_type == LogicalType::Any;
    let right_ok = right_type.is_numeric() || right_type == LogicalType::Any;

    if !left_ok || !right_ok {
        bail!(error::ARITHMETIC_ONLY_NUMERIC);
    }
    Ok(())
}

/// QueryNodeが集合を返すか（true）、スカラーを返すか（false）を判定
fn is_set_operation(node: &QueryNode) -> bool {
    match node {
        // 集合を返す
        QueryNode::And(_)
        | QueryNode::Or(_)
        | QueryNode::Difference(_, _)
        | QueryNode::Complement(_)
        | QueryNode::TypedTag(_)
        | QueryNode::ColumnMatch { .. }
        | QueryNode::Projection(_) => true,

        // ラベル比較は集合を返す、スカラー比較は真偽値を返す
        QueryNode::Comparison(cmp) => {
            // すべての演算子がラベル比較（ComparisonOp::Label）ならば集合を返す
            cmp.rest
                .iter()
                .all(|(op, _)| matches!(op, ComparisonOp::Label(_)))
        }

        // スカラーを返す
        QueryNode::Aggregation(_) => false,
    }
}

/// 集合演算のオペランド型検証
fn validate_set_operation_operands(
    nodes: &[QueryNode],
    operation: &str,
) -> Result<()> {
    // 各オペランドが集合を返すかチェック
    let set_flags: Vec<bool> =
        nodes.iter().map(|n| is_set_operation(n)).collect();

    for (idx, &is_set) in set_flags.iter().enumerate() {
        if !is_set {
            let node = &nodes[idx];
            let operand_type = match node {
                QueryNode::Comparison(_) => "scalar comparison",
                QueryNode::Aggregation(_) => "aggregation function",
                _ => "scalar value",
            };

            // 二項演算の場合、left_is_set と right_is_set を渡す
            let (left_is_set, right_is_set) = if nodes.len() == 2 {
                (set_flags[0], set_flags[1])
            } else {
                (false, false)
            };

            bail!(error::invalid_set_operation_operand_msg(
                nodes,
                operation,
                operand_type,
                left_is_set,
                right_is_set,
            ));
        }
    }
    Ok(())
}

/// Calculation の中に Query(And([filter, Projection])) がある場合、書き換えます。
///
/// 例: Projection(Calculation(Query(And([filter, proj])), *, 2))
///  → And([filter, Projection(Calculation(proj, *, 2))])
fn try_rewrite_filtered_calculation(
    calc: &CalculationNode,
) -> Option<QueryNode> {
    // left が Query(And([filter, Projection])) の場合
    if let Operand::Query(node) = &calc.left {
        if let QueryNode::And(nodes) = &**node {
            // And の中から filter と Projection を抽出
            let (filters, projs): (Vec<_>, Vec<_>) = nodes
                .iter()
                .cloned()
                .partition(|n| !matches!(n, QueryNode::Projection(_)));

            if !projs.is_empty() && !filters.is_empty() {
                // 最初の Projection を取り出す
                if let QueryNode::Projection(proj_op) = &projs[0] {
                    // 新しい Calculation を作成: Calculation(proj, op, right)
                    let new_calc = CalculationNode {
                        left: proj_op.clone(),
                        op: calc.op,
                        right: calc.right.clone(),
                    };

                    // And([filters..., Projection(new_calc)])
                    let mut new_nodes = filters;
                    new_nodes.push(QueryNode::Projection(
                        Operand::Calculation(Box::new(new_calc)),
                    ));

                    return Some(QueryNode::And(new_nodes));
                }
            }
        }
    }

    // right が Query(And([filter, Projection])) の場合も同様
    if let Operand::Query(node) = &calc.right {
        if let QueryNode::And(nodes) = &**node {
            let (filters, projs): (Vec<_>, Vec<_>) = nodes
                .iter()
                .cloned()
                .partition(|n| !matches!(n, QueryNode::Projection(_)));

            if !projs.is_empty() && !filters.is_empty() {
                if let QueryNode::Projection(proj_op) = &projs[0] {
                    let new_calc = CalculationNode {
                        left: calc.left.clone(),
                        op: calc.op,
                        right: proj_op.clone(),
                    };

                    let mut new_nodes = filters;
                    new_nodes.push(QueryNode::Projection(
                        Operand::Calculation(Box::new(new_calc)),
                    ));

                    return Some(QueryNode::And(new_nodes));
                }
            }
        }
    }

    None
}

/// QueryNodeが意味論的にProjectionを返すかどうかを判定します。
///
/// 集合演算の意味論:
/// - TypedTag & TypedTag → アイテム集合
/// - TypedTag & Projection → Projection (フィルタ付き)
/// - Projection & Projection → Projection
/// - Comparison & Projection → Projection (フィルタ付き)
/// - TypedTag | TypedTag → アイテム集合
/// - TypedTag | Projection → Projection (複数の可能性)
/// - Projection | Projection → Projection
///
/// 注: Comparison, TypedTag, ColumnMatch は意味的にアイテム集合を返すため、
/// 集合演算と組み合わせた場合の動作は TypedTag と同様に扱う。
fn returns_projection(node: &QueryNode) -> bool {
    match node {
        QueryNode::Projection(_) => true,
        QueryNode::And(nodes) | QueryNode::Or(nodes) => {
            // 少なくとも1つがProjectionなら、結果もProjection
            nodes.iter().any(returns_projection)
        }
        QueryNode::Difference(l, _) => {
            // 差集合の左辺がProjectionなら、結果もProjection
            returns_projection(l)
        }
        QueryNode::Complement(_) => {
            // 補集合は通常アイテム集合を返す
            false
        }
        QueryNode::TypedTag(_)
        | QueryNode::ColumnMatch { .. }
        | QueryNode::Comparison(_)
        | QueryNode::Aggregation(_) => false,
    }
}

/// Operandを論理展開し、必要に応じて検証を行います。
fn expand_operand(
    schema: &impl LogicalSchema,
    operand: Operand,
) -> Result<Operand> {
    match operand {
        Operand::Literal(label) => Ok(Operand::Literal(label)),
        Operand::TypeRef(tag_type) => Ok(Operand::TypeRef(tag_type)),
        Operand::Calculation(calc) => {
            let expanded = expand_calculation(schema, *calc)?;
            Ok(Operand::Calculation(Box::new(expanded)))
        }
        Operand::Aggregation(agg) => {
            let expanded = expand_aggregation(schema, *agg)?;
            Ok(Operand::Aggregation(Box::new(expanded)))
        }
        Operand::Query(node) => {
            // 括弧で囲まれた式を論理展開
            let expanded_node = expand_query_node(schema, *node)?;

            // Queryが算術演算のコンテキストで使用される場合、Projectionを返す必要がある
            if !returns_projection(&expanded_node) {
                bail!(error::PARENTHESIZED_EXPR_MUST_RETURN_PROJECTION);
            }

            Ok(Operand::Query(Box::new(expanded_node)))
        }
    }
}

/// 算術演算ノードを論理展開します。
fn expand_calculation(
    schema: &impl LogicalSchema,
    calc: CalculationNode,
) -> Result<CalculationNode> {
    let left = expand_operand(schema, calc.left)?;
    let right = expand_operand(schema, calc.right)?;
    Ok(CalculationNode {
        left,
        op: calc.op,
        right,
    })
}

/// 集約ノードを論理展開します。
fn expand_aggregation(
    schema: &impl LogicalSchema,
    agg: AggregationNode,
) -> Result<AggregationNode> {
    match agg {
        AggregationNode::Count(node) => {
            let expanded = expand_query_node(schema, *node)?;
            Ok(AggregationNode::Count(Box::new(expanded)))
        }
        AggregationNode::Arithmetic { op, inner } => {
            let expanded = expand_query_node(schema, *inner)?;
            Ok(AggregationNode::Arithmetic {
                op,
                inner: Box::new(expanded),
            })
        }
    }
}

/// オペランドの型を推論
fn infer_type(
    operand: &Operand,
    schema: &impl LogicalSchema,
) -> Result<LogicalType> {
    match operand {
        Operand::Literal(label) => Ok(infer_logical_type_from_label(label)),
        Operand::TypeRef(tag_type) => Ok(schema.get_logical_type(tag_type)),
        Operand::Calculation(calc) => {
            validate_calculation(calc, schema)?;
            Ok(LogicalType::Integer) // 暫定：演算結果は一旦数値
        }
        Operand::Aggregation(_) => Ok(LogicalType::Integer),
        Operand::Query(node) => {
            // 括弧で囲まれた式の型を推論
            // Projection を返す必要がある (returns_projection でチェック済み)
            // Projection の場合、その中の Operand の型を推論
            // 集合演算の場合、結果の Projection の型を推論
            match &**node {
                QueryNode::Projection(op) => infer_type(op, schema),
                QueryNode::And(nodes) | QueryNode::Or(nodes) => {
                    // 集合演算の中から Projection を見つけて型を推論
                    for n in nodes {
                        if let QueryNode::Projection(op) = n {
                            return infer_type(op, schema);
                        }
                    }
                    // 見つからない場合は Any (実際には returns_projection で保証されている)
                    Ok(LogicalType::Any)
                }
                QueryNode::Difference(l, _) => {
                    // 差集合の左辺から型を推論（再帰）
                    match &**l {
                        QueryNode::Projection(op) => infer_type(op, schema),
                        _ => Ok(LogicalType::Any),
                    }
                }
                _ => {
                    // returns_projection が true であることが保証されているはずだが、
                    // ここに到達する場合は念のため Any を返す
                    Ok(LogicalType::Any)
                }
            }
        }
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
    use crate::query::ast::{
        CalculationNode, ComparisonNode, Operand, QueryNode,
    };
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
                crate::query::ast::ComparisonOp::Scalar(
                    crate::query::ast::BasicOp::Gt,
                ),
                Operand::Literal(crate::types::Label::from(100i64)),
            )],
        };
        let node = QueryNode::Comparison(cmp);
        let result = expand_query_node(&lens, node);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(
            "Arithmetic operations are only possible for numeric types"
        ));
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
                (
                    ComparisonOp::Scalar(BasicOp::Lt),
                    Operand::TypeRef(TagType::from("size")),
                ),
                (
                    ComparisonOp::Scalar(BasicOp::Le),
                    Operand::Literal(crate::types::Label::from(100i64)),
                ),
            ],
        };

        let node = QueryNode::Comparison(cmp);
        let expanded = expand_query_node(&lens, node).unwrap();

        // 期待値: And([Comparison(10 < size:), Comparison(size: <= 100)])
        if let QueryNode::And(nodes) = expanded {
            assert_eq!(nodes.len(), 2);
            // 内部の各 ComparisonNode は再帰的に展開されているはず
        } else {
            panic!(
                "Expected QueryNode::And for chain comparison, got {:?}",
                expanded
            );
        }
    }
}
