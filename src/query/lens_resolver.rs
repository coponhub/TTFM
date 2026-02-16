//! # 物理解決器（Lens Resolver）
//!
//! Query AST を OneView の物理構造にマッピングします。
//!
//! ## 責務
//!
//! 1. **StorageMappingの決定**: Column/RowTag/Virtual の判定
//! 2. **SQL型の解決**: タグに対応する物理的なSQL型を決定
//! 3. **物理解決済みノードの生成**: ResolvedNode への変換
//!
//! ## 処理フロー
//!
//! ```text
//! QueryNode (論理展開済み)
//!   ↓ resolve_query_node()
//! ResolvedNode (物理解決済み)
//!   ↓ sql.rs
//! SQL文（OneViewに対するクエリ）
//! ```

use crate::db::{Col, SqlType};
use crate::query::ast::{ComparisonNode, ComparisonOp, Operand, QueryNode};
use crate::query::lens_schema::{Lens, StorageMapping};
use crate::types::{Label, LabelValue, SType, TagType};
use anyhow::{bail, Result};
use sea_query::{BinOper, Condition, Expr, SimpleExpr};

/// 物理マッピングが解決された後のクエリノード。
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedNode {
    And(Vec<ResolvedNode>),
    Or(Vec<ResolvedNode>),
    Difference(Box<ResolvedNode>, Box<ResolvedNode>),
    Complement(Box<ResolvedNode>),
    /// 投影クエリ用。
    Projection(ResolvedOperand),
    /// 物理カラムへの直接マッチ。
    ColumnMatch {
        tag: SType,
        label: Label,
    },
    /// 物理的な条件。
    Match {
        tag_type: TagType,
        storage: StorageMapping,
        sql_type: SqlType,
        op: ComparisonOp,
        label: Label,
    },
    Aggregation(ResolvedAggregationNode),
    /// 集約結果との比較。
    AggregationMatch {
        agg: ResolvedAggregationNode,
        op: ComparisonOp,
        label: Label,
    },
    /// 算術演算とリテラルの比較 (例: (1 + 2) :> size:)
    CalculationMatch {
        calc: ResolvedCalculationNode,
        op: ComparisonOp,
        label: Label,
    },
    /// タグと算術演算の比較 (例: size: > (1000 + 500))
    TagCalculationMatch {
        tag_type: TagType,
        storage: StorageMapping,
        sql_type: SqlType,
        op: ComparisonOp,
        calc: ResolvedCalculationNode,
    },
    AggregationCalculationMatch {
        agg: ResolvedAggregationNode,
        op: ComparisonOp,
        calc: ResolvedCalculationNode,
    },
    /// 集約関数同士の比較 (例: sum(size:) > sum(extension:h & size:))
    AggregationAggregationMatch {
        left: ResolvedAggregationNode,
        op: ComparisonOp,
        right: ResolvedAggregationNode,
    },
    /// 集約関数とタグの比較 (例: max(size:) == size:)
    AggregationTagMatch {
        agg: ResolvedAggregationNode,
        op: ComparisonOp,
        tag_type: TagType,
        storage: StorageMapping,
        sql_type: SqlType,
    },
    /// タグ同士の比較 (例: width: > height:)
    TagTagMatch {
        left_storage: StorageMapping,
        left_sql_type: SqlType,
        op: ComparisonOp,
        right_storage: StorageMapping,
        right_sql_type: SqlType,
    },
    /// リテラル同士のスカラー比較 (例: 10 > 2)
    ScalarMatch {
        left: Label,
        op: ComparisonOp,
        right: Label,
    },
}
#[derive(Debug, PartialEq, Clone)]
pub enum ResolvedAggregationNode {
    Count(Box<ResolvedNode>),
    Arithmetic {
        op: crate::query::ast::ArithmeticAggOp,
        inner: Box<ResolvedNode>,
    },
}

impl ResolvedAggregationNode {
    pub fn is_string_type(&self) -> bool {
        match self {
            ResolvedAggregationNode::Count(_) => false,
            ResolvedAggregationNode::Arithmetic { inner, .. } => {
                let (_, _, operand) = inner.extract_agg_parts();
                operand.map(|op| op.is_string_type()).unwrap_or(false)
            }
        }
    }
}

/// 算術演算のオペランド（解決済み）
#[derive(Debug, PartialEq, Clone)]
pub enum ResolvedOperand {
    /// リテラル値（数値または文字列）
    Literal(Label),
    /// タグ参照（Projection相当）
    TagRef {
        tag_type: TagType,
        storage: StorageMapping,
        sql_type: SqlType,
    },
    /// ネストした算術演算
    Calculation(Box<ResolvedCalculationNode>),
    /// 集約関数（スカラー値を返す）
    Aggregation(ResolvedAggregationNode),
}

impl ResolvedOperand {
    /// 集約関数が含まれているかチェックします。
    pub fn contains_aggregation(&self) -> bool {
        match self {
            ResolvedOperand::Aggregation(_) => true,
            ResolvedOperand::Calculation(calc) => calc.contains_aggregation(),
            _ => false,
        }
    }

    pub fn to_condition(&self) -> Condition {
        match self {
            ResolvedOperand::Literal(_) => Condition::any(),
            ResolvedOperand::TagRef { storage, .. } => cond_projection(storage),
            ResolvedOperand::Calculation(calc) => calc.to_condition(),
            ResolvedOperand::Aggregation(_) => Condition::any(),
        }
    }

    /// RowTag への参照が含まれているかチェックします（EAV 計算比較用）。
    pub fn contains_row_tag(&self) -> bool {
        match self {
            ResolvedOperand::TagRef { storage, .. } => {
                matches!(storage, StorageMapping::RowTag { .. })
            }
            ResolvedOperand::Calculation(calc) => calc.contains_row_tag(),
            _ => false,
        }
    }

    /// タグ参照を含まない純粋なスカラー式かどうかを判定します。
    /// リテラルと集約関数のみで構成される場合に true を返します。
    pub fn is_pure_scalar(&self) -> bool {
        match self {
            ResolvedOperand::Literal(_) | ResolvedOperand::Aggregation(_) => {
                true
            }
            ResolvedOperand::Calculation(calc) => {
                calc.left.is_pure_scalar() && calc.right.is_pure_scalar()
            }
            ResolvedOperand::TagRef { .. } => false,
        }
    }

    /// 文字列型かどうかを判定します。
    pub fn is_string_type(&self) -> bool {
        match self {
            ResolvedOperand::Literal(label) => matches!(
                label.value(),
                LabelValue::String(_) | LabelValue::Literal(_)
            ),
            ResolvedOperand::TagRef {
                tag_type, sql_type, ..
            } => {
                // 標準タグ (Base) の VARCHAR は確実に文字列として扱う。
                // カスタムタグ (Custom) は、暗黙の数値演算を許容するため、ここでの文字列判定からは除外する。
                !matches!(tag_type, TagType::Custom(_))
                    && matches!(sql_type, SqlType::VARCHAR)
            }
            ResolvedOperand::Calculation(calc) => {
                calc.left.is_string_type() && calc.right.is_string_type()
            }
            ResolvedOperand::Aggregation(agg) => agg.is_string_type(),
        }
    }
}

/// 算術演算の解決済みノード
#[derive(Debug, PartialEq, Clone)]
pub struct ResolvedCalculationNode {
    pub left: ResolvedOperand,
    pub op: crate::query::ast::ArithmeticOp,
    pub right: ResolvedOperand,
}

impl ResolvedCalculationNode {
    /// 集約関数が含まれているかチェックします。
    pub fn contains_aggregation(&self) -> bool {
        self.left.contains_aggregation() || self.right.contains_aggregation()
    }

    /// RowTag への参照が含まれているかチェックします（EAV 計算比較用）。
    pub fn contains_row_tag(&self) -> bool {
        self.left.contains_row_tag() || self.right.contains_row_tag()
    }

    pub fn to_condition(&self) -> Condition {
        self.left.to_condition().add(self.right.to_condition())
    }
}

impl ResolvedNode {
    /// このノードを sea_query::Condition に変換します。
    pub fn to_condition(&self) -> Condition {
        match self {
            ResolvedNode::And(nodes) => cond_and(nodes),
            ResolvedNode::Or(nodes) => cond_or(nodes),
            ResolvedNode::Difference(l, _r) => {
                // DIFFERENCE は WHERE 句単体では表現しきれない（通常 EXCEPT を使用）
                // ただしサブクエリ等で使用するための基本的な条件を返す
                l.to_condition()
            }
            ResolvedNode::Complement(_c) => {
                // COMPLEMENT も同様
                Condition::any()
            }
            ResolvedNode::Projection(op) => op.to_condition(),
            ResolvedNode::ColumnMatch { tag, label } => {
                cond_column_match(*tag, label)
            }
            ResolvedNode::Match {
                storage,
                sql_type,
                op,
                label,
                ..
            } => storage.to_condition(*op, label, *sql_type),
            ResolvedNode::Aggregation(_)
            | ResolvedNode::AggregationMatch { .. } => {
                // 集約は基本的に SELECT 句または HAVING 句で扱われる
                Condition::any()
            }
            ResolvedNode::CalculationMatch { .. }
            | ResolvedNode::TagCalculationMatch { .. }
            | ResolvedNode::AggregationCalculationMatch { .. }
            | ResolvedNode::AggregationAggregationMatch { .. }
            | ResolvedNode::AggregationTagMatch { .. }
            | ResolvedNode::TagTagMatch { .. }
            | ResolvedNode::ScalarMatch { .. } => {
                // 算術演算や集約比較は、単一の WHERE 句の Condition だけでは不十分な場合が多いため、
                // build_pick_sql 側で完全に SelectStatement を構築する。
                // 連結用には Condition::any() を返しておく。
                Condition::any()
            }
        }
    }

    /// このノードが投影（Projection）を目的としている場合、その対象の型を返します。
    pub fn get_projection(&self) -> Option<TagType> {
        match self {
            ResolvedNode::Projection(op) => extract_tag_type_from_operand(op),
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_projection())
            }
            ResolvedNode::Difference(l, _) | ResolvedNode::Complement(l) => {
                l.get_projection()
            }
            _ => None,
        }
    }

    /// Projection が Calculation を含むかどうか（ラベルグループ取得時に使用）
    pub fn get_projection_operand(&self) -> Option<&ResolvedOperand> {
        match self {
            ResolvedNode::Projection(op) => Some(op),
            _ => None,
        }
    }

    /// ネストされた Projection を再帰的に探索して返します。
    pub fn get_nested_projection(&self) -> Option<&ResolvedOperand> {
        match self {
            ResolvedNode::Projection(op) => Some(op),
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_nested_projection())
            }
            ResolvedNode::Difference(l, _) | ResolvedNode::Complement(l) => {
                l.get_nested_projection()
            }
            _ => None,
        }
    }

    /// 集約のために、このノードから投影（集計対象）とフィルタ条件を分離して返します。
    pub fn extract_agg_parts(
        &self,
    ) -> (
        Option<&StorageMapping>,
        Option<ResolvedNode>,
        Option<&ResolvedOperand>,
    ) {
        if let Some(op) = self.get_nested_projection() {
            let storage = extract_storage_from_operand(op);

            // 1. 自身が Projection の場合はフィルタなし
            if matches!(self, ResolvedNode::Projection(_)) {
                return (storage, None, Some(op));
            }

            // 2. And ノードの場合は、プロジェクションを含むノードを除外または部分的に除外
            if let ResolvedNode::And(nodes) = self {
                let mut filter_nodes = Vec::with_capacity(nodes.len());
                let mut extracted = false;

                for n in nodes {
                    if !extracted && n.get_nested_projection().is_some() {
                        // このノード内にプロジェクションがある
                        let (_, inner_filter, _) = n.extract_agg_parts();
                        if let Some(f) = inner_filter {
                            filter_nodes.push(f);
                        }
                        extracted = true;
                    } else {
                        filter_nodes.push(n.clone());
                    }
                }

                let final_filter = match filter_nodes.len() {
                    0 => None,
                    1 => Some(filter_nodes[0].clone()),
                    _ => Some(ResolvedNode::And(filter_nodes)),
                };
                return (storage, final_filter, Some(op));
            }

            // 3. その他（通常はここには来ないが、Or など）は自身をフィルタとして返す
            (storage, Some(self.clone()), Some(op))
        } else {
            (None, Some(self.clone()), None)
        }
    }

    /// 全ての子要素が集約比較であれば、このノード全体をスカラー（ブーリアン）結果として扱う
    pub fn is_boolean_result(&self) -> bool {
        match self {
            ResolvedNode::AggregationMatch { .. }
            | ResolvedNode::AggregationCalculationMatch { .. }
            | ResolvedNode::AggregationAggregationMatch { .. }
            | ResolvedNode::AggregationTagMatch { .. }
            | ResolvedNode::ScalarMatch { .. } => true,
            ResolvedNode::TagCalculationMatch { calc, .. }
            | ResolvedNode::CalculationMatch { calc, .. } => {
                calc.contains_aggregation()
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                !nodes.is_empty() && nodes.iter().all(|n| n.is_boolean_result())
            }
            ResolvedNode::Difference(l, r) => {
                l.is_boolean_result() && r.is_boolean_result()
            }
            ResolvedNode::Complement(c) => c.is_boolean_result(),
            _ => false,
        }
    }
}

/// ResolvedOperand から再帰的に最初の TagRef の TagType を抽出するヘルパー
fn extract_tag_type_from_operand(op: &ResolvedOperand) -> Option<TagType> {
    match op {
        ResolvedOperand::TagRef { tag_type, .. } => Some(tag_type.clone()),
        ResolvedOperand::Calculation(calc) => {
            extract_tag_type_from_operand(&calc.left)
                .or_else(|| extract_tag_type_from_operand(&calc.right))
        }
        _ => None,
    }
}

/// 算術演算の中から再帰的に最初の TagRef のストレージを抽出するヘルパー
fn extract_storage_from_operand(
    op: &ResolvedOperand,
) -> Option<&StorageMapping> {
    match op {
        ResolvedOperand::TagRef { storage, .. } => Some(storage),
        ResolvedOperand::Calculation(calc) => {
            extract_storage_from_operand(&calc.left)
                .or_else(|| extract_storage_from_operand(&calc.right))
        }
        _ => None,
    }
}

fn cond_and(nodes: &[ResolvedNode]) -> Condition {
    let mut cond = Condition::all();
    for n in nodes {
        cond = cond.add(n.to_condition());
    }
    cond
}

fn cond_or(nodes: &[ResolvedNode]) -> Condition {
    let mut cond = Condition::any();
    for n in nodes {
        cond = cond.add(n.to_condition());
    }
    cond
}

fn cond_projection(storage: &StorageMapping) -> Condition {
    // Projection は「存在する」ことが条件
    match storage {
        StorageMapping::Column(col) => {
            Condition::all().add(Expr::col(*col).is_not_null())
        }
        StorageMapping::RowTag { tag_type, .. } => {
            Condition::all().add(check_tag_match(tag_type))
        }
        StorageMapping::Virtual => Condition::any(),
    }
}

fn cond_column_match(tag: SType, label: &Label) -> Condition {
    // 直接の物理カラム指定
    let col = tag;
    let val = match label.value() {
        crate::types::LabelValue::Integer(i) => Expr::val(i),
        crate::types::LabelValue::Boolean(b) => Expr::val(b),
        crate::types::LabelValue::Double(bits) => {
            Expr::val(f64::from_bits(bits))
        }
        crate::types::LabelValue::Null => Expr::val(None::<i32>),
        crate::types::LabelValue::String(s)
        | crate::types::LabelValue::Literal(s) => Expr::val(s),
    };
    // ColumnMatch の場合は型固有のルールは適用せず、単純にマッピング
    Condition::all().add(Expr::col(col).eq(val))
}

fn check_tag_match(tag_type: &str) -> SimpleExpr {
    let mut tag_op = BinOper::Equal;
    if tag_type.contains('*')
        || tag_type.contains('?')
        || tag_type.contains('[')
    {
        tag_op = BinOper::Custom("GLOB");
    }
    Expr::col(Col::Type).binary(tag_op, tag_type)
}

// ========== 物理解決関数群 ==========

pub(crate) fn resolve_operand(
    lens: &Lens,
    op: &Operand,
) -> Result<ResolvedOperand> {
    match op {
        Operand::Literal(label) => Ok(ResolvedOperand::Literal(label.clone())),
        Operand::TypeRef(tt) => resolve_type_ref_operand(lens, tt),
        Operand::Calculation(calc) => {
            let resolved_calc = resolve_calculation_node(lens, calc)?;
            Ok(ResolvedOperand::Calculation(Box::new(resolved_calc)))
        }
        Operand::Aggregation(agg) => {
            let resolved_agg = resolve_aggregation_node(lens, agg)?;
            Ok(ResolvedOperand::Aggregation(resolved_agg))
        }
        Operand::Query(_) => {
            // 論理リゾルバで展開されているはずなので、ここに来ることはないはずだが、
            // 型定義上はあり得るのでエラーを返す
            bail!(
                "Operand::Query should have been flattened by logical resolver"
            );
        }
    }
}

fn resolve_type_ref_operand(
    lens: &Lens,
    tt: &TagType,
) -> Result<ResolvedOperand> {
    let (storage, sql_type) = match lens.look_up(tt) {
        Some(desc) => (desc.storage.clone(), desc.sql_type()),
        None => (
            StorageMapping::RowTag {
                column: Col::LabelStr,
                tag_type: tt.as_str().to_string(),
            },
            SqlType::VARCHAR,
        ),
    };
    Ok(ResolvedOperand::TagRef {
        tag_type: tt.clone(),
        storage,
        sql_type,
    })
}

fn resolve_calculation_node(
    lens: &Lens,
    calc: &crate::query::ast::CalculationNode,
) -> Result<ResolvedCalculationNode> {
    let left = resolve_operand(lens, &calc.left)?;
    let right = resolve_operand(lens, &calc.right)?;
    validate_calculation_types(&left, &right, calc.op)?;
    Ok(ResolvedCalculationNode {
        left,
        op: calc.op,
        right,
    })
}

/// 算術演算の型バリデーション（共通関数）
fn validate_calculation_types(
    left: &ResolvedOperand,
    right: &ResolvedOperand,
    op: crate::query::ast::ArithmeticOp,
) -> Result<()> {
    use crate::query::ast::ArithmeticOp;

    let left_is_str = left.is_string_type();
    let right_is_str = right.is_string_type();

    // String と non-String の混合演算はエラー
    if left_is_str != right_is_str {
        return Err(crate::query::error::unsupported_mixed_type_arithmetic(
            if left_is_str { "String" } else { "non-String" },
            if right_is_str { "String" } else { "non-String" },
        ));
    }

    // String 同士の場合、+ と * のみ許可
    if left_is_str
        && right_is_str
        && !matches!(op, ArithmeticOp::Add | ArithmeticOp::Mul)
    {
        return Err(crate::query::error::unsupported_string_arithmetic(
            &format!("{:?}", op),
        ));
    }

    Ok(())
}

fn resolve_aggregation_node(
    lens: &Lens,
    agg: &crate::query::ast::AggregationNode,
) -> Result<ResolvedAggregationNode> {
    use crate::query::ast::AggregationNode;
    match agg {
        AggregationNode::Count(node) => {
            // Count Aggregation wraps a QueryNode
            let resolved_inner = resolve_query_node(lens, *node.clone())?;
            Ok(ResolvedAggregationNode::Count(Box::new(resolved_inner)))
        }
        AggregationNode::Arithmetic { op, inner } => {
            // Arithmetic Aggregation wraps a QueryNode (typically Projection)
            let resolved_inner = resolve_query_node(lens, *inner.clone())?;
            Ok(ResolvedAggregationNode::Arithmetic {
                op: *op,
                inner: Box::new(resolved_inner),
            })
        }
    }
}

pub(crate) fn resolve_query_node(
    lens: &Lens,
    node: QueryNode,
) -> Result<ResolvedNode> {
    match node {
        QueryNode::TypedTag(tt) => {
            let tag_type = tt.label.tag_type();
            let (storage, sql_type) = match lens.look_up(&tag_type) {
                Some(desc) => (desc.storage.clone(), desc.sql_type()),
                None => (
                    StorageMapping::RowTag {
                        column: Col::LabelStr,
                        tag_type: tag_type.as_str().to_string(),
                    },
                    SqlType::VARCHAR,
                ),
            };
            Ok(ResolvedNode::Match {
                tag_type,
                storage,
                sql_type,
                op: ComparisonOp::Scalar(crate::query::ast::BasicOp::Eq),
                label: tt.label,
            })
        }
        QueryNode::ColumnMatch { tag, label } => {
            let tag_type = TagType::Base(tag);
            let desc = lens
                .look_up(&tag_type)
                .ok_or_else(|| anyhow::anyhow!("Unknown SType: {:?}", tag))?;
            Ok(ResolvedNode::Match {
                tag_type,
                storage: desc.storage.clone(),
                sql_type: desc.sql_type(),
                op: ComparisonOp::Scalar(crate::query::ast::BasicOp::Eq),
                label,
            })
        }
        QueryNode::Comparison(cmp) => resolve_comparison(lens, cmp),
        QueryNode::And(nodes) => {
            let mut resolved = Vec::new();
            for n in nodes {
                resolved.push(resolve_query_node(lens, n)?);
            }
            Ok(ResolvedNode::And(resolved))
        }
        QueryNode::Or(nodes) => {
            let mut resolved = Vec::new();
            for n in nodes {
                resolved.push(resolve_query_node(lens, n)?);
            }
            Ok(ResolvedNode::Or(resolved))
        }
        QueryNode::Difference(l, r) => Ok(ResolvedNode::Difference(
            Box::new(resolve_query_node(lens, *l)?),
            Box::new(resolve_query_node(lens, *r)?),
        )),
        QueryNode::Complement(c) => Ok(ResolvedNode::Complement(Box::new(
            resolve_query_node(lens, *c)?,
        ))),
        QueryNode::Projection(op) => {
            let resolved_op = resolve_operand(lens, &op)?;
            Ok(ResolvedNode::Projection(resolved_op))
        }
        QueryNode::Aggregation(agg) => {
            let res = resolve_aggregation(lens, agg)?;
            Ok(ResolvedNode::Aggregation(res))
        }
        QueryNode::Nest(_nest) => {
            // Phase 3-5 で実装予定
            Err(anyhow::anyhow!("Nest node resolution not yet implemented"))
        }
    }
}

fn resolve_aggregation(
    lens: &Lens,
    agg: crate::query::ast::AggregationNode,
) -> Result<ResolvedAggregationNode> {
    use crate::query::ast::AggregationNode;
    match agg {
        AggregationNode::Count(node) => Ok(ResolvedAggregationNode::Count(
            Box::new(resolve_query_node(lens, *node)?),
        )),
        AggregationNode::Arithmetic { op, inner } => {
            Ok(ResolvedAggregationNode::Arithmetic {
                op,
                inner: Box::new(resolve_query_node(lens, *inner)?),
            })
        }
    }
}

pub(crate) fn resolve_calculation(
    lens: &Lens,
    calc: crate::query::ast::CalculationNode,
) -> Result<ResolvedCalculationNode> {
    let left = resolve_operand_for_calc(lens, calc.left)?;
    let right = resolve_operand_for_calc(lens, calc.right)?;
    validate_calculation_types(&left, &right, calc.op)?;
    Ok(ResolvedCalculationNode {
        left,
        op: calc.op,
        right,
    })
}

fn resolve_operand_for_calc(
    lens: &Lens,
    operand: Operand,
) -> Result<ResolvedOperand> {
    match operand {
        Operand::Literal(lab) => Ok(ResolvedOperand::Literal(lab)),
        Operand::TypeRef(tt) => {
            let (storage, sql_type) = get_storage_and_type(lens, &tt);
            Ok(ResolvedOperand::TagRef {
                tag_type: tt,
                storage,
                sql_type,
            })
        }
        Operand::Calculation(calc) => {
            let resolved = resolve_calculation(lens, *calc)?;
            Ok(ResolvedOperand::Calculation(Box::new(resolved)))
        }
        Operand::Aggregation(agg) => {
            let resolved = resolve_aggregation(lens, *agg)?;
            Ok(ResolvedOperand::Aggregation(resolved))
        }
        Operand::Query(_) => {
            bail!(
                "Operand::Query should have been flattened by logical resolver"
            );
        }
    }
}

fn resolve_comparison(
    lens: &Lens,
    mut cmp: ComparisonNode,
) -> Result<ResolvedNode> {
    if cmp.rest.len() != 1 {
        bail!("Logical resolver should have flattened the comparison chain");
    }

    let (op, right) = cmp.rest.pop().unwrap();
    resolve_single_match(lens, cmp.first, op, right)
}

/// メイン解決ロジック（15パターンの比較処理）
fn resolve_single_match(
    lens: &Lens,
    left: Operand,
    op: ComparisonOp,
    right: Operand,
) -> Result<ResolvedNode> {
    match (left, right) {
        (Operand::TypeRef(tt), Operand::Literal(lab)) => {
            let (storage, sql_type) = get_storage_and_type(lens, &tt);
            Ok(ResolvedNode::Match {
                tag_type: tt,
                storage,
                sql_type,
                op,
                label: lab,
            })
        }
        (Operand::Literal(lab), Operand::TypeRef(tt)) => {
            let (storage, sql_type) = get_storage_and_type(lens, &tt);
            Ok(ResolvedNode::Match {
                tag_type: tt,
                storage,
                sql_type,
                op: flip_op(op),
                label: lab,
            })
        }
        (Operand::Aggregation(agg), Operand::Literal(lab)) => {
            let res_agg = resolve_aggregation(lens, *agg)?;
            Ok(ResolvedNode::AggregationMatch {
                agg: res_agg,
                op,
                label: lab,
            })
        }
        (Operand::Literal(lab), Operand::Aggregation(agg)) => {
            let res_agg = resolve_aggregation(lens, *agg)?;
            Ok(ResolvedNode::AggregationMatch {
                agg: res_agg,
                op: flip_op(op),
                label: lab,
            })
        }
        // (1 + 2) :> size:
        (Operand::Calculation(calc), Operand::Literal(lab)) => {
            let res_calc = resolve_calculation(lens, *calc)?;
            Ok(ResolvedNode::CalculationMatch {
                calc: res_calc,
                op,
                label: lab,
            })
        }
        // size: > (1000 + 500)
        (Operand::TypeRef(tt), Operand::Calculation(calc)) => {
            let (storage, sql_type) = get_storage_and_type(lens, &tt);
            let res_calc = resolve_calculation(lens, *calc)?;
            Ok(ResolvedNode::TagCalculationMatch {
                tag_type: tt,
                storage,
                sql_type,
                op,
                calc: res_calc,
            })
        }
        // (1 + 2) :> size:
        // 意味: size: が (1+2) より大きい → size: > (1+2)
        // 解決後の TagCalculationMatch は (tag op calc) の順なので、演算子を反転させる必要がある
        (Operand::Calculation(calc), Operand::TypeRef(tt)) => {
            let (storage, sql_type) = get_storage_and_type(lens, &tt);
            let res_calc = resolve_calculation(lens, *calc)?;
            Ok(ResolvedNode::TagCalculationMatch {
                tag_type: tt,
                storage,
                sql_type,
                op: flip_op(op),
                calc: res_calc,
            })
        }
        // sum(size:) > (100 * 2)
        (Operand::Aggregation(agg), Operand::Calculation(calc)) => {
            let res_agg = resolve_aggregation(lens, *agg)?;
            let res_calc = resolve_calculation(lens, *calc)?;
            Ok(ResolvedNode::AggregationCalculationMatch {
                agg: res_agg,
                op,
                calc: res_calc,
            })
        }
        // (100 * 2) < sum(size:)
        (Operand::Calculation(calc), Operand::Aggregation(agg)) => {
            let res_agg = resolve_aggregation(lens, *agg)?;
            let res_calc = resolve_calculation(lens, *calc)?;
            Ok(ResolvedNode::AggregationCalculationMatch {
                agg: res_agg,
                op: flip_op(op),
                calc: res_calc,
            })
        }
        // sum(size:) > sum(extension:h & size:)
        (Operand::Aggregation(l_agg), Operand::Aggregation(r_agg)) => {
            let res_l = resolve_aggregation(lens, *l_agg)?;
            let res_r = resolve_aggregation(lens, *r_agg)?;
            Ok(ResolvedNode::AggregationAggregationMatch {
                left: res_l,
                op,
                right: res_r,
            })
        }
        // max(size:) == size:
        (Operand::Aggregation(agg), Operand::TypeRef(tt)) => {
            let (storage, sql_type) = get_storage_and_type(lens, &tt);
            let res_agg = resolve_aggregation(lens, *agg)?;
            Ok(ResolvedNode::AggregationTagMatch {
                agg: res_agg,
                op,
                tag_type: tt,
                storage,
                sql_type,
            })
        }
        // size: == max(size:)
        (Operand::TypeRef(tt), Operand::Aggregation(agg)) => {
            let (storage, sql_type) = get_storage_and_type(lens, &tt);
            let res_agg = resolve_aggregation(lens, *agg)?;
            Ok(ResolvedNode::AggregationTagMatch {
                agg: res_agg,
                op: flip_op(op),
                tag_type: tt,
                storage,
                sql_type,
            })
        }
        // Literal < Calculation のパターン (例: 100MB < (size: / 2))
        (Operand::Literal(lab), Operand::Calculation(calc)) => {
            let res_calc = resolve_calculation(lens, *calc)?;
            // 値のパースは行わない（sql.rsに任せる）
            Ok(ResolvedNode::CalculationMatch {
                calc: res_calc,
                op: flip_op(op),
                label: lab,
            })
        }
        // width: > height:
        (Operand::TypeRef(tt_l), Operand::TypeRef(tt_r)) => {
            let (sl, tl) = get_storage_and_type(lens, &tt_l);
            let (sr, tr) = get_storage_and_type(lens, &tt_r);
            Ok(ResolvedNode::TagTagMatch {
                left_storage: sl,
                left_sql_type: tl,
                op,
                right_storage: sr,
                right_sql_type: tr,
            })
        }
        // 10 > 2 (リテラル同士のスカラー比較)
        (Operand::Literal(left), Operand::Literal(right)) => {
            Ok(ResolvedNode::ScalarMatch { left, op, right })
        }
        _ => Err(anyhow::anyhow!("Unsupported comparison pattern")),
    }
}

fn get_storage_and_type(
    lens: &Lens,
    tt: &TagType,
) -> (StorageMapping, SqlType) {
    match lens.look_up(tt) {
        Some(desc) => (desc.storage.clone(), desc.sql_type()),
        None => (
            StorageMapping::RowTag {
                column: Col::LabelStr,
                tag_type: tt.as_str().to_string(),
            },
            SqlType::VARCHAR,
        ),
    }
}

pub fn flip_op(op: ComparisonOp) -> ComparisonOp {
    match op {
        ComparisonOp::Scalar(b) => ComparisonOp::Scalar(flip_basic_op(b)),
        ComparisonOp::Label(b) => ComparisonOp::Label(flip_basic_op(b)),
    }
}

pub fn flip_basic_op(
    op: crate::query::ast::BasicOp,
) -> crate::query::ast::BasicOp {
    use crate::query::ast::BasicOp;
    match op {
        BasicOp::Gt => BasicOp::Lt,
        BasicOp::Ge => BasicOp::Le,
        BasicOp::Lt => BasicOp::Gt,
        BasicOp::Le => BasicOp::Ge,
        other => other,
    }
}

/// ComparisonOp を sea_query の BinOper に変換します。
///
/// **重要**: sql.rs で使用中
pub fn to_bin_op(op: ComparisonOp) -> BinOper {
    use crate::query::ast::BasicOp;
    let basic = match op {
        ComparisonOp::Scalar(b) => b,
        ComparisonOp::Label(b) => b,
    };
    match basic {
        BasicOp::Eq => BinOper::Equal,
        BasicOp::Ne => BinOper::NotEqual,
        BasicOp::Gt => BinOper::GreaterThan,
        BasicOp::Ge => BinOper::GreaterThanOrEqual,
        BasicOp::Lt => BinOper::SmallerThan,
        BasicOp::Le => BinOper::SmallerThanOrEqual,
    }
}

// ========== Resolver 構造体 ==========

/// クエリの論理展開と物理解決を統合する構造体
pub struct Resolver {
    lens: Lens,
    pub expanded_query: QueryNode,
    pub resolved_query: ResolvedNode,
}

impl Resolver {
    /// 標準的な Lens を使用してクエリ文字列から Resolver を生成
    ///
    /// 処理フロー:
    /// 1. パース: Query文字列 → QueryNode (AST)
    /// 2. 論理展開: logical_resolver::expand_query_node()
    /// 3. 物理解決: resolve_query_node() → ResolvedNode
    pub fn new(query: &str) -> Result<Self> {
        let lens = Lens::base_standard();
        let node = if query.trim().is_empty() {
            QueryNode::And(vec![])
        } else {
            crate::query::parse(query)?
        };

        // 論理展開 + 型チェック（logical_resolver.rsに委譲）
        let expanded =
            crate::query::logical_resolver::expand_query_node(&lens, node)?;

        // 物理解決（このファイル内のresolve_query_node）
        let resolved = resolve_query_node(&lens, expanded.clone())?;

        Ok(Self {
            lens,
            expanded_query: expanded,
            resolved_query: resolved,
        })
    }

    /// Lens への参照を返す（Fetcherで使用）
    pub fn lens(&self) -> &Lens {
        &self.lens
    }

    /// 投影対象の型を返す
    pub fn get_projection(&self) -> Option<TagType> {
        self.resolved_query.get_projection()
    }

    /// トップレベル集約を返す
    pub fn get_aggregation(&self) -> Option<ResolvedAggregationNode> {
        match &self.resolved_query {
            ResolvedNode::Aggregation(agg) => Some(agg.clone()),
            _ => None,
        }
    }

    /// スカラー式を返す
    pub fn get_scalar_expression(&self) -> Option<ResolvedOperand> {
        match &self.resolved_query {
            ResolvedNode::Aggregation(agg) => {
                Some(ResolvedOperand::Aggregation(agg.clone()))
            }
            ResolvedNode::Projection(op) => {
                if op.contains_aggregation() || op.is_pure_scalar() {
                    Some(op.clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::lens_schema::{build_int_condition, build_str_condition};
    use crate::types::{Label, SType};

    #[test]
    fn test_cond_and_basic() {
        let node = ResolvedNode::ColumnMatch {
            tag: SType::LabelStr,
            label: Label::from("val"),
        };

        let nodes = vec![node];
        let cond = cond_and(&nodes);
        let debug_str = format!("{:?}", cond);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_build_int_condition() {
        let cond = build_int_condition(Col::LabelInt, BinOper::Equal, 10, true);
        let debug_str = format!("{:?}", cond);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_build_str_condition() {
        let cond = build_str_condition(
            Col::LabelStr,
            BinOper::Equal,
            "test_val",
            SqlType::VARCHAR,
            true,
        );
        let debug_str = format!("{:?}", cond);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_resolved_node_is_boolean_result() {
        use crate::query::ast::ComparisonOp;
        use crate::types::SType;

        // 1. AggregationMatch is boolean
        let agg =
            ResolvedAggregationNode::Count(Box::new(ResolvedNode::And(vec![])));
        let node_bool = ResolvedNode::AggregationMatch {
            agg,
            op: ComparisonOp::Scalar(crate::query::ast::BasicOp::Gt),
            label: Label::from(0),
        };
        assert!(node_bool.is_boolean_result());

        // 2. Normal ColumnMatch is NOT boolean
        let node_normal = ResolvedNode::ColumnMatch {
            tag: SType::Extension,
            label: Label::from("rs"),
        };
        assert!(!node_normal.is_boolean_result());

        // 3. AND(Boolean, Boolean) is boolean
        let node_and_bool =
            ResolvedNode::And(vec![node_bool.clone(), node_bool.clone()]);
        assert!(node_and_bool.is_boolean_result());

        // 4. AND(Boolean, Normal) is NOT boolean
        let node_mixed =
            ResolvedNode::And(vec![node_bool.clone(), node_normal.clone()]);
        assert!(!node_mixed.is_boolean_result());

        // 5. OR(Boolean, Boolean) is boolean
        let node_or_bool =
            ResolvedNode::Or(vec![node_bool.clone(), node_bool.clone()]);
        assert!(node_or_bool.is_boolean_result());

        // 6. Complement(Boolean) is boolean
        let node_not_bool =
            ResolvedNode::Complement(Box::new(node_bool.clone()));
        assert!(node_not_bool.is_boolean_result());
    }

    #[test]
    fn test_extract_agg_parts_logic() {
        use crate::db::Col;
        use crate::query::lens_schema::StorageMapping;
        use crate::types::{Label, SType, TagType};

        // Prepare nodes
        // Case 1: And(Projection, Filter)
        let projection = ResolvedNode::Projection(ResolvedOperand::TagRef {
            tag_type: TagType::Base(SType::Size),
            storage: StorageMapping::Column(Col::Size),
            sql_type: SqlType::BIGINT,
        });
        let filter = ResolvedNode::ColumnMatch {
            tag: SType::Extension,
            label: Label::from("txt"),
        };

        let and_node =
            ResolvedNode::And(vec![projection.clone(), filter.clone()]);

        let (storage, extracted_filter, _) = and_node.extract_agg_parts();

        // Verification
        assert!(storage.is_some(), "Storage should be extracted");
        assert_eq!(
            storage.unwrap(),
            &StorageMapping::Column(Col::Size),
            "Storage content mismatch"
        );

        assert!(extracted_filter.is_some(), "Filter should be extracted");
        assert_eq!(
            extracted_filter.unwrap(),
            filter,
            "Filter content mismatch"
        );
    }

    #[test]
    fn test_resolve_calculation_literal() {
        use crate::query::ast::{ArithmeticOp, CalculationNode, Operand};
        use crate::types::Label;

        let lens = Lens::base_standard();
        let calc = CalculationNode {
            left: Operand::Literal(Label::from(1)),
            op: ArithmeticOp::Add,
            right: Operand::Literal(Label::from(2)),
        };

        let result = resolve_calculation(&lens, calc);
        assert!(result.is_ok());

        let resolved = result.unwrap();
        assert_eq!(resolved.op, ArithmeticOp::Add);
        assert_eq!(resolved.left, ResolvedOperand::Literal(Label::from(1)));
        assert_eq!(resolved.right, ResolvedOperand::Literal(Label::from(2)));
    }

    #[test]
    fn test_resolve_calculation_with_tag_ref() {
        use crate::query::ast::{ArithmeticOp, CalculationNode, Operand};
        use crate::types::{Label, SType, TagType};

        let lens = Lens::base_standard();
        let calc = CalculationNode {
            left: Operand::TypeRef(TagType::Base(SType::Size)),
            op: ArithmeticOp::Add,
            right: Operand::Literal(Label::from(100)),
        };

        let result = resolve_calculation(&lens, calc);
        assert!(result.is_ok());

        let resolved = result.unwrap();
        assert_eq!(resolved.op, ArithmeticOp::Add);

        // 左側がTagRefであることを確認
        match &resolved.left {
            ResolvedOperand::TagRef {
                tag_type, storage, ..
            } => {
                assert_eq!(*tag_type, TagType::Base(SType::Size));
                // size:はRowTagとして保存されている
                assert!(matches!(storage, StorageMapping::RowTag { .. }));
            }
            _ => panic!("Expected TagRef"),
        }

        // 右側がLiteralであることを確認
        assert_eq!(resolved.right, ResolvedOperand::Literal(Label::from(100)));
    }

    #[test]
    fn test_resolve_calculation_nested() {
        use crate::query::ast::{ArithmeticOp, CalculationNode, Operand};
        use crate::types::Label;

        let lens = Lens::base_standard();

        // ((1 + 2) * 3) のような入れ子構造
        let inner_calc = CalculationNode {
            left: Operand::Literal(Label::from(1)),
            op: ArithmeticOp::Add,
            right: Operand::Literal(Label::from(2)),
        };

        let outer_calc = CalculationNode {
            left: Operand::Calculation(Box::new(inner_calc)),
            op: ArithmeticOp::Mul,
            right: Operand::Literal(Label::from(3)),
        };

        let result = resolve_calculation(&lens, outer_calc);
        assert!(result.is_ok());

        let resolved = result.unwrap();
        assert_eq!(resolved.op, ArithmeticOp::Mul);

        // 左側がCalculationであることを確認
        match &resolved.left {
            ResolvedOperand::Calculation(nested) => {
                assert_eq!(nested.op, ArithmeticOp::Add);
                assert_eq!(
                    nested.left,
                    ResolvedOperand::Literal(Label::from(1))
                );
                assert_eq!(
                    nested.right,
                    ResolvedOperand::Literal(Label::from(2))
                );
            }
            _ => panic!("Expected Calculation"),
        }

        // 右側がLiteralであることを確認
        assert_eq!(resolved.right, ResolvedOperand::Literal(Label::from(3)));
    }

    #[test]
    fn test_resolve_operand_with_aggregation() {
        use crate::query::ast::{
            AggregationNode, ArithmeticAggOp, ArithmeticOp, CalculationNode,
            Operand, QueryNode,
        };
        use crate::types::{Label, SType, TagType};

        let lens = Lens::base_standard();

        // sum(size:) のAggregationNode
        let agg_node = AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::Base(SType::Size),
            ))),
        };

        // (sum(size:) + 100) のCalculationNode
        let calc = CalculationNode {
            left: Operand::Aggregation(Box::new(agg_node)),
            op: ArithmeticOp::Add,
            right: Operand::Literal(Label::from(100)),
        };

        let result = resolve_calculation(&lens, calc);
        assert!(result.is_ok());

        let resolved = result.unwrap();
        assert_eq!(resolved.op, ArithmeticOp::Add);

        // 左側がAggregationであることを確認
        match &resolved.left {
            ResolvedOperand::Aggregation(agg) => {
                // ResolvedAggregationNode::Arithmeticであることを確認
                match agg {
                    ResolvedAggregationNode::Arithmetic { op, .. } => {
                        assert_eq!(*op, ArithmeticAggOp::Sum);
                    }
                    _ => panic!("Expected Arithmetic aggregation"),
                }
            }
            _ => panic!("Expected Aggregation"),
        }

        // 右側がLiteralであることを確認
        assert_eq!(resolved.right, ResolvedOperand::Literal(Label::from(100)));
    }

    #[test]
    fn test_resolve_operand_literal() {
        use crate::query::ast::Operand;
        use crate::types::Label;

        let lens = Lens::base_standard();
        let op = Operand::Literal(Label::from(42));
        let resolved = resolve_operand(&lens, &op).unwrap();

        match resolved {
            ResolvedOperand::Literal(l) => assert_eq!(l.as_i64(), 42),
            _ => panic!("Expected Literal"),
        }
    }

    #[test]
    fn test_resolve_operand_tag_ref() {
        use crate::query::ast::Operand;
        use crate::types::{SType, TagType};

        let lens = Lens::base_standard();
        let op = Operand::TypeRef(TagType::Base(SType::Size));
        let resolved = resolve_operand(&lens, &op).unwrap();

        match resolved {
            ResolvedOperand::TagRef { tag_type, .. } => {
                assert_eq!(tag_type, TagType::Base(SType::Size));
            }
            _ => panic!("Expected TagRef"),
        }
    }

    #[test]
    fn test_resolve_comparison_simple() {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        let lens = Lens::base_standard();

        // size: > 100
        let cmp = ComparisonNode {
            first: Operand::TypeRef("size".into()),
            rest: vec![(
                ComparisonOp::Scalar(BasicOp::Gt),
                Operand::Literal(crate::types::Label::from(100i64)),
            )],
        };

        let resolved = resolve_comparison(&lens, cmp).unwrap();
        if let crate::query::lens_resolver::ResolvedNode::Match {
            op,
            label,
            ..
        } = resolved
        {
            assert_eq!(op, ComparisonOp::Scalar(BasicOp::Gt));
            assert_eq!(label.as_i64(), 100);
        } else {
            panic!("Expected Match, got {:?}", resolved);
        }
    }

    #[test]
    fn test_resolve_tag_tag_comparison() {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::TagType;
        let lens = Lens::base_standard();

        // width: > height:
        let cmp = ComparisonNode {
            first: Operand::TypeRef(TagType::from("width")),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::TypeRef(TagType::from("height")),
            )],
        };

        let resolved = resolve_comparison(&lens, cmp).unwrap();
        if let ResolvedNode::TagTagMatch { op, .. } = resolved {
            assert_eq!(op, ComparisonOp::Label(BasicOp::Gt));
        } else {
            panic!("Expected TagTagMatch, got {:?}", resolved);
        }
    }

    #[test]
    fn test_extract_agg_parts_calculation() {
        use crate::query::ast::{ArithmeticOp, CalculationNode, Operand};
        use crate::types::{Label, SType, TagType};

        let lens = Lens::base_standard();
        // size: - 100
        let calc = CalculationNode {
            left: Operand::TypeRef(TagType::Base(SType::Size)),
            op: ArithmeticOp::Sub,
            right: Operand::Literal(Label::from(100)),
        };
        let resolved_calc = resolve_calculation(&lens, calc).unwrap();
        let operand = ResolvedOperand::Calculation(Box::new(resolved_calc));
        let node = ResolvedNode::Projection(operand.clone());

        // 修正後の期待値: (Some(storage), None, Some(operand))
        let (storage, filter, res_op) = node.extract_agg_parts();
        assert!(storage.is_some());
        assert!(matches!(storage.unwrap(), StorageMapping::RowTag { .. }));
        assert!(filter.is_none());
        assert!(res_op.is_some());
        assert_eq!(res_op.unwrap(), &operand);
    }

    #[test]
    fn test_validate_calculation_types_string_sub_rejected() {
        use crate::query::ast::ArithmeticOp;
        use crate::types::Label;

        let left = ResolvedOperand::Literal(Label::Other(
            crate::types::TagType::Custom(String::new()),
            crate::types::LabelValue::Literal("a".into()),
        ));
        let right = ResolvedOperand::Literal(Label::Other(
            crate::types::TagType::Custom(String::new()),
            crate::types::LabelValue::Literal("b".into()),
        ));

        let result =
            validate_calculation_types(&left, &right, ArithmeticOp::Sub);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported arithmetic"));
    }

    #[test]
    fn test_validate_calculation_types_mixed_type_rejected() {
        use crate::query::ast::ArithmeticOp;
        use crate::types::Label;

        let left = ResolvedOperand::Literal(Label::Other(
            crate::types::TagType::Custom(String::new()),
            crate::types::LabelValue::Literal("a".into()),
        ));
        let right = ResolvedOperand::Literal(Label::from(1i64));

        let result =
            validate_calculation_types(&left, &right, ArithmeticOp::Add);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("String and non-String"));
    }

    #[test]
    fn test_validate_calculation_types_string_add_allowed() {
        use crate::query::ast::ArithmeticOp;
        use crate::types::Label;

        let left = ResolvedOperand::Literal(Label::Other(
            crate::types::TagType::Custom(String::new()),
            crate::types::LabelValue::Literal("a".into()),
        ));
        let right = ResolvedOperand::Literal(Label::Other(
            crate::types::TagType::Custom(String::new()),
            crate::types::LabelValue::Literal("b".into()),
        ));

        let result =
            validate_calculation_types(&left, &right, ArithmeticOp::Add);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_calculation_types_int_sub_allowed() {
        use crate::query::ast::ArithmeticOp;
        use crate::types::Label;

        let left = ResolvedOperand::Literal(Label::from(10i64));
        let right = ResolvedOperand::Literal(Label::from(5i64));

        let result =
            validate_calculation_types(&left, &right, ArithmeticOp::Sub);
        assert!(result.is_ok());
    }
}
