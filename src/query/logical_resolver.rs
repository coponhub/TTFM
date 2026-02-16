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
        QueryNode::Nest(nest) => {
            let left = expand_query_node(schema, *nest.left)?;
            let right = expand_query_node(schema, *nest.right)?;

            // 左辺から Projection を抽出。
            // 展開後に And([filter..., Projection]) になるケースに対応:
            //   extension: → And([is_dir:false, Projection(extension)])
            //   → And([is_dir:false, Nest(Projection(extension), right)])
            let (proj_left, filter_nodes) = extract_projection_from_left(left)?;

            // 右辺が Comparison の場合、比較を Nest の外に分配する:
            // Nest(L, Cmp(agg op lit)) → Cmp(Nest(L, agg) :op lit)
            // Nest(L, Cmp(agg1 op agg2)) → Cmp(Nest(L, agg1) :op Nest(L, agg2))
            let nest_node = match right {
                QueryNode::Comparison(cmp) => {
                    expand_nest_comparison(proj_left, cmp)?
                }
                _ => QueryNode::Nest(NestNode {
                    left: Box::new(proj_left),
                    right: Box::new(right),
                }),
            };

            // フィルタがある場合は And で包む
            if filter_nodes.is_empty() {
                Ok(nest_node)
            } else {
                let mut nodes = filter_nodes;
                nodes.push(nest_node);
                Ok(QueryNode::And(nodes))
            }
        }
        other => Ok(other),
    }
}

/// Nest 左辺から Projection を抽出し、残りをフィルタノードとして返します。
/// `extension:` 等は展開後に `And([is_dir:false, Projection(extension)])` になるため、
/// And の中から Projection を取り出し、残りをフィルタとして分離します。
fn extract_projection_from_left(
    left: QueryNode,
) -> Result<(QueryNode, Vec<QueryNode>)> {
    match left {
        QueryNode::Projection(_) | QueryNode::Nest(_) => {
            Ok((left, Vec::new()))
        }
        QueryNode::And(nodes) => {
            let mut proj = None;
            let mut filters = Vec::new();
            for node in nodes {
                if proj.is_none() && returns_projection(&node) {
                    proj = Some(node);
                } else {
                    filters.push(node);
                }
            }
            match proj {
                Some(p) => Ok((p, filters)),
                None => bail!(
                    "Nest operator '&:' requires a Projection on the left side, \
                     but And contains no Projection"
                ),
            }
        }
        _ => bail!(
            "Nest operator '&:' requires a Projection or Nest on the left side, got: {:?}",
            left
        ),
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
        | QueryNode::Complement(_)
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
/// クエリノードから再帰的にプロジェクションのオペランドを探します
fn find_projection_operand(node: &QueryNode) -> Option<&Operand> {
    match node {
        QueryNode::Projection(op) => Some(op),
        QueryNode::And(nodes) | QueryNode::Or(nodes) => {
            nodes.iter().find_map(find_projection_operand)
        }
        QueryNode::Difference(l, _) => find_projection_operand(l),
        QueryNode::Complement(c) => find_projection_operand(c),
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
        QueryNode::Complement(_) => {
            // 補集合は通常アイテム集合を返す
            false
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

/// Nest の右辺 Comparison を Nest の外に分配する正規化。
///
/// Aggregation/Projection 系オペランドは Nest で包み、
/// Literal はそのまま残す。スカラー演算子はラベル演算子に変換。
///
/// 例1 (agg vs agg): `parentdir: &: (avg(size:) == sum(size:))`
///   → `(parentdir: &: avg(size:)) := (parentdir: &: sum(size:))`
/// 例2 (agg vs lit): `parentdir: &: (count(ext:jpg) > 10)`
///   → `(parentdir: &: count(ext:jpg)) :> 10`
fn expand_nest_comparison(
    left: QueryNode,
    cmp: ComparisonNode,
) -> Result<QueryNode> {
    let first = wrap_operand_for_nest(left.clone(), cmp.first);

    let mut rest = Vec::new();
    for (op, operand) in cmp.rest {
        let wrapped = wrap_operand_for_nest(left.clone(), operand);
        // スカラー比較をラベル比較に変換（Nest結果はProjectionなので）
        let label_op = match op {
            ComparisonOp::Scalar(basic) => ComparisonOp::Label(basic),
            label @ ComparisonOp::Label(_) => label,
        };
        rest.push((label_op, wrapped));
    }

    Ok(QueryNode::Comparison(ComparisonNode { first, rest }))
}

/// 比較オペランドを Nest 分配用に変換する。
/// Aggregation/TypeRef/Calculation/Query → Nest で包んで Query オペランドにする。
/// Literal → そのまま残す（Nest で包まない）。
fn wrap_operand_for_nest(left: QueryNode, operand: Operand) -> Operand {
    match operand {
        Operand::Literal(_) => operand,
        other => {
            let right_query = operand_to_query_node(other);
            let nest = QueryNode::Nest(NestNode {
                left: Box::new(left),
                right: Box::new(right_query),
            });
            Operand::Query(Box::new(nest))
        }
    }
}

/// Operand を QueryNode に変換するヘルパー。
/// Nest の右辺として使用可能な形に変換する。
fn operand_to_query_node(operand: Operand) -> QueryNode {
    match operand {
        Operand::Aggregation(agg) => QueryNode::Aggregation(*agg),
        Operand::TypeRef(tt) => QueryNode::Projection(Operand::TypeRef(tt)),
        Operand::Calculation(calc) => {
            QueryNode::Projection(Operand::Calculation(calc))
        }
        Operand::Query(node) => *node,
        Operand::Literal(_) => QueryNode::Projection(operand),
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
    fn test_nest_left_must_be_projection_or_nest() {
        let lens = Lens::base_standard();
        // TypedTag &: Projection → エラー（左辺が Projection/Nest でない）
        let node = QueryNode::Nest(NestNode {
            left: Box::new(QueryNode::TypedTag(TypedTag::new("size", "100"))),
            right: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from("extension"),
            ))),
        });
        let result = expand_query_node(&lens, node);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Nest operator"),
            "Error should mention Nest operator"
        );
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
}
