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
    AggregationNode, ArithmeticOp, CalculationNode, ComparisonNode,
    ComparisonOp, NestNode, Operand, QueryNode,
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
        matches!(self, Self::Integer | Self::Float | Self::Boolean)
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
            // 括弧で囲まれた式 (sum > 10) などが Projection(Query(Comparison)) としてパースされる場合がある
            // 中身がプロジェクションを返さない（アイテムセットを返すフィルター）なら、ラップを剥がす
            if let Operand::Query(node) = &op {
                if !returns_projection(node) {
                    return expand_query_node(schema, *node.clone());
                }
            }
            // TypeRef の場合は schema による展開を適用
            if let Operand::TypeRef(tag_type) = &op {
                Ok(schema.expand_projection(tag_type))
            } else {
                // それ以外の Operand (Calculation, Aggregation, Query) は展開
                let expanded_op = expand_operand(schema, op)?;

                // Operand 内に埋もれた Nest フィルタを抽出
                let mut lifted_filters = Vec::new();
                let cleaned_op = strip_filters_from_operand(
                    expanded_op,
                    &mut lifted_filters,
                );

                if lifted_filters.is_empty() {
                    Ok(QueryNode::Projection(cleaned_op))
                } else {
                    lifted_filters.push(QueryNode::Projection(cleaned_op));
                    Ok(QueryNode::And(lifted_filters))
                }
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
        QueryNode::Comparison(cmp) => {
            expand_comparison_with_recursion(schema, cmp)
        }
        QueryNode::Aggregation(agg) => {
            let expanded = expand_aggregation(schema, agg)?;
            // Unnest 可能な集約（inner に Nest を含む）は Nest/Projection 形式に変換する。
            // And/Or で包まれた内部の Nest も透過的に扱う（Issue #4 対応）。
            if is_unnestable_aggregation(&expanded) {
                let unnested = unnest_aggregation(expanded);
                Ok(expand_query_node(schema, unnested)?)
            } else {
                Ok(QueryNode::Aggregation(expanded))
            }
        }
        QueryNode::Nest(nest) => Ok(expand_nest(schema, nest)?),
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

    // Operand 内に埋もれた Nest フィルタを抽出
    let mut lifted_filters = Vec::new();
    cmp.first = strip_filters_from_operand(cmp.first, &mut lifted_filters);
    let mut new_rest = Vec::new();
    for (op, operand) in cmp.rest {
        let stripped = strip_filters_from_operand(operand, &mut lifted_filters);
        new_rest.push((op, stripped));
    }
    cmp.rest = new_rest;

    // 標準の比較ノード展開（日付範囲化など）を実行
    let reg = QueryFunctionRegistry::with_standard();
    let expanded_node = expand_comparison_node(cmp, &reg);

    // 抽出したフィルタがあれば Comparison 全体を And で包む
    if lifted_filters.is_empty() {
        Ok(expanded_node)
    } else {
        lifted_filters.push(expanded_node);
        Ok(QueryNode::And(lifted_filters))
    }
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
// Any 型はスキーマに登録されていないカスタムタグを許容するために数値として扱う
pub fn validate_calculation(
    calc: &CalculationNode,
    schema: &impl LogicalSchema,
) -> Result<()> {
    let left_type = infer_type(&calc.left, schema)?;
    let right_type = infer_type(&calc.right, schema)?;

    let left_is_str = left_type == LogicalType::String;
    let right_is_str = right_type == LogicalType::String;

    // String と non-String の混合演算はエラー
    if left_is_str != right_is_str {
        return Err(error::unsupported_mixed_type_arithmetic(
            if left_is_str { "String" } else { "non-String" },
            if right_is_str { "String" } else { "non-String" },
        )
        .into());
    }

    if left_is_str && right_is_str {
        // String 同士の場合、+ と * のみ許可
        if !matches!(
            calc.op,
            crate::query::ast::ArithmeticOp::Add
                | crate::query::ast::ArithmeticOp::Mul
        ) {
            return Err(error::unsupported_string_arithmetic(&format!(
                "{:?}",
                calc.op
            ))
            .into());
        }
    } else {
        // 数値演算の場合
        let left_ok = left_type.is_numeric() || left_type == LogicalType::Any;
        let right_ok =
            right_type.is_numeric() || right_type == LogicalType::Any;

        if !left_ok || !right_ok {
            bail!(error::ARITHMETIC_ONLY_NUMERIC);
        }
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
        | QueryNode::TypedTag(_)
        | QueryNode::ColumnMatch { .. } => true,

        // Projection は集合を返すが、Literal のみの場合はスカラー
        QueryNode::Projection(op) => !matches!(op, Operand::Literal(_)),

        // ラベル比較は集合を返す、スカラー比較は真偽値を返す
        QueryNode::Comparison(cmp) => {
            // すべての演算子がラベル比較（ComparisonOp::Label）ならば集合を返す
            cmp.rest
                .iter()
                .all(|(op, _)| matches!(op, ComparisonOp::Label(_)))
        }

        // スカラーを返す
        QueryNode::Aggregation(_) => false,

        // Nestは集合を返す
        QueryNode::Nest(_) => true,
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
/// クエリノードから再帰的にプロジェクションのオペランドを探します
fn find_projection_operand(node: &QueryNode) -> Option<&Operand> {
    match node {
        QueryNode::Projection(op) => Some(op),
        QueryNode::And(nodes) | QueryNode::Or(nodes) => {
            nodes.iter().find_map(find_projection_operand)
        }
        QueryNode::Difference(l, _) => find_projection_operand(l),
        _ => None,
    }
}

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
        QueryNode::TypedTag(_)
        | QueryNode::ColumnMatch { .. }
        | QueryNode::Comparison(_)
        | QueryNode::Aggregation(_) => false,
        QueryNode::Nest(nest) => returns_projection(&nest.left),
    }
}

/// Operandを論理展開し、必要に応じて検証を行います。
fn expand_operand(
    schema: &impl LogicalSchema,
    operand: Operand,
) -> Result<Operand> {
    match operand {
        Operand::Literal(label) => {
            // 文字列 "true", "false" を Boolean 型に正規化
            let s = label.as_str();
            if s == "true" {
                Ok(Operand::Literal(crate::types::Label::from(true)))
            } else if s == "false" {
                Ok(Operand::Literal(crate::types::Label::from(false)))
            } else if let Some(bytes) = crate::util::parse_size(&s) {
                // サイズ単位付きリテラル ("1M", "10TB" 等) を数値に正規化
                Ok(Operand::Literal(crate::types::Label::from(bytes)))
            } else {
                Ok(Operand::Literal(label))
            }
        }
        Operand::TypeRef(tag_type) => Ok(Operand::TypeRef(tag_type)),
        Operand::Calculation(calc) => {
            let expanded = expand_calculation(schema, *calc)?;
            Ok(try_fold_calculation(expanded))
        }
        Operand::Aggregation(agg) => {
            let expanded = expand_aggregation(schema, *agg)?;
            Ok(Operand::Aggregation(Box::new(expanded)))
        }
        Operand::Query(node) => {
            // 括弧で囲まれた式を論理展開
            let flattened = expand_query_node(schema, *node)?;

            // Queryが算術演算のコンテキストで使用される場合、Projectionを返す必要がある
            if !returns_projection(&flattened) {
                bail!(error::PARENTHESIZED_EXPR_MUST_RETURN_PROJECTION);
            }

            Ok(Operand::Query(Box::new(flattened)))
        }
    }
}

/// Operand 内に埋もれた And([filters, core]) からフィルタを抽出し、
/// core のみの Operand に置き換えます。Calculation 内も再帰的に処理します。
fn strip_filters_from_operand(
    operand: Operand,
    filters: &mut Vec<QueryNode>,
) -> Operand {
    match operand {
        Operand::Query(node) => {
            let nodes = match *node {
                QueryNode::And(nodes) => nodes,
                QueryNode::Projection(op) => return op,
                other => return Operand::Query(Box::new(other)),
            };
            let (filter_nodes, core_nodes): (Vec<_>, Vec<_>) =
                nodes.into_iter().partition(|n| !returns_projection(n));
            filters.extend(filter_nodes);
            let core = match core_nodes.len() {
                1 => core_nodes.into_iter().next().unwrap(),
                _ => return Operand::Query(Box::new(QueryNode::And(core_nodes))),
            };
            match core {
                QueryNode::Projection(op) => op,
                other => Operand::Query(Box::new(other)),
            }
        }
        Operand::Calculation(calc) => {
            let left = strip_filters_from_operand(calc.left, filters);
            let right = strip_filters_from_operand(calc.right, filters);
            Operand::Calculation(Box::new(CalculationNode {
                left,
                op: calc.op,
                right,
            }))
        }
        other => other,
    }
}

/// 算術演算ノードを論理展開します。
/// 両辺がリテラルの場合は定数畳み込みを行い、結果のリテラルを返します。
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

/// 両辺がリテラルの場合、算術演算を定数畳み込みして結果の Operand::Literal を返します。
/// TagRef や Aggregation を含む場合は Operand::Calculation のまま返します。
fn try_fold_calculation(calc: CalculationNode) -> Operand {
    if let (Operand::Literal(left), Operand::Literal(right)) =
        (&calc.left, &calc.right)
    {
        if let Some(result) = fold_literal_arithmetic(left, calc.op, right) {
            return Operand::Literal(result);
        }
    }
    Operand::Calculation(Box::new(calc))
}

/// リテラル同士の算術演算を計算します。
fn fold_literal_arithmetic(
    left: &crate::types::Label,
    op: ArithmeticOp,
    right: &crate::types::Label,
) -> Option<crate::types::Label> {
    use crate::types::LabelValue;

    let lv = left.value();
    let rv = right.value();

    match (lv, rv) {
        (LabelValue::Integer(l), LabelValue::Integer(r)) => {
            let result = match op {
                ArithmeticOp::Add => l.checked_add(r)?,
                ArithmeticOp::Sub => l.checked_sub(r)?,
                ArithmeticOp::Mul => l.checked_mul(r)?,
                ArithmeticOp::Div => {
                    if r == 0 {
                        return None;
                    }
                    l.checked_div(r)?
                }
                ArithmeticOp::Mod => {
                    if r == 0 {
                        return None;
                    }
                    l.checked_rem(r)?
                }
            };
            Some(crate::types::Label::from(result))
        }
        (LabelValue::Double(l_bits), LabelValue::Double(r_bits)) => {
            let l = f64::from_bits(l_bits);
            let r = f64::from_bits(r_bits);
            let result = match op {
                ArithmeticOp::Add => l + r,
                ArithmeticOp::Sub => l - r,
                ArithmeticOp::Mul => l * r,
                ArithmeticOp::Div => l / r,
                ArithmeticOp::Mod => l % r,
            };
            Some(double_label(result))
        }
        (LabelValue::Integer(l), LabelValue::Double(r_bits)) => {
            let r = f64::from_bits(r_bits);
            let result = match op {
                ArithmeticOp::Add => (l as f64) + r,
                ArithmeticOp::Sub => (l as f64) - r,
                ArithmeticOp::Mul => (l as f64) * r,
                ArithmeticOp::Div => (l as f64) / r,
                ArithmeticOp::Mod => (l as f64) % r,
            };
            Some(double_label(result))
        }
        (LabelValue::Double(l_bits), LabelValue::Integer(r)) => {
            let l = f64::from_bits(l_bits);
            let result = match op {
                ArithmeticOp::Add => l + (r as f64),
                ArithmeticOp::Sub => l - (r as f64),
                ArithmeticOp::Mul => l * (r as f64),
                ArithmeticOp::Div => l / (r as f64),
                ArithmeticOp::Mod => l % (r as f64),
            };
            Some(double_label(result))
        }
        (LabelValue::Boolean(l), LabelValue::Boolean(r)) => {
            // Boolean は FALSE=0, TRUE=1 として計算、結果は Integer
            let li = if l { 1i64 } else { 0 };
            let ri = if r { 1i64 } else { 0 };
            let result = match op {
                ArithmeticOp::Add => li + ri,
                ArithmeticOp::Sub => li - ri,
                ArithmeticOp::Mul => li * ri,
                ArithmeticOp::Div => {
                    if ri == 0 {
                        return None;
                    }
                    li / ri
                }
                ArithmeticOp::Mod => {
                    if ri == 0 {
                        return None;
                    }
                    li % ri
                }
            };
            Some(crate::types::Label::from(result))
        }
        (LabelValue::String(l), LabelValue::String(r))
        | (LabelValue::Literal(l), LabelValue::Literal(r))
        | (LabelValue::String(l), LabelValue::Literal(r))
        | (LabelValue::Literal(l), LabelValue::String(r)) => match op {
            ArithmeticOp::Add => {
                Some(crate::types::Label::from(format!("{}, {}", l, r)))
            }
            ArithmeticOp::Mul => {
                Some(crate::types::Label::from(format!("{}{}", l, r)))
            }
            _ => None, // -, / はバリデーションで弾かれているはず
        },
        _ => None,
    }
}

/// f64 値を Label に変換するヘルパー
fn double_label(v: f64) -> crate::types::Label {
    Label::Other(
        TagType::Custom(String::new()),
        LabelValue::Double(v.to_bits()),
    )
}

/// And ノードから投影ノードとそれ以外のフィルタを分離します。
/// 左辺が And 以外の場合はフィルタなしとして返します。
fn split_projection_filter(node: QueryNode) -> (QueryNode, Option<QueryNode>) {
    match node {
        QueryNode::And(mut nodes) => {
            let proj_idx = nodes.iter().position(|n| returns_projection(n));
            if let Some(idx) = proj_idx {
                let proj = nodes.remove(idx);
                let filter = match nodes.len() {
                    0 => None,
                    1 => Some(nodes.remove(0)),
                    _ => Some(QueryNode::And(nodes)),
                };
                (proj, filter)
            } else {
                // 投影が見つからない場合はそのまま返す
                (QueryNode::And(nodes), None)
            }
        }
        _ => (node, None),
    }
}

/// QueryNode 内の全ての AggregationNode の内部クエリにフィルタを And 結合で注入します。
fn inject_filter_into_aggregations(
    node: QueryNode,
    filter: &QueryNode,
) -> QueryNode {
    match node {
        QueryNode::Aggregation(agg) => {
            let injected = match agg {
                AggregationNode::Count(inner) => AggregationNode::Count(
                    Box::new(wrap_with_filter(*inner, filter)),
                ),
                AggregationNode::Arithmetic { op, inner } => {
                    AggregationNode::Arithmetic {
                        op,
                        inner: Box::new(wrap_with_filter(*inner, filter)),
                    }
                }
            };
            QueryNode::Aggregation(injected)
        }
        QueryNode::Comparison(cmp) => {
            let new_first = inject_filter_into_operand(cmp.first, filter);
            let new_rest = cmp
                .rest
                .into_iter()
                .map(|(op, operand)| {
                    (op, inject_filter_into_operand(operand, filter))
                })
                .collect();
            QueryNode::Comparison(ComparisonNode {
                first: new_first,
                rest: new_rest,
            })
        }
        QueryNode::And(nodes) => QueryNode::And(
            nodes
                .into_iter()
                .map(|n| inject_filter_into_aggregations(n, filter))
                .collect(),
        ),
        QueryNode::Or(nodes) => QueryNode::Or(
            nodes
                .into_iter()
                .map(|n| inject_filter_into_aggregations(n, filter))
                .collect(),
        ),
        QueryNode::Nest(mut nest) => {
            nest.right =
                Box::new(inject_filter_into_aggregations(*nest.right, filter));
            QueryNode::Nest(nest)
        }
        QueryNode::Projection(Operand::Calculation(mut calc)) => {
            calc.left = inject_filter_into_operand(calc.left, filter);
            calc.right = inject_filter_into_operand(calc.right, filter);
            QueryNode::Projection(Operand::Calculation(calc))
        }
        _ => node,
    }
}

/// Operand 内の集約ノードにフィルタを注入します。
fn inject_filter_into_operand(operand: Operand, filter: &QueryNode) -> Operand {
    match operand {
        Operand::Aggregation(agg) => {
            match inject_filter_into_aggregations(
                QueryNode::Aggregation(*agg),
                filter,
            ) {
                QueryNode::Aggregation(a) => Operand::Aggregation(Box::new(a)),
                _ => unreachable!(),
            }
        }
        Operand::Calculation(mut calc) => {
            calc.left = inject_filter_into_operand(calc.left, filter);
            calc.right = inject_filter_into_operand(calc.right, filter);
            Operand::Calculation(calc)
        }
        Operand::Query(q) => Operand::Query(Box::new(
            inject_filter_into_aggregations(*q, filter),
        )),
        _ => operand,
    }
}

/// 既存のノードをフィルタと And 結合で包みます。
/// 既存ノードが And の場合は要素として追記します。
fn wrap_with_filter(node: QueryNode, filter: &QueryNode) -> QueryNode {
    match node {
        QueryNode::And(mut nodes) => {
            nodes.insert(0, filter.clone());
            QueryNode::And(nodes)
        }
        _ => QueryNode::And(vec![filter.clone(), node]),
    }
}

/// Nest ノードの論理展開。
///
/// 左辺をそのまま維持し、右辺(Comparison, And(連鎖比較), Calculation)の各要素へ
/// 左辺のコンテキスト全体を素直に分配します。
/// 左辺がフィルタ付き（And展開後）の場合、フィルタを右辺の集計ノードにも注入します。
fn expand_nest(
    schema: &impl LogicalSchema,
    nest: NestNode,
) -> Result<QueryNode> {
    let left_raw = expand_query_node(schema, *nest.left)?;
    let right = expand_query_node(schema, *nest.right)?;

    // 左辺からフィルタを分離
    let (left, filter) = split_projection_filter(left_raw);

    // 右辺にフィルタを注入（集計ノード内部に And 結合）
    let right_with_filter = match &filter {
        Some(f) => inject_filter_into_aggregations(right, f),
        None => right,
    };

    let result = match right_with_filter {
        QueryNode::Comparison(cmp) => {
            expand_nest_comparison(left.clone(), cmp)?
        }
        QueryNode::And(mut nodes) => {
            // 右辺が And の場合、プロジェクションが含まれているか確認
            let mut projections = Vec::new();
            let mut filters = Vec::new();
            let mut has_comparison = false;

            for node in nodes.drain(..) {
                if node.get_projections().is_empty() {
                    if matches!(node, QueryNode::Comparison(_)) {
                        has_comparison = true;
                    }
                    filters.push(node);
                } else {
                    projections.push(node);
                }
            }

            if !projections.is_empty() && !has_comparison {
                // プロジェクションが含まれる場合、フィルタを左辺のコンテキストに寄せる
                let sub_filter = if filters.is_empty() {
                    None
                } else if filters.len() == 1 {
                    Some(filters.remove(0))
                } else {
                    Some(QueryNode::And(filters))
                };

                // 右辺のプロジェクション要素を結合
                let right_proj = if projections.len() == 1 {
                    projections.remove(0)
                } else {
                    QueryNode::And(projections)
                };

                // 左辺に右辺由来のフィルタを注入
                let left_with_sub_filter = match sub_filter {
                    Some(sf) => inject_filter_into_aggregations(left, &sf),
                    None => left,
                };

                QueryNode::Nest(NestNode {
                    left: Box::new(left_with_sub_filter),
                    right: Box::new(right_proj),
                })
            } else {
                // プロジェクションがない、または単純な比較の And の場合は、各要素に左辺を分配する（従来通り）
                let mut distributed = Vec::new();
                for node in filters.into_iter().chain(projections.into_iter()) {
                    match node {
                        QueryNode::Comparison(c) => {
                            distributed
                                .push(expand_nest_comparison(left.clone(), c)?);
                        }
                        _ => {
                            distributed.push(QueryNode::Nest(NestNode {
                                left: Box::new(left.clone()),
                                right: Box::new(node),
                            }));
                        }
                    }
                }
                QueryNode::And(distributed)
            }
        }
        // 右辺が Projection(Calculation) → 算術演算の両辺に Nest キーを分配する
        QueryNode::Projection(Operand::Calculation(calc))
            if calc_has_only_aggregations_and_literals(&calc) =>
        {
            let distributed = distribute_nest_over_calc(&left, *calc);
            QueryNode::Projection(Operand::Calculation(Box::new(distributed)))
        }
        _ => QueryNode::Nest(NestNode {
            left: Box::new(left),
            right: Box::new(right_with_filter),
        }),
    };

    // フィルタを全体にも適用（Projection 結果の絞り込み用）
    match filter {
        Some(f) => Ok(QueryNode::And(vec![f, result])),
        None => Ok(result),
    }
}

/// Calculation 内に集約関数とリテラルのみが含まれ、TypeRef(カラム参照)が含まれていないかを判定します。
/// 分配対象のガード条件として使用します。
fn calc_has_only_aggregations_and_literals(calc: &CalculationNode) -> bool {
    operand_has_only_aggregations_and_literals(&calc.left)
        && operand_has_only_aggregations_and_literals(&calc.right)
}

fn operand_has_only_aggregations_and_literals(operand: &Operand) -> bool {
    match operand {
        Operand::Literal(_) | Operand::Aggregation(_) => true,
        Operand::Calculation(c) => calc_has_only_aggregations_and_literals(c),
        Operand::Query(q) => match &**q {
            QueryNode::Projection(op) => {
                operand_has_only_aggregations_and_literals(op)
            }
            QueryNode::Aggregation(_) => true,
            _ => false,
        },
        Operand::TypeRef(_) => false,
    }
}

/// Nest の右辺にある単一 Comparison を分配するヘルパー。
fn expand_nest_comparison(
    left: QueryNode,
    cmp: ComparisonNode,
) -> Result<QueryNode> {
    let first = distribute_nest_over_operand(&left, cmp.first);

    let mut rest = Vec::new();
    for (op, operand) in cmp.rest {
        let wrapped = distribute_nest_over_operand(&left, operand);
        // スカラー比較をラベル比較に変換（Nest結果はProjectionなので）
        let label_op = match op {
            ComparisonOp::Scalar(basic) => ComparisonOp::Label(basic),
            label @ ComparisonOp::Label(_) => label,
        };
        rest.push((label_op, wrapped));
    }

    Ok(QueryNode::Comparison(ComparisonNode { first, rest }))
}

/// Nest を算術 Calculation の全オペランドに再帰的に分配する。
/// Nest(L, Calc{agg1, op, agg2}) → Calc{Query(Nest(L,agg1)), op, Query(Nest(L,agg2))}
fn distribute_nest_over_calc(
    key: &QueryNode,
    calc: CalculationNode,
) -> CalculationNode {
    CalculationNode {
        left: distribute_nest_over_operand(key, calc.left),
        op: calc.op,
        right: distribute_nest_over_operand(key, calc.right),
    }
}

/// Operand に Nest キーを分配する。
/// - Aggregation → Query(Nest(key, Aggregation))
/// - Calculation → 再帰的に両辺に分配
/// - Literal     → そのまま（スカラーは Nest 不要）
/// - TypeRef     → Query(Nest(key, Projection(TypeRef)))
/// - Query       → Query(Nest(key, Query))
fn distribute_nest_over_operand(key: &QueryNode, operand: Operand) -> Operand {
    match operand {
        Operand::Literal(l) => Operand::Literal(l),
        Operand::Aggregation(agg) => {
            Operand::Query(Box::new(QueryNode::Nest(NestNode {
                left: Box::new(key.clone()),
                right: Box::new(QueryNode::Aggregation(*agg)),
            })))
        }
        Operand::Calculation(calc) => Operand::Calculation(Box::new(
            distribute_nest_over_calc(key, *calc),
        )),
        Operand::TypeRef(tt) => {
            Operand::Query(Box::new(QueryNode::Nest(NestNode {
                left: Box::new(key.clone()),
                right: Box::new(QueryNode::Projection(Operand::TypeRef(tt))),
            })))
        }
        Operand::Query(node) => {
            Operand::Query(Box::new(QueryNode::Nest(NestNode {
                left: Box::new(key.clone()),
                right: node,
            })))
        }
    }
}

/// Nestの右辺がunnest対象かを判定します。
/// Projection, Nest, または And/Or/Difference/Comparison の中に Projection/Nest を含む場合に true を返します。
fn is_unnestable_right(right: &QueryNode) -> bool {
    match right {
        QueryNode::Nest(_) => true,
        QueryNode::Projection(op) => match op {
            Operand::TypeRef(_) => true,
            Operand::Query(node) => is_unnestable_right(node),
            _ => false,
        },
        QueryNode::And(nodes) | QueryNode::Or(nodes) => {
            nodes.iter().any(is_unnestable_right)
        }
        QueryNode::Difference(l, _) => is_unnestable_right(l),
        QueryNode::Comparison(cmp) => {
            operand_is_nest(&cmp.first)
                || cmp.rest.iter().any(|(_, op)| operand_is_nest(op))
        }
        _ => false,
    }
}

/// 集約の内部がNestで、かつ右辺がunnest可能かを判定します。
fn is_unnestable_aggregation(agg: &AggregationNode) -> bool {
    let inner = match agg {
        AggregationNode::Count(n) => n.as_ref(),
        AggregationNode::Arithmetic { inner, .. } => inner.as_ref(),
    };
    is_unnestable_inner(inner)
}

fn is_unnestable_inner(node: &QueryNode) -> bool {
    match node {
        QueryNode::Nest(n) => is_unnestable_right(&n.right),
        QueryNode::And(nodes) | QueryNode::Or(nodes) => {
            nodes.iter().any(is_unnestable_inner)
        }
        QueryNode::Difference(l, _) => is_unnestable_inner(l),
        // Comparison は NestMatch 比較として既に解決済みのため、アンネスト対象ではない
        QueryNode::Projection(Operand::Query(node)) => {
            is_unnestable_inner(node)
        }
        _ => false,
    }
}

fn unnest_inner(
    node: QueryNode,
    wrap: &dyn Fn(Box<QueryNode>) -> AggregationNode,
) -> QueryNode {
    match node {
        QueryNode::Nest(nest) if is_unnestable_right(&nest.right) => {
            QueryNode::Nest(NestNode {
                left: nest.left,
                right: Box::new(QueryNode::Aggregation(wrap(nest.right))),
            })
        }
        QueryNode::And(nodes) => {
            let mut done = false;
            let new_nodes = nodes
                .into_iter()
                .map(|n| {
                    if !done && is_unnestable_inner(&n) {
                        done = true;
                        unnest_inner(n, wrap)
                    } else {
                        n
                    }
                })
                .collect();
            QueryNode::And(new_nodes)
        }
        QueryNode::Or(nodes) => {
            let mut done = false;
            let new_nodes = nodes
                .into_iter()
                .map(|n| {
                    if !done && is_unnestable_inner(&n) {
                        done = true;
                        unnest_inner(n, wrap)
                    } else {
                        n
                    }
                })
                .collect();
            QueryNode::Or(new_nodes)
        }
        QueryNode::Difference(l, r) => {
            if is_unnestable_inner(&l) {
                QueryNode::Difference(Box::new(unnest_inner(*l, wrap)), r)
            } else {
                QueryNode::Difference(l, r)
            }
        }
        other => QueryNode::Aggregation(wrap(Box::new(other))),
    }
}

fn operand_is_nest(op: &Operand) -> bool {
    match op {
        Operand::Query(node) => matches!(node.as_ref(), QueryNode::Nest(_)),
        _ => false,
    }
}

/// agg(Nest(L, R)) → Nest(L, agg(R)) の書き換えを実行します。
/// inner が And/Or/Difference([Nest(L,R), ...]) の場合は And/Or/Difference([Nest(L, agg(R)), ...]) に変換します。
/// 呼び出し前に `is_unnestable_aggregation` で判定済みであること。
fn unnest_aggregation(agg: AggregationNode) -> QueryNode {
    match agg {
        AggregationNode::Count(inner) => {
            unnest_inner(*inner, &|r| AggregationNode::Count(r))
        }
        AggregationNode::Arithmetic { op, inner } => {
            unnest_inner(*inner, &|r| AggregationNode::Arithmetic {
                op,
                inner: r,
            })
        }
    }
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
            let left_type = infer_type(&calc.left, schema)?;
            let right_type = infer_type(&calc.right, schema)?;
            if left_type == LogicalType::String
                && right_type == LogicalType::String
            {
                Ok(LogicalType::String)
            } else {
                Ok(LogicalType::Integer) // 暫定：数値演算結果は一旦整数扱い
            }
        }
        Operand::Aggregation(agg) => match &**agg {
            AggregationNode::Count(_) => Ok(LogicalType::Integer),
            AggregationNode::Arithmetic { inner, .. } => {
                // inner の中からプロジェクションを探して型を推論
                if let Some(op) = find_projection_operand(inner) {
                    infer_type(op, schema)
                } else {
                    // プロジェクションがない場合は数値扱い（通常は集計エラーだが型推論としては Integer）
                    Ok(LogicalType::Integer)
                }
            }
        },
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
        LabelValue::Double(_) => LogicalType::Float,
        LabelValue::Null => LogicalType::Any,
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
        AggregationNode, ArithmeticOp, BasicOp, CalculationNode,
        ComparisonNode, ComparisonOp, NestNode, Operand, QueryNode,
    };
    use crate::query::lens_schema::Lens;
    use crate::types::{LabelValue, TagType, TypedTag};

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
        assert!(result.unwrap_err().to_string().contains("not allowed"));
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

        // path + "abc" (String + String) -> OK (Phase 3)
        let calc_str = CalculationNode {
            left: Operand::TypeRef(TagType::from("path")),
            op: crate::query::ast::ArithmeticOp::Add,
            right: Operand::Literal(crate::types::Label::from("abc")),
        };
        assert!(validate_calculation(&calc_str, &lens).is_ok());

        // path + 100 (String + Integer) -> Err (Mixed)
        let calc_err = CalculationNode {
            left: Operand::TypeRef(TagType::from("path")),
            op: crate::query::ast::ArithmeticOp::Add,
            right: Operand::Literal(crate::types::Label::from(100i64)),
        };
        let res = validate_calculation(&calc_err, &lens);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("not allowed"));

        // is_dir + 1 (Boolean + Integer) -> OK
        let calc_bool = CalculationNode {
            left: Operand::TypeRef(TagType::from("is_dir")),
            op: crate::query::ast::ArithmeticOp::Add,
            right: Operand::Literal(crate::types::Label::from(1i64)),
        };
        assert!(validate_calculation(&calc_bool, &lens).is_ok());
    }

    #[test]
    fn test_validate_calculation_mismatched_keys() {
        let lens = Lens::base_standard();
        // size: + mtime: (Numeric + Numeric, TypeRef 同士) -> OK
        // TypeRef は集計スコープを持たないため、異なるタグ名でもエラーにならない。
        let calc_ok = CalculationNode {
            left: Operand::TypeRef(TagType::from("size")),
            op: crate::query::ast::ArithmeticOp::Add,
            right: Operand::TypeRef(TagType::from("mtime")),
        };
        let res = validate_calculation(&calc_ok, &lens);
        assert!(
            res.is_ok(),
            "size: + mtime: should be allowed (row-level arithmetic)"
        );
    }

    #[test]
    fn test_logical_type_numeric() {
        assert!(LogicalType::Integer.is_numeric());
        assert!(LogicalType::Float.is_numeric());
        assert!(!LogicalType::String.is_numeric());
        assert!(LogicalType::Boolean.is_numeric()); // Should be True
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

    #[test]
    fn test_fold_pure_literal_calculation() {
        use crate::query::ast::ArithmeticOp;
        use crate::types::Label;

        let lens = Lens::base_standard();

        // (1 + 2) → Literal(3)
        let calc = CalculationNode {
            left: Operand::Literal(Label::from(3i64)),
            op: ArithmeticOp::Add,
            right: Operand::Literal(Label::from(2i64)),
        };
        let node = QueryNode::Projection(Operand::Calculation(Box::new(calc)));
        let expanded = expand_query_node(&lens, node).unwrap();
        // 畳み込みにより Projection(Literal(5)) になるはず
        match expanded {
            QueryNode::Projection(Operand::Literal(l)) => {
                assert_eq!(l.as_i64(), 5);
            }
            _ => panic!("Expected Projection(Literal), got {:?}", expanded),
        }
    }

    #[test]
    fn test_fold_literal_calculation_preserves_tag_ref() {
        use crate::query::ast::ArithmeticOp;
        use crate::types::{Label, TagType};

        let lens = Lens::base_standard();

        // (size: + 1) → Calculation は残る (TagRef を含むので畳み込めない)
        let calc = CalculationNode {
            left: Operand::TypeRef(TagType::from("size")),
            op: ArithmeticOp::Add,
            right: Operand::Literal(Label::from(1i64)),
        };
        let node = QueryNode::Projection(Operand::Calculation(Box::new(calc)));
        let expanded = expand_query_node(&lens, node).unwrap();
        assert!(matches!(
            expanded,
            QueryNode::Projection(Operand::Calculation(_))
        ));
    }

    // ========== Nest (&:) テスト ==========

    #[test]
    fn test_expand_nest_basic() {
        let lens = Lens::base_standard();
        // custom: &: custom2: → 子ノードが正しく展開される
        // (base_standard で特殊展開されないカスタムタグを使用)
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("project"),
            ))),
            right: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("category"),
            ))),
        });
        let expanded = expand_query_node(&lens, node).unwrap();
        match expanded {
            QueryNode::Nest(nest) => {
                assert!(
                    matches!(*nest.left, QueryNode::Projection(_)),
                    "left should be Projection, got: {:?}",
                    nest.left
                );
                assert!(
                    matches!(*nest.right, QueryNode::Projection(_)),
                    "right should be Projection, got: {:?}",
                    nest.right
                );
            }
            _ => panic!("Expected Nest, got {:?}", expanded),
        }
    }

    #[test]
    fn test_nest_is_set_operation() {
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("project"),
            ))),
            right: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
        });
        assert!(is_set_operation(&node));
    }

    /// agg-vs-literal の Comparison は Nest 外に分配される
    /// parentdir: &: (count(ext:jpg) > 1) → (parentdir: &: count(ext:jpg)) :> 1
    #[test]
    fn test_nest_comparison_agg_literal_distributed() {
        use crate::query::ast::{AggregationNode, BasicOp, ComparisonOp};
        let lens = Lens::base_standard();

        let count_agg = AggregationNode::Count(Box::new(QueryNode::TypedTag(
            TypedTag::new("extension", "jpg"),
        )));
        let cmp = ComparisonNode {
            first: Operand::Aggregation(Box::new(count_agg)),
            rest: vec![(
                ComparisonOp::Scalar(BasicOp::Gt),
                Operand::Literal(crate::types::Label::from(1i64)),
            )],
        };
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("parentdir"),
            ))),
            right: Box::new(QueryNode::Comparison(cmp)),
        });

        let expanded = expand_query_node(&lens, node).unwrap();

        // 結果は Comparison（分配された形）
        match &expanded {
            QueryNode::Comparison(c) => {
                // first: Query(Nest(parentdir, count(ext:jpg)))
                assert!(
                    matches!(&c.first, Operand::Query(q) if matches!(&**q, QueryNode::Nest(_))),
                    "first should be Query(Nest(...)), got: {:?}",
                    c.first
                );
                // rest[0]: Label(Gt), Literal(1)
                assert_eq!(c.rest.len(), 1);
                assert_eq!(c.rest[0].0, ComparisonOp::Label(BasicOp::Gt));
                assert!(
                    matches!(&c.rest[0].1, Operand::Literal(_)),
                    "rest operand should remain Literal, got: {:?}",
                    c.rest[0].1
                );
            }
            _ => panic!(
                "Expected Comparison after agg-vs-literal rewrite, got {:?}",
                expanded
            ),
        }
    }

    /// agg-vs-agg の Comparison は Nest 外に分配される
    #[test]
    fn test_nest_comparison_agg_agg_distributed() {
        use crate::query::ast::{
            AggregationNode, ArithmeticAggOp, BasicOp, ComparisonOp,
        };
        let lens = Lens::base_standard();

        // parentdir: &: (avg(size:) == sum(size:))
        // → Comparison(Nest(parentdir, avg(size:)) := Nest(parentdir, sum(size:)))
        let avg_agg = AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Avg,
            inner: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("size"),
            ))),
        };
        let sum_agg = AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("size"),
            ))),
        };
        let cmp = ComparisonNode {
            first: Operand::Aggregation(Box::new(avg_agg)),
            rest: vec![(
                ComparisonOp::Scalar(BasicOp::Eq),
                Operand::Aggregation(Box::new(sum_agg)),
            )],
        };
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("parentdir"),
            ))),
            right: Box::new(QueryNode::Comparison(cmp)),
        });

        let expanded = expand_query_node(&lens, node).unwrap();

        // 結果は Comparison（分配された形）
        match &expanded {
            QueryNode::Comparison(c) => {
                // first: Nest(parentdir, avg(size:))
                assert!(
                    matches!(&c.first, Operand::Query(q) if matches!(&**q, QueryNode::Nest(_))),
                    "first should be Query(Nest(...))"
                );
                // rest[0]: Label(Eq), Nest(parentdir, sum(size:))
                assert_eq!(c.rest.len(), 1);
                assert_eq!(c.rest[0].0, ComparisonOp::Label(BasicOp::Eq));
            }
            _ => panic!(
                "Expected Comparison after agg-vs-agg rewrite, got {:?}",
                expanded
            ),
        }
    }

    /// 展開後に And([filter, Projection]) になる左辺から Projection を抽出し、
    /// フィルタと Nest を And で包むことを確認
    #[test]
    fn test_nest_extract_projection_from_and_left() {
        let lens = Lens::base_standard();
        // extension: は And([is_dir:false, Projection(extension)]) に展開される
        // extension: &: count(name:) → And([is_dir:false, Nest(Proj(ext), count(name:))])
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
            right: Box::new(QueryNode::Aggregation(AggregationNode::Count(
                Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("name"),
                ))),
            ))),
        });
        let expanded = expand_query_node(&lens, node).unwrap();

        // extension: は And に展開されるため、結果も And になるはず
        match &expanded {
            QueryNode::And(nodes) => {
                // フィルタ (is_dir:false) + Nest の2要素
                assert!(
                    nodes.len() >= 2,
                    "Should have filter + nest, got {} nodes: {:?}",
                    nodes.len(),
                    nodes
                );
                // 最後の要素が Nest であること
                let nest =
                    nodes.iter().find(|n| matches!(n, QueryNode::Nest(_)));
                assert!(
                    nest.is_some(),
                    "And should contain a Nest node: {:?}",
                    nodes
                );
            }
            // extension: が展開されず Projection のままの場合は Nest 直接
            QueryNode::Nest(_) => {}
            _ => panic!("Expected And or Nest, got {:?}", expanded),
        }
    }

    /// 連鎖比較 + Nest: And に展開された各 Comparison にコンテキストが分配されることを確認
    /// `parentdir: &: (200 > sum(size:) > 50)` → And([Comparison, Comparison])
    /// 各 Comparison のオペランドが Nest で包まれること
    #[test]
    fn test_nest_chained_comparison_expansion() {
        use crate::query::ast::*;
        let lens = Lens::base_standard();

        // parentdir: &: (200 > sum(size:) > 50)
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("parentdir"),
            ))),
            right: Box::new(QueryNode::Comparison(ComparisonNode {
                first: Operand::Literal(Label::from(200)),
                rest: vec![
                    (
                        ComparisonOp::Scalar(BasicOp::Gt),
                        Operand::Aggregation(Box::new(
                            AggregationNode::Arithmetic {
                                op: ArithmeticAggOp::Sum,
                                inner: Box::new(QueryNode::Projection(
                                    Operand::TypeRef(TagType::from("size")),
                                )),
                            },
                        )),
                    ),
                    (
                        ComparisonOp::Scalar(BasicOp::Gt),
                        Operand::Literal(Label::from(50)),
                    ),
                ],
            })),
        });

        let expanded = expand_query_node(&lens, node).unwrap();

        // 結果は And([Comparison, Comparison]) であるべき
        match &expanded {
            QueryNode::And(nodes) => {
                // 各ノードが Comparison であること
                let comparisons: Vec<_> = nodes
                    .iter()
                    .filter(|n| matches!(n, QueryNode::Comparison(_)))
                    .collect();
                assert_eq!(
                    comparisons.len(),
                    2,
                    "Should have 2 comparisons, got: {:?}",
                    nodes
                );

                // 各 Comparison のオペランドに Query(Nest) が含まれること
                for cmp_node in &comparisons {
                    if let QueryNode::Comparison(cmp) = cmp_node {
                        let has_nest_operand =
                            matches!(&cmp.first, Operand::Query(q) if matches!(&**q, QueryNode::Nest(_)))
                            || cmp.rest.iter().any(|(_, op)| {
                                matches!(op, Operand::Query(q) if matches!(&**q, QueryNode::Nest(_)))
                            });
                        assert!(
                            has_nest_operand || matches!(&cmp.first, Operand::Literal(_)),
                            "Each comparison should have a Nest operand or Literal: {:?}",
                            cmp
                        );
                    }
                }
            }
            _ => panic!("Expected And with comparisons, got: {:?}", expanded),
        }
    }

    /// Nest の正規化構造を検証するテスト
    /// フィルタを含むタグ (extension:) が Nest の左辺に来た際、
    /// フィルタが外に持ち上げられ、集計内部にも注入されることを確認する。
    #[test]
    fn test_expand_nest_normalization() {
        let lens = Lens::base_standard();
        // クエリ: extension: &: count(name:)
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
            right: Box::new(QueryNode::Aggregation(AggregationNode::Count(
                Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("name"),
                ))),
            ))),
        });

        let expanded = expand_query_node(&lens, node).unwrap();

        // 期待: And([is_dir:false, Nest(Projection(ext), Count(And([is_dir:false, name])))])
        let nest = find_nest_in_and(&expanded);
        // Nest の左辺は Projection のみ（フィルタは分離済み）
        assert!(
            matches!(&*nest.left, QueryNode::Projection(_)),
            "Nest の左辺は Projection であること: {:?}",
            nest.left
        );
        // 集計内部にフィルタが注入されていること
        let agg_inner = get_agg_inner(&nest.right);
        assert_has_injected_filter(agg_inner);
    }

    /// Calculation における Nest 分配の正規化を検証するテスト
    /// フィルタが外に持ち上げられ、集計内部にも注入された And 構造を期待する。
    #[test]
    fn test_distribute_nest_over_calc_normalization() {
        use crate::query::ast::ArithmeticOp;
        let lens = Lens::base_standard();

        // クエリ: extension: &: (count(rs) * 10)
        let count_agg = AggregationNode::Count(Box::new(QueryNode::TypedTag(
            TypedTag::new("extension", "rs"),
        )));
        let calc = CalculationNode {
            left: Operand::Aggregation(Box::new(count_agg)),
            op: ArithmeticOp::Mul,
            right: Operand::Literal(Label::from(10)),
        };
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
            right: Box::new(QueryNode::Projection(Operand::Calculation(
                Box::new(calc),
            ))),
        });

        let expanded = expand_query_node(&lens, node).unwrap();

        // 期待: And([is_dir:false, Projection(Calculation(Query(Nest(Proj, Count(And([filter, tag])))), *, Literal))])
        match &expanded {
            QueryNode::And(nodes) => {
                assert!(
                    nodes.iter().any(|n| matches!(n, QueryNode::TypedTag(_))),
                    "トップレベルにフィルタが存在すること: {:?}",
                    nodes
                );
                let calc_node = nodes.iter().find(|n| {
                    matches!(n, QueryNode::Projection(Operand::Calculation(_)))
                });
                assert!(
                    calc_node.is_some(),
                    "Projection(Calculation) が存在すること: {:?}",
                    nodes
                );
            }
            _ => panic!("展開結果が And であること: {:?}", expanded),
        }
    }

    // ---- フィルタ注入テスト用ヘルパー ----

    /// And ノードから Nest を探して返す。見つからなければ panic。
    fn find_nest_in_and(expanded: &QueryNode) -> &NestNode {
        let QueryNode::And(nodes) = expanded else {
            panic!("展開結果が And であること: {:?}", expanded);
        };
        assert!(
            nodes.iter().any(|n| matches!(n, QueryNode::TypedTag(_))),
            "トップレベルにフィルタ (TypedTag) が存在すること: {:?}",
            nodes
        );
        nodes
            .iter()
            .find_map(|n| match n {
                QueryNode::Nest(nest) => Some(nest),
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!("And 内に Nest が存在すること: {:?}", nodes)
            })
    }

    /// AggregationNode の内部クエリを取得する。
    fn get_agg_inner(node: &QueryNode) -> &QueryNode {
        match node {
            QueryNode::Aggregation(AggregationNode::Count(inner)) => inner,
            QueryNode::Aggregation(AggregationNode::Arithmetic {
                inner,
                ..
            }) => inner,
            _ => panic!("Aggregation であること: {:?}", node),
        }
    }

    /// ノードが And であり、その中に TypedTag（フィルタ）を含むことを検証する。
    fn assert_has_injected_filter(node: &QueryNode) {
        let QueryNode::And(nodes) = node else {
            panic!("集計内部が And になっていること: {:?}", node);
        };
        assert!(
            nodes.iter().any(|n| matches!(n, QueryNode::TypedTag(_))),
            "集計内部にフィルタが注入されていること: {:?}",
            nodes
        );
    }

    // ---- フィルタ注入テストケース ----

    /// extension: &: count(name:) で is_dir:false が count 内部に注入されることを検証
    #[test]
    fn test_nest_filter_injection_count() {
        let lens = Lens::base_standard();
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
            right: Box::new(QueryNode::Aggregation(AggregationNode::Count(
                Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("name"),
                ))),
            ))),
        });
        let expanded = expand_query_node(&lens, node).unwrap();

        let nest = find_nest_in_and(&expanded);
        let agg_inner = get_agg_inner(&nest.right);
        assert_has_injected_filter(agg_inner);
    }

    /// extension: &: sum(size:) で is_dir:false が sum 内部に注入されることを検証
    #[test]
    fn test_nest_filter_injection_sum() {
        use crate::query::ast::ArithmeticAggOp;
        let lens = Lens::base_standard();
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
            right: Box::new(QueryNode::Aggregation(
                AggregationNode::Arithmetic {
                    op: ArithmeticAggOp::Sum,
                    inner: Box::new(QueryNode::Projection(Operand::TypeRef(
                        TagType::from("size"),
                    ))),
                },
            )),
        });
        let expanded = expand_query_node(&lens, node).unwrap();

        let nest = find_nest_in_and(&expanded);
        let agg_inner = get_agg_inner(&nest.right);
        assert_has_injected_filter(agg_inner);
    }

    /// Operand 内に Operand::Query(And(...)) が残っていないことを再帰的に検証する。
    fn assert_no_query_and_in_operand(operand: &Operand) {
        match operand {
            Operand::Query(node) => {
                assert!(
                    !matches!(&**node, QueryNode::And(_)),
                    "Operand::Query 内に And が残っていないこと: {:?}",
                    node
                );
            }
            Operand::Calculation(calc) => {
                assert_no_query_and_in_operand(&calc.left);
                assert_no_query_and_in_operand(&calc.right);
            }
            _ => {}
        }
    }

    /// Calculation 内の Nest からフィルタが抽出され、
    /// Operand::Query に And が残らないことを検証
    /// クエリ: (extension: &: count(extension:rs)) * 10 :> 0
    #[test]
    fn test_nest_in_calculation_filter_lifted() {
        let lens = Lens::base_standard();
        let node = QueryNode::Comparison(ComparisonNode {
            first: Operand::Calculation(Box::new(CalculationNode {
                left: Operand::Query(Box::new(QueryNode::Nest(NestNode {
                    left: Box::new(QueryNode::Projection(Operand::TypeRef(
                        TagType::from("extension"),
                    ))),
                    right: Box::new(QueryNode::Aggregation(
                        AggregationNode::Count(Box::new(QueryNode::TypedTag(
                            TypedTag::new(
                                "extension",
                                LabelValue::String("rs".to_string()),
                            ),
                        ))),
                    )),
                }))),
                op: ArithmeticOp::Mul,
                right: Operand::Literal(crate::types::Label::from(10)),
            })),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::Literal(crate::types::Label::from(0)),
            )],
        });
        let expanded = expand_query_node(&lens, node).unwrap();

        // 結果は And([is_dir:false, Comparison(Calc(Query(Nest(...)), *, 10), :>, 0)])
        let QueryNode::And(ref nodes) = expanded else {
            panic!("結果が And であること: {:?}", expanded);
        };
        // フィルタが存在すること
        assert!(
            nodes.iter().any(|n| matches!(n, QueryNode::TypedTag(_))),
            "トップレベルにフィルタが持ち上げられていること: {:?}",
            nodes
        );
        // Comparison が存在すること
        let cmp_node = nodes
            .iter()
            .find(|n| matches!(n, QueryNode::Comparison(_)))
            .expect("Comparison が存在すること");
        // Comparison 内の Operand::Query に And が残っていないこと
        if let QueryNode::Comparison(cmp) = cmp_node {
            assert_no_query_and_in_operand(&cmp.first);
            for (_, op) in &cmp.rest {
                assert_no_query_and_in_operand(op);
            }
        }
    }

    /// フィルタなしの左辺 (parentdir:) の場合、注入は行われないことを検証
    #[test]
    fn test_nest_no_filter_no_injection() {
        let lens = Lens::base_standard();
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("parentdir"),
            ))),
            right: Box::new(QueryNode::Aggregation(AggregationNode::Count(
                Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("name"),
                ))),
            ))),
        });
        let expanded = expand_query_node(&lens, node).unwrap();

        assert!(
            matches!(&expanded, QueryNode::Nest(_)),
            "フィルタなしの場合は Nest がそのまま返ること: {:?}",
            expanded
        );
    }

    #[test]
    fn test_validate_calculation_mixed_keys() {
        let lens = Lens::base_standard();
        let calc = CalculationNode {
            left: Operand::Query(Box::new(QueryNode::Nest(NestNode {
                left: Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("parentdir"),
                ))),
                right: Box::new(QueryNode::Aggregation(
                    AggregationNode::Count(Box::new(QueryNode::TypedTag(
                        TypedTag::new("parentdir", "foo"),
                    ))),
                )),
            }))),
            op: ArithmeticOp::Add,
            right: Operand::Query(Box::new(QueryNode::Nest(NestNode {
                left: Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("extension"),
                ))),
                right: Box::new(QueryNode::Aggregation(
                    AggregationNode::Count(Box::new(QueryNode::TypedTag(
                        TypedTag::new("extension", "rs"),
                    ))),
                )),
            }))),
        };
        // 異種キー演算が validate_calculation で許可されていることを確認
        assert!(validate_calculation(&calc, &lens).is_ok());
    }

    // ──────────────────────────────────────────────
    // Phase 7: Unnest by Aggregation
    // ──────────────────────────────────────────────

    /// sum(Nest(parentdir, Proj(size))) → Nest(parentdir, sum(Proj(size)))
    #[test]
    fn test_unnest_sum_projection() {
        let agg = AggregationNode::Arithmetic {
            op: crate::query::ast::ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::Nest(NestNode {
                left: Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("parentdir"),
                ))),
                right: Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("size"),
                ))),
            })),
        };

        assert!(is_unnestable_aggregation(&agg));

        let result = unnest_aggregation(agg);
        match &result {
            QueryNode::Nest(nest) => {
                // left = Proj(parentdir)
                assert!(matches!(
                    nest.left.as_ref(),
                    QueryNode::Projection(Operand::TypeRef(t)) if t.as_str() == "parentdir"
                ));
                // right = Aggregation(Sum, Proj(size))
                match nest.right.as_ref() {
                    QueryNode::Aggregation(AggregationNode::Arithmetic {
                        op,
                        inner,
                    }) => {
                        assert!(matches!(
                            op,
                            crate::query::ast::ArithmeticAggOp::Sum
                        ));
                        assert!(matches!(
                            inner.as_ref(),
                            QueryNode::Projection(Operand::TypeRef(t)) if t.as_str() == "size"
                        ));
                    }
                    other => panic!("Expected Aggregation, got: {:?}", other),
                }
            }
            other => panic!("Expected Nest, got: {:?}", other),
        }
    }

    /// count(Nest(parentdir, Proj(extension))) → Nest(parentdir, count(Proj(extension)))
    #[test]
    fn test_unnest_count_projection() {
        let agg = AggregationNode::Count(Box::new(QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("parentdir"),
            ))),
            right: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
        })));

        assert!(is_unnestable_aggregation(&agg));

        let result = unnest_aggregation(agg);
        match &result {
            QueryNode::Nest(nest) => {
                assert!(matches!(
                    nest.left.as_ref(),
                    QueryNode::Projection(Operand::TypeRef(t)) if t.as_str() == "parentdir"
                ));
                assert!(matches!(
                    nest.right.as_ref(),
                    QueryNode::Aggregation(AggregationNode::Count(_))
                ));
            }
            other => panic!("Expected Nest, got: {:?}", other),
        }
    }

    /// sum(Nest(Nest(a,b), Proj(size))) → Nest(Nest(a,b), sum(Proj(size)))
    #[test]
    fn test_unnest_deep_nest() {
        let inner_nest = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("parentdir"),
            ))),
            right: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
        });
        let agg = AggregationNode::Arithmetic {
            op: crate::query::ast::ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::Nest(NestNode {
                left: Box::new(inner_nest),
                right: Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("size"),
                ))),
            })),
        };

        assert!(is_unnestable_aggregation(&agg));

        let result = unnest_aggregation(agg);
        match &result {
            QueryNode::Nest(nest) => {
                // left = Nest(parentdir, extension)
                assert!(matches!(nest.left.as_ref(), QueryNode::Nest(_)));
                // right = Aggregation(Sum, Proj(size))
                assert!(matches!(
                    nest.right.as_ref(),
                    QueryNode::Aggregation(AggregationNode::Arithmetic { .. })
                ));
            }
            other => panic!("Expected Nest, got: {:?}", other),
        }
    }

    /// sum(Nest(parentdir, Agg(count))) → 変換なし (右辺がAggregation)
    #[test]
    fn test_unnest_no_change_right_is_agg() {
        let agg = AggregationNode::Arithmetic {
            op: crate::query::ast::ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::Nest(NestNode {
                left: Box::new(QueryNode::Projection(Operand::TypeRef(
                    TagType::from("parentdir"),
                ))),
                right: Box::new(QueryNode::Aggregation(
                    AggregationNode::Count(Box::new(QueryNode::TypedTag(
                        TypedTag::new("*", "*"),
                    ))),
                )),
            })),
        };

        assert!(!is_unnestable_aggregation(&agg));
    }

    /// count(Nest(parentdir, Comparison(...))) → 変換なし (右辺がComparison)
    #[test]
    fn test_unnest_no_change_right_is_comparison() {
        let agg = AggregationNode::Count(Box::new(QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("parentdir"),
            ))),
            right: Box::new(QueryNode::Comparison(ComparisonNode {
                first: Operand::Aggregation(Box::new(AggregationNode::Count(
                    Box::new(QueryNode::TypedTag(TypedTag::new(
                        "extension",
                        "jpg",
                    ))),
                ))),
                rest: vec![(
                    ComparisonOp::Scalar(BasicOp::Gt),
                    Operand::Literal(crate::types::Label::from(10i64)),
                )],
            })),
        })));

        assert!(!is_unnestable_aggregation(&agg));
    }

    /// sum(Proj(size)) → 変換なし (内部がNestではない)
    #[test]
    fn test_unnest_no_change_plain_agg() {
        let agg = AggregationNode::Arithmetic {
            op: crate::query::ast::ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("size"),
            ))),
        };

        assert!(!is_unnestable_aggregation(&agg));
    }

    // ── Phase 7: And/Or 透過テスト ────────────────────────────────────────

    /// is_unnestable_right(And([Proj(size), TypedTag(path)])) → true
    #[test]
    fn test_is_unnestable_right_and_with_projection() {
        let right = QueryNode::And(vec![
            QueryNode::Projection(Operand::TypeRef(TagType::from("size"))),
            QueryNode::TypedTag(TypedTag::new("path", "/tmp/test/*")),
        ]);
        assert!(is_unnestable_right(&right));
    }

    /// is_unnestable_right(And([TypedTag, TypedTag])) → false (Proj なし)
    #[test]
    fn test_is_unnestable_right_and_no_projection() {
        let right = QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new("path", "/tmp/*")),
            QueryNode::TypedTag(TypedTag::new("extension", "rs")),
        ]);
        assert!(!is_unnestable_right(&right));
    }

    /// is_unnestable_aggregation(Sum, And([Nest(parentdir, size), TypedTag(path)])) → true
    #[test]
    fn test_is_unnestable_agg_and_inner_with_nest() {
        let agg = AggregationNode::Arithmetic {
            op: crate::query::ast::ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::And(vec![
                QueryNode::Nest(NestNode {
                    left: Box::new(QueryNode::Projection(Operand::TypeRef(
                        TagType::from("parentdir"),
                    ))),
                    right: Box::new(QueryNode::Projection(Operand::TypeRef(
                        TagType::from("size"),
                    ))),
                }),
                QueryNode::TypedTag(TypedTag::new("path", "/tmp/*")),
            ])),
        };
        assert!(is_unnestable_aggregation(&agg));
    }

    /// is_unnestable_aggregation(Sum, And([TypedTag, TypedTag])) → false (Nest なし)
    #[test]
    fn test_is_unnestable_agg_and_inner_no_nest() {
        let agg = AggregationNode::Arithmetic {
            op: crate::query::ast::ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::And(vec![
                QueryNode::TypedTag(TypedTag::new("path", "/tmp/*")),
                QueryNode::TypedTag(TypedTag::new("extension", "rs")),
            ])),
        };
        assert!(!is_unnestable_aggregation(&agg));
    }

    /// unnest_aggregation(Sum, And([Nest(parentdir, size), TypedTag(path)]))
    ///   → And([Nest(parentdir, Sum(size)), TypedTag(path)])
    #[test]
    fn test_unnest_aggregation_and_inner() {
        let agg = AggregationNode::Arithmetic {
            op: crate::query::ast::ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::And(vec![
                QueryNode::Nest(NestNode {
                    left: Box::new(QueryNode::Projection(Operand::TypeRef(
                        TagType::from("parentdir"),
                    ))),
                    right: Box::new(QueryNode::Projection(Operand::TypeRef(
                        TagType::from("size"),
                    ))),
                }),
                QueryNode::TypedTag(TypedTag::new("path", "/tmp/*")),
            ])),
        };

        let result = unnest_aggregation(agg);

        let QueryNode::And(nodes) = result else {
            panic!("Expected And, got: {:?}", result);
        };
        assert_eq!(nodes.len(), 2);

        // 最初の要素: Nest(parentdir, Sum(size))
        let QueryNode::Nest(nest) = &nodes[0] else {
            panic!("Expected Nest, got: {:?}", nodes[0]);
        };
        assert!(matches!(
            nest.right.as_ref(),
            QueryNode::Aggregation(AggregationNode::Arithmetic {
                op: crate::query::ast::ArithmeticAggOp::Sum,
                ..
            })
        ));

        // 2番目の要素: TypedTag はそのまま
        assert!(matches!(&nodes[1], QueryNode::TypedTag(_)));
    }

    /// is_unnestable_aggregation(Sum, Nest(And([Proj(parentdir), TypedTag]), And([Proj(size), TypedTag])))
    ///   → true (right は And([Proj, TypedTag]))
    #[test]
    fn test_is_unnestable_agg_nest_with_and_right() {
        let agg = AggregationNode::Arithmetic {
            op: crate::query::ast::ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::Nest(NestNode {
                left: Box::new(QueryNode::And(vec![
                    QueryNode::Projection(Operand::TypeRef(TagType::from(
                        "parentdir",
                    ))),
                    QueryNode::TypedTag(TypedTag::new("path", "/tmp/*")),
                ])),
                right: Box::new(QueryNode::And(vec![
                    QueryNode::Projection(Operand::TypeRef(TagType::from(
                        "size",
                    ))),
                    QueryNode::TypedTag(TypedTag::new("path", "/tmp/*")),
                ])),
            })),
        };
        assert!(is_unnestable_aggregation(&agg));
    }
}
