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
use crate::query::ast::{
    AggregationNode, ArithmeticAggOp, ArithmeticOp, ComparisonNode,
    ComparisonOp, NestNode, Operand, QueryNode,
};
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
    Projection {
        operand: ResolvedOperand,
        /// nvalue（評価値）。付与されている場合、算術演算や比較において
        /// 実際のラベル値の代わりに使用される（集合演算を除く）。
        /// 集約・スカラー・算術式のいずれも取りうる。
        nvalue: Option<ResolvedOperand>,
        /// このプロジェクション（およびそのnvalue集計）に適用されるべきフィルタ
        context: Option<Box<ResolvedNode>>,
    },
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
    /// nvalue 付き Projection への比較。フィルタされたラベル集合を返す。
    /// 例: `(parentdir: &: count(ext:jpg)) :> 10`
    ProjectionMatch {
        operand: ResolvedOperand,
        nvalue: ResolvedOperand,
        op: ComparisonOp,
        label: Label,
        /// このプロジェクション（およびそのnvalue集計）に適用されるべきフィルタ
        context: Option<Box<ResolvedNode>>,
    },
    /// nvalue 付き Projection 同士の比較または算術演算。
    /// - op: Comparison → フィルタ結果（どのグループが条件を満たすか）
    /// - op: Arithmetic → nvalue 付き Projection 結果（各グループの計算値）
    ProjectionProjectionMatch {
        left_operand: ResolvedOperand,
        left_nvalue: ResolvedOperand,
        left_context: Option<Box<ResolvedNode>>,
        op: ProjectionOp,
        right_operand: ResolvedOperand,
        right_nvalue: ResolvedOperand,
        right_context: Option<Box<ResolvedNode>>,
    },
    /// マージされた nvalue 付き Projection マッチ。
    /// 共通の GROUP BY キー（operand）に対して複数の条件を AND/OR で適用する。
    MergedProjectionMatch {
        operand: ResolvedOperand,
        matches: Vec<ProjectionMatchCondition>,
        is_or: bool,
    },
}

/// nvalue 付き Projection 同士を結ぶ演算子。
/// 比較（フィルタ）と算術（nvalue 計算）の両方を統一的に扱う。
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionOp {
    Comparison(ComparisonOp),
    Arithmetic(ArithmeticOp),
}

/// 同一 operand（GROUP BY キー）に対する検索条件。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionMatchCondition {
    pub nvalue: ResolvedOperand,
    pub op: ProjectionOp,
    pub right: ResolvedOperand,
    pub context: Option<Box<ResolvedNode>>,
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
                // nvalue が付与されている場合は、nvalue 自体の型をチェックする
                if let Some(nvalue) = inner.get_nvalue() {
                    let res = nvalue.is_string_type();
                    if std::env::var("TTFM_DEBUG").is_ok() {
                        println!("DEBUG: ResolvedAggregationNode::is_string_type: derived from nvalue={:?} -> {}", nvalue, res);
                    }
                    return res;
                }
                let (_, _, operand) = inner.extract_agg_parts();
                let res =
                    operand.map(|op| op.is_string_type()).unwrap_or(false);
                if std::env::var("TTFM_DEBUG").is_ok() {
                    println!("DEBUG: ResolvedAggregationNode::is_string_type: derived from operand={:?} -> {}", operand, res);
                }
                res
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

    pub fn get_storage(&self) -> Option<&StorageMapping> {
        extract_storage_from_operand(self)
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
    /// 自身、または入れ子のいずれかに Projection / ProjectionMatch を含むかチェックします。
    pub fn is_projection_recursive(&self) -> bool {
        match self {
            ResolvedNode::Projection { .. }
            | ResolvedNode::ProjectionMatch { .. }
            | ResolvedNode::ProjectionProjectionMatch { .. }
            | ResolvedNode::MergedProjectionMatch { .. } => true,
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().any(|n| n.is_projection_recursive())
            }
            ResolvedNode::Difference(l, r) => {
                l.is_projection_recursive() || r.is_projection_recursive()
            }
            ResolvedNode::Complement(c) => c.is_projection_recursive(),
            _ => false,
        }
    }

    /// 自身、または入れ子の全ての Projection / ProjectionMatch にコンテキストを注入します。
    pub fn inject_context(&mut self, context: ResolvedNode) {
        match self {
            ResolvedNode::Projection {
                context: ref mut ctx,
                ..
            }
            | ResolvedNode::ProjectionMatch {
                context: ref mut ctx,
                ..
            } => {
                if let Some(old) = ctx.take() {
                    // 既存のコンテキストがある場合は And で結合
                    *ctx =
                        Some(Box::new(ResolvedNode::And(vec![*old, context])));
                } else {
                    *ctx = Some(Box::new(context));
                }
            }
            ResolvedNode::ProjectionProjectionMatch {
                left_context: ref mut l_ctx,
                right_context: ref mut r_ctx,
                ..
            } => {
                // 両方に注入
                if let Some(old) = l_ctx.take() {
                    *l_ctx = Some(Box::new(ResolvedNode::And(vec![
                        *old,
                        context.clone(),
                    ])));
                } else {
                    *l_ctx = Some(Box::new(context.clone()));
                }

                if let Some(old) = r_ctx.take() {
                    *r_ctx =
                        Some(Box::new(ResolvedNode::And(vec![*old, context])));
                } else {
                    *r_ctx = Some(Box::new(context));
                }
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                for n in nodes {
                    n.inject_context(context.clone());
                }
            }
            ResolvedNode::Difference(l, r) => {
                l.inject_context(context.clone());
                r.inject_context(context);
            }
            ResolvedNode::Complement(c) => {
                c.inject_context(context);
            }
            _ => {}
        }
    }

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
            ResolvedNode::Projection {
                operand: op,
                context,
                ..
            }
            | ResolvedNode::ProjectionMatch {
                operand: op,
                context,
                ..
            } => {
                let mut cond = op.to_condition();
                if let Some(ctx) = context {
                    cond = cond.add(ctx.to_condition());
                }
                cond
            }
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
            | ResolvedNode::ProjectionProjectionMatch { .. }
            | ResolvedNode::MergedProjectionMatch { .. }
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
            ResolvedNode::Projection { operand: op, .. }
            | ResolvedNode::ProjectionMatch { operand: op, .. }
            | ResolvedNode::ProjectionProjectionMatch {
                left_operand: op,
                ..
            }
            | ResolvedNode::MergedProjectionMatch { operand: op, .. } => {
                extract_tag_type_from_operand(op)
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_projection())
            }
            ResolvedNode::Difference(l, _) | ResolvedNode::Complement(l) => {
                l.get_projection()
            }
            _ => None,
        }
    }

    pub fn get_nvalue(&self) -> Option<&ResolvedOperand> {
        match self {
            ResolvedNode::Projection { nvalue, .. } => nvalue.as_ref(),
            ResolvedNode::ProjectionMatch { nvalue, .. } => Some(nvalue),
            ResolvedNode::ProjectionProjectionMatch { left_nvalue, .. } => {
                Some(left_nvalue)
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_nvalue())
            }
            ResolvedNode::Difference(l, _) | ResolvedNode::Complement(l) => {
                l.get_nvalue()
            }
            ResolvedNode::MergedProjectionMatch { matches, .. } => {
                matches.first().map(|m| &m.nvalue)
            }
            _ => None,
        }
    }

    /// nvalueの再構築（主に算術演算のProjectionProjectionMatch用）
    /// get_nvalue() は参照を返すが、ProjectionProjectionMatch(Arithmetic) の場合は
    /// 左右のオペランドを結合したCalculationを所有権付きで返す必要があるため新設。
    pub fn get_nvalue_combined(&self) -> Option<ResolvedOperand> {
        match self {
            ResolvedNode::Projection { nvalue, .. } => nvalue.clone(),
            ResolvedNode::ProjectionMatch { nvalue, .. } => {
                Some(nvalue.clone())
            }
            ResolvedNode::ProjectionProjectionMatch {
                left_nvalue,
                op: ProjectionOp::Arithmetic(arith_op),
                right_nvalue,
                ..
            } => Some(ResolvedOperand::Calculation(Box::new(
                ResolvedCalculationNode {
                    left: left_nvalue.clone(),
                    op: *arith_op,
                    right: right_nvalue.clone(),
                },
            ))),
            ResolvedNode::ProjectionProjectionMatch { left_nvalue, .. } => {
                Some(left_nvalue.clone())
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_nvalue_combined())
            }
            ResolvedNode::Difference(l, _) | ResolvedNode::Complement(l) => {
                l.get_nvalue_combined()
            }
            ResolvedNode::MergedProjectionMatch { matches, .. } => {
                matches.first().map(|m| {
                    if let ProjectionOp::Arithmetic(arith_op) = &m.op {
                        ResolvedOperand::Calculation(Box::new(
                            ResolvedCalculationNode {
                                left: m.nvalue.clone(),
                                op: *arith_op,
                                right: m.right.clone(),
                            },
                        ))
                    } else {
                        m.nvalue.clone()
                    }
                })
            }
            _ => None,
        }
    }

    /// Projection が Calculation を含むかどうか（ラベルグループ取得時に使用）
    pub fn get_projection_operand(&self) -> Option<&ResolvedOperand> {
        match self {
            ResolvedNode::Projection { operand: op, .. }
            | ResolvedNode::ProjectionMatch { operand: op, .. }
            | ResolvedNode::ProjectionProjectionMatch {
                left_operand: op,
                ..
            }
            | ResolvedNode::MergedProjectionMatch { operand: op, .. } => {
                Some(op)
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_projection_operand())
            }
            ResolvedNode::Difference(l, _) | ResolvedNode::Complement(l) => {
                l.get_projection_operand()
            }
            _ => None,
        }
    }

    /// ノードからコンテキスト（フィルタ）を取得します。
    pub fn get_context(&self) -> Option<&ResolvedNode> {
        match self {
            ResolvedNode::Projection { context, .. }
            | ResolvedNode::ProjectionMatch { context, .. } => {
                context.as_deref()
            }
            ResolvedNode::ProjectionProjectionMatch {
                left_context, ..
            } => left_context.as_deref(),
            _ => None,
        }
    }

    /// ネストされた Projection を再帰的に探索して返します。
    pub fn get_nested_projection(&self) -> Option<&ResolvedOperand> {
        match self {
            ResolvedNode::Projection { operand: op, .. }
            | ResolvedNode::ProjectionMatch { operand: op, .. } => Some(op),
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
        if let Some(op) =
            self.get_nvalue().or_else(|| self.get_nested_projection())
        {
            let storage = extract_storage_from_operand(op);

            // 1. 自身が Projection/ProjectionMatch の場合はフィルタなし
            if matches!(
                self,
                ResolvedNode::Projection { .. }
                    | ResolvedNode::ProjectionMatch { .. }
            ) {
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

    /// ProjectionMatch の比較条件を再帰的に探索して返します。
    pub fn get_nvalue_condition(&self) -> Option<(&ComparisonOp, &Label)> {
        match self {
            ResolvedNode::ProjectionMatch { op, label, .. } => {
                Some((op, label))
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_nvalue_condition())
            }
            ResolvedNode::Difference(l, _) | ResolvedNode::Complement(l) => {
                l.get_nvalue_condition()
            }
            ResolvedNode::MergedProjectionMatch { matches, .. } => {
                matches.iter().find_map(|m| {
                    if let crate::query::lens_resolver::ProjectionOp::Comparison(op) = &m.op {
                        if let crate::query::ResolvedOperand::Literal(label) = &m.right {
                            return Some((op, label));
                        }
                    }
                    None
                })
            }
            _ => None,
        }
    }

    /// 全ての子要素が集約比較であれば、このノード全体をスカラー（ブーリアン）結果として扱う
    pub fn is_boolean_result(&self) -> bool {
        match self {
            ResolvedNode::AggregationMatch { .. }
            | ResolvedNode::AggregationCalculationMatch { .. }
            | ResolvedNode::AggregationAggregationMatch { .. }
            | ResolvedNode::AggregationTagMatch { .. }
            | ResolvedNode::ScalarMatch { .. }
            | ResolvedNode::ProjectionMatch { .. }
            | ResolvedNode::MergedProjectionMatch { .. } => true,
            // Arithmetic は Projection 結果（nvalue 付き）なので false
            ResolvedNode::ProjectionProjectionMatch { op, .. } => {
                matches!(op, ProjectionOp::Comparison(_))
            }
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
    let res = Ok(ResolvedOperand::TagRef {
        tag_type: tt.clone(),
        storage,
        sql_type,
    });
    if std::env::var("TTFM_DEBUG").is_ok() {
        println!(
            "DEBUG: resolve_type_ref_operand: tag={:?}, sql_type={:?}",
            tt, sql_type
        );
    }
    res
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

            // コンテキストの抽出とプロジェクションへの注入
            let mut filters = Vec::new();
            let mut projections = Vec::new();

            for (i, node) in resolved.iter().enumerate() {
                if node.is_projection_recursive() {
                    projections.push(i);
                } else {
                    filters.push(node.clone());
                }
            }

            if !filters.is_empty() && !projections.is_empty() {
                let context = if filters.len() == 1 {
                    filters.remove(0)
                } else {
                    ResolvedNode::And(filters)
                };

                for idx in projections {
                    resolved[idx].inject_context(context.clone());
                }
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
        // Projection(Calculation{Query(Nest(A,agg1)), op, Query(Nest(A,agg2))})
        // → logical_resolver が算術分配した形式。ProjectionProjectionMatch に解決する。
        QueryNode::Projection(Operand::Calculation(calc))
            if calc_has_nest_operands(&calc) =>
        {
            resolve_projection_arithmetic(lens, *calc)
        }
        // Projection(Calculation{Query(Nest(A,agg1)), op, Query(Nest(A,agg2))})
        // → logical_resolver が算術分配した形式。ProjectionProjectionMatch に解決する。
        QueryNode::Projection(Operand::Calculation(calc))
            if calc_has_nest_operands(&calc) =>
        {
            resolve_projection_arithmetic(lens, *calc)
        }
        QueryNode::Projection(op) => {
            let resolved_op = resolve_operand(lens, &op)?;
            Ok(ResolvedNode::Projection {
                operand: resolved_op,
                nvalue: None,
                context: None,
            })
        }
        QueryNode::Aggregation(agg) => {
            let res = resolve_aggregation(lens, agg)?;
            Ok(ResolvedNode::Aggregation(res))
        }
        QueryNode::Nest(nest) => resolve_nest(lens, nest),
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

/// Nest ノードの物理解決。
///
/// 深さ1: `Nest(Proj, Agg/Scalar)` → `Projection { operand, nvalue }`
/// 深さ2+: Phase 5 で実装
fn resolve_nest(
    lens: &Lens,
    nest: crate::query::ast::NestNode,
) -> Result<ResolvedNode> {
    let left = resolve_query_node(lens, *nest.left)?;
    let right_node = *nest.right;

    let (operand, context) = extract_projection_operand_with_context(left)?;

    match right_node {
        // 深さ1: 右辺が Aggregation → nvalue 付き Projection
        QueryNode::Aggregation(agg) => {
            let resolved_agg = resolve_aggregation(lens, agg)?;
            Ok(ResolvedNode::Projection {
                operand,
                nvalue: Some(ResolvedOperand::Aggregation(resolved_agg)),
                context,
            })
        }
        // Comparison は logical_resolver で分配済みのためここには来ない
        QueryNode::Comparison(_) => {
            bail!("Comparison in Nest right side should have been distributed by logical_resolver")
        }
        // 右辺が Projection(Literal) → nvalue にスカラー値を付与
        QueryNode::Projection(Operand::Literal(label)) => {
            Ok(ResolvedNode::Projection {
                operand,
                nvalue: Some(ResolvedOperand::Literal(label)),
                context,
            })
        }
        // 右辺が Projection(Calculation) は logical_resolver で算術分配済みのため
        // ここには来ない（Query 形式に変換されている）
        QueryNode::Projection(Operand::Calculation(calc)) => {
            // 集約と定数のみの算術式は logical_resolver で分配済みだが、
            // TypeRef を含む純粋な計算式（e.g., size: * 2）はここに到達する
            let resolved_calc = resolve_calculation(lens, *calc)?;
            Ok(ResolvedNode::Projection {
                operand,
                nvalue: Some(ResolvedOperand::Calculation(Box::new(
                    resolved_calc,
                ))),
                context,
            })
        }
        // 深さ2+: 右辺が投影（TypeRefなど）の場合は未実装
        QueryNode::Projection(_) => {
            bail!("Nest with Projection right side (depth 2+) not yet implemented")
        }
        QueryNode::Nest(_) => {
            bail!("Nested Nest (depth 2+) not yet implemented")
        }
        _ => {
            bail!("Unsupported Nest right side: {:?}", right_node)
        }
    }
}

/// Calculation 内に Query(Nest(...)) オペランドが含まれるか確認するヘルパー。
fn calc_has_nest_operands(calc: &crate::query::ast::CalculationNode) -> bool {
    operand_has_nest(&calc.left) || operand_has_nest(&calc.right)
}

fn operand_has_nest(op: &Operand) -> bool {
    match op {
        Operand::Query(node) => matches!(node.as_ref(), QueryNode::Nest(_)),
        Operand::Calculation(calc) => {
            operand_has_nest(&calc.left) || operand_has_nest(&calc.right)
        }
        _ => false,
    }
}

/// Projection(Calculation{Query(Nest(A,agg1)), op, Query(Nest(A,agg2))}) を
/// ProjectionProjectionMatch { op: Arithmetic } に解決する。
fn resolve_projection_arithmetic(
    lens: &Lens,
    calc: crate::query::ast::CalculationNode,
) -> Result<ResolvedNode> {
    let arith_op = calc.op;
    let (common_key, left_nv, right_nv) = if let Ok((k, l_nv)) =
        resolve_nest_operand_extract_key(lens, calc.left.clone())
    {
        let r_nv = resolve_nest_calc_operand(lens, calc.right, &k)?;
        (k, l_nv, r_nv)
    } else if let Ok((k, r_nv)) =
        resolve_nest_operand_extract_key(lens, calc.right.clone())
    {
        let l_nv = resolve_nest_calc_operand(lens, calc.left, &k)?;
        (k, l_nv, r_nv)
    } else {
        bail!("Could not extract GROUP BY key from either side of arithmetic expression");
    };

    validate_calculation_types(&left_nv, &right_nv, arith_op)?;
    Ok(ResolvedNode::ProjectionProjectionMatch {
        left_operand: common_key.clone(),
        left_nvalue: left_nv,
        left_context: None,
        op: ProjectionOp::Arithmetic(arith_op),
        right_operand: common_key,
        right_nvalue: right_nv,
        right_context: None,
    })
}

/// Operand を解決し、GROUP BY キー (operand) と nvalue を返す。
/// Nest(A, agg) → (A_resolved, agg_resolved)
/// Calculation{...} → (common_key, Calculation{resolved_agg, op, resolved_agg})
fn resolve_nest_operand_extract_key(
    lens: &Lens,
    operand: Operand,
) -> Result<(ResolvedOperand, ResolvedOperand)> {
    match operand {
        Operand::Query(node) => {
            let resolved = resolve_query_node(lens, *node)?;
            match resolved {
                ResolvedNode::Projection {
                    operand: key,
                    nvalue: Some(nv),
                    ..
                } => Ok((key, nv)),
                _ => bail!(
                    "Arithmetic Nest operand must resolve to Projection with nvalue"
                ),
            }
        }
        Operand::Calculation(calc) => {
            let arith_op = calc.op;
            // 左辺または右辺からキーの抽出を試みる
            let (key, left_nv, right_nv) = if let Ok((k, l_nv)) =
                resolve_nest_operand_extract_key(lens, calc.left.clone())
            {
                let r_nv = resolve_nest_calc_operand(lens, calc.right, &k)?;
                (k, l_nv, r_nv)
            } else if let Ok((k, r_nv)) =
                resolve_nest_operand_extract_key(lens, calc.right.clone())
            {
                let l_nv = resolve_nest_calc_operand(lens, calc.left, &k)?;
                (k, l_nv, r_nv)
            } else {
                bail!("Could not extract key from either side of calculation");
            };

            validate_calculation_types(&left_nv, &right_nv, arith_op)?;
            let combined = ResolvedOperand::Calculation(Box::new(
                ResolvedCalculationNode {
                    left: left_nv,
                    op: arith_op,
                    right: right_nv,
                },
            ));
            Ok((key, combined))
        }
        Operand::Literal(_) => {
            bail!("Literal does not contain a key");
        }
        _ => bail!("Expected Query(Nest) or Calculation in arithmetic Nest"),
    }
}

/// Nest 算術式のオペランドを解決する（キーは既知）。
fn resolve_nest_calc_operand(
    lens: &Lens,
    operand: Operand,
    expected_key: &ResolvedOperand,
) -> Result<ResolvedOperand> {
    match operand {
        Operand::Query(node) => {
            let resolved = resolve_query_node(lens, *node)?;
            match resolved {
                ResolvedNode::Projection {
                    operand: key,
                    nvalue: Some(nv),
                    ..
                } => {
                    if key != *expected_key {
                        bail!("Mismatched GROUP BY keys in arithmetic Nest expression");
                    }
                    Ok(nv)
                }
                _ => bail!(
                    "Arithmetic Nest operand must resolve to Projection with nvalue"
                ),
            }
        }
        Operand::Calculation(calc) => {
            let arith_op = calc.op;
            let left =
                resolve_nest_calc_operand(lens, calc.left, expected_key)?;
            let right =
                resolve_nest_calc_operand(lens, calc.right, expected_key)?;
            validate_calculation_types(&left, &right, arith_op)?;
            Ok(ResolvedOperand::Calculation(Box::new(
                ResolvedCalculationNode {
                    left,
                    op: arith_op,
                    right,
                },
            )))
        }
        Operand::Literal(l) => Ok(ResolvedOperand::Literal(l)),
        _ => bail!("Unexpected operand type in arithmetic Nest expression"),
    }
}

/// Nest の左辺から ResolvedOperand と、付随するコンテキスト (And などのフィルタ) を抽出するヘルパー。
fn extract_projection_operand_with_context(
    left: ResolvedNode,
) -> Result<(ResolvedOperand, Option<Box<ResolvedNode>>)> {
    match left {
        ResolvedNode::Projection {
            operand, context, ..
        } => Ok((operand, context)),
        ResolvedNode::And(nodes) => {
            for n in nodes {
                if let ResolvedNode::Projection {
                    operand, context, ..
                } = n
                {
                    return Ok((operand, context));
                }
            }
            bail!("Nest left side And must contain a Projection")
        }
        _ => bail!("Nest left side must resolve to Projection or And"),
    }
}

/// nvalue 付き Projection を ResolvedNode から抽出するヘルパー。
///
/// `extension:` などは展開後に `And([TypedTag(is_dir:false), Projection { nvalue }])` に
/// なるため、And の中から Projection を取り出し、残りをコンテキストフィルタとして返す。
///
/// # 戻り値
/// `(operand, nvalue, context)` — context は is_dir:false などのフィルタノード
pub(crate) fn extract_nvalue_projection_parts(
    node: ResolvedNode,
) -> Result<(ResolvedOperand, ResolvedOperand, Option<Box<ResolvedNode>>)> {
    match node {
        ResolvedNode::Projection {
            operand,
            nvalue: Some(nv),
            context,
        } => Ok((operand, nv, context)),
        ResolvedNode::ProjectionMatch {
            operand,
            nvalue,
            context,
            ..
        } => Ok((operand, nvalue, context)),
        ResolvedNode::And(mut nodes) => {
            // nvalue 付き Projection または ProjectionMatch の位置を探す
            let proj_idx = nodes.iter().position(|n| {
                matches!(
                    n,
                    ResolvedNode::Projection { nvalue: Some(_), .. }
                        | ResolvedNode::ProjectionMatch { .. }
                )
            });
            if let Some(idx) = proj_idx {
                let proj = nodes.remove(idx);
                let (operand, nv, proj_ctx) = match proj {
                    ResolvedNode::Projection {
                        operand,
                        nvalue: Some(nv),
                        context,
                    } => (operand, nv, context),
                    ResolvedNode::ProjectionMatch {
                        operand,
                        nvalue,
                        context,
                        ..
                    } => (operand, nvalue, context),
                    _ => unreachable!(),
                };

                // 残りのノード（is_dir:false などのフィルタ）をコンテキストとして統合
                let filter_ctx = if nodes.is_empty() {
                    None
                } else if nodes.len() == 1 {
                    Some(Box::new(nodes.remove(0)))
                } else {
                    Some(Box::new(ResolvedNode::And(nodes)))
                };

                // proj_ctx と filter_ctx をマージ
                let merged_ctx = match (proj_ctx, filter_ctx) {
                    (None, fc) => fc,
                    (pc, None) => pc,
                    (Some(pc), Some(fc)) => {
                        Some(Box::new(ResolvedNode::And(vec![*pc, *fc])))
                    }
                };
                Ok((operand, nv, merged_ctx))
            } else {
                bail!("And node does not contain a nvalue-bearing Projection")
            }
        }
        other => bail!(
            "Both sides of Nest comparison must resolve to nvalue-bearing Projection, got: {:?}",
            other
        ),
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
        // (nest_calc) :> 100 → Nest 算術演算子の ProjectionMatch に変換
        (Operand::Calculation(calc), Operand::Literal(lab))
            if calc_has_nest_operands(&calc) =>
        {
            let arithmetic_op = calc.op;
            let resolved = resolve_projection_arithmetic(lens, *calc)?;
            match resolved {
                ResolvedNode::ProjectionProjectionMatch {
                    left_operand,
                    left_nvalue,
                    left_context,
                    op: _,
                    right_operand: _,
                    right_nvalue,
                    right_context: _,
                } => Ok(ResolvedNode::ProjectionMatch {
                    operand: left_operand,
                    nvalue: ResolvedOperand::Calculation(Box::new(
                        ResolvedCalculationNode {
                            left: left_nvalue,
                            op: arithmetic_op,
                            right: right_nvalue,
                        },
                    )),
                    op,
                    label: lab,
                    context: left_context,
                }),
                _ => Ok(resolved),
            }
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
        // 100 :< (nest_calc) → Nest 算術演算子の ProjectionMatch に変換（flip）
        (Operand::Literal(lab), Operand::Calculation(calc))
            if calc_has_nest_operands(&calc) =>
        {
            let arithmetic_op = calc.op;
            let resolved = resolve_projection_arithmetic(lens, *calc)?;
            match resolved {
                ResolvedNode::ProjectionProjectionMatch {
                    left_operand,
                    left_nvalue,
                    left_context,
                    op: _,
                    right_operand: _,
                    right_nvalue,
                    right_context: _,
                } => Ok(ResolvedNode::ProjectionMatch {
                    operand: left_operand,
                    nvalue: ResolvedOperand::Calculation(Box::new(
                        ResolvedCalculationNode {
                            left: left_nvalue,
                            op: arithmetic_op,
                            right: right_nvalue,
                        },
                    )),
                    op: flip_op(op),
                    label: lab,
                    context: left_context,
                }),
                _ => Ok(resolved),
            }
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
        // nvalue 付き Projection への比較
        // (parentdir: &: count(ext:jpg)) :> 10 → ProjectionMatch
        (Operand::Query(node), Operand::Literal(lit)) => {
            let resolved = resolve_query_node(lens, *node)?;
            match resolved {
                ResolvedNode::Projection {
                    operand,
                    nvalue: Some(nv),
                    context,
                } => Ok(ResolvedNode::ProjectionMatch {
                    operand,
                    nvalue: nv,
                    op,
                    label: lit,
                    context,
                }),
                ResolvedNode::Projection { .. } => {
                    bail!("Comparison on Projection without nvalue is not supported as label comparison")
                }
                ResolvedNode::And(ref nodes)
                    if nodes.iter().any(|n| n.get_projection().is_some()) =>
                {
                    Ok(resolved)
                }
                _ => bail!(
                    "Query operand in comparison must resolve to Projection, got: {:?}",
                    resolved
                ),
            }
        }
        (Operand::Literal(lit), Operand::Query(node)) => {
            let resolved = resolve_query_node(lens, *node)?;
            match resolved {
                ResolvedNode::Projection {
                    operand,
                    nvalue: Some(nv),
                    context,
                } => Ok(ResolvedNode::ProjectionMatch {
                    operand,
                    nvalue: nv,
                    op: flip_op(op),
                    label: lit,
                    context,
                }),
                ResolvedNode::Projection { .. } => {
                    bail!("Comparison on Projection without nvalue is not supported as label comparison")
                }
                ResolvedNode::And(ref nodes)
                    if nodes.iter().any(|n| n.get_projection().is_some()) =>
                {
                    Ok(resolved)
                }
                _ => bail!(
                    "Query operand in comparison must resolve to Projection, got: {:?}",
                    resolved
                ),
            }
        }
        // Query vs Query (両辺が Nest、比較演算子)
        // extension: などが And([is_dir:false, Projection { nvalue }]) に展開されるケースも処理する
        (Operand::Query(l_node), Operand::Query(r_node)) => {
            let res_l = resolve_query_node(lens, *l_node)?;
            let res_r = resolve_query_node(lens, *r_node)?;

            let (lo, lnv, lc) = extract_nvalue_projection_parts(res_l)?;
            let (ro, rnv, rc) = extract_nvalue_projection_parts(res_r)?;

            Ok(ResolvedNode::ProjectionProjectionMatch {
                left_operand: lo,
                left_nvalue: lnv,
                left_context: lc,
                op: ProjectionOp::Comparison(op),
                right_operand: ro,
                right_nvalue: rnv,
                right_context: rc,
            })
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

        // Phase 5: 最適化パスの適用
        let optimized = crate::query::lens_optimizer::optimize(resolved);

        Ok(Self {
            lens,
            expanded_query: expanded,
            resolved_query: optimized,
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

    /// nvalue（評価値）定義を返す
    pub fn get_nvalue(&self) -> Option<&ResolvedOperand> {
        self.resolved_query.get_nvalue()
    }

    pub fn get_nvalue_combined(&self) -> Option<ResolvedOperand> {
        self.resolved_query.get_nvalue_combined()
    }

    /// nvalue に対するフィルタ条件を返す（resolved_query から再帰的に探索）
    pub fn get_nvalue_condition(&self) -> Option<(&ComparisonOp, &Label)> {
        self.resolved_query.get_nvalue_condition()
    }

    /// スカラー式を返す
    pub fn get_scalar_expression(&self) -> Option<ResolvedOperand> {
        match &self.resolved_query {
            ResolvedNode::Aggregation(agg) => {
                Some(ResolvedOperand::Aggregation(agg.clone()))
            }
            ResolvedNode::Projection { operand: op, .. } => {
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
        let projection = ResolvedNode::Projection {
            operand: ResolvedOperand::TagRef {
                tag_type: TagType::Base(SType::Size),
                storage: StorageMapping::Column(Col::Size),
                sql_type: SqlType::BIGINT,
            },
            nvalue: None,
            context: None,
        };
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
        let node = ResolvedNode::Projection {
            operand: operand.clone(),
            nvalue: None,
            context: None,
        };

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

    // ========== Nest (Phase 3) テスト ==========

    #[test]
    fn test_resolve_nest_nvalue() {
        // parentdir: &: count(extension:jpg) → Projection { nvalue: Some(Count) }
        let resolver = Resolver::new("parentdir: &: count(extension:jpg)")
            .expect("nest with agg should resolve");
        match &resolver.resolved_query {
            ResolvedNode::Projection {
                nvalue, operand, ..
            } => {
                assert!(nvalue.is_some(), "nvalue should be present");
                match nvalue.as_ref().unwrap() {
                    ResolvedOperand::Aggregation(
                        ResolvedAggregationNode::Count(_),
                    ) => {}
                    other => panic!("Expected Count nvalue, got {:?}", other),
                }
                // operand は parentdir の TagRef
                match operand {
                    ResolvedOperand::TagRef { tag_type, .. } => {
                        assert_eq!(tag_type.as_str(), "parentdir");
                    }
                    _ => panic!("Expected TagRef operand, got {:?}", operand),
                }
            }
            other => panic!("Expected Projection with nvalue, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_nest_sum_nvalue() {
        // project: &: sum(size:) → Projection { nvalue: Some(Sum) }
        let resolver = Resolver::new("project: &: sum(size:)")
            .expect("nest with sum should resolve");
        match &resolver.resolved_query {
            ResolvedNode::Projection {
                nvalue, operand, ..
            } => {
                assert!(nvalue.is_some());
                match nvalue.as_ref().unwrap() {
                    ResolvedOperand::Aggregation(
                        ResolvedAggregationNode::Arithmetic {
                            op: crate::query::ast::ArithmeticAggOp::Sum,
                            ..
                        },
                    ) => {}
                    other => panic!("Expected Sum nvalue, got {:?}", other),
                }
                match operand {
                    ResolvedOperand::TagRef { tag_type, .. } => {
                        assert_eq!(tag_type.as_str(), "project");
                    }
                    _ => panic!("Expected TagRef operand"),
                }
            }
            other => panic!("Expected Projection with nvalue, got {:?}", other),
        }
    }

    #[test]
    fn test_projection_no_regression() {
        // size: → Projection { nvalue: None }
        let resolver =
            Resolver::new("size:").expect("simple projection should resolve");
        match &resolver.resolved_query {
            ResolvedNode::Projection { nvalue, .. } => {
                assert!(
                    nvalue.is_none(),
                    "plain projection should have no nvalue"
                );
            }
            other => panic!("Expected Projection, got {:?}", other),
        }
    }

    /// `extension:` のように展開後 And([filter, Projection]) になるケースでも
    /// get_nvalue() が再帰的に nvalue を取得できることを確認
    #[test]
    fn test_get_nvalue_through_and() {
        // extension: は And([is_dir:false, Projection(extension)]) に展開される
        let resolver = Resolver::new("extension: &: count(name:test)").unwrap();

        // get_projection は And 内の Projection を見つける
        assert!(
            resolver.get_projection().is_some(),
            "Should find projection inside And"
        );

        // get_nvalue も And 内の Projection の nvalue を見つける
        assert!(
            resolver.get_nvalue().is_some(),
            "Should find nvalue inside And (extension: expands to And)"
        );
    }

    /// And でラップされていない純粋な Projection の nvalue も取得できる
    #[test]
    fn test_get_nvalue_direct_projection() {
        // parentdir は展開後も Projection のまま (And でラップされない)
        let resolver =
            Resolver::new("parentdir: &: count(extension:jpg)").unwrap();

        assert!(resolver.get_projection().is_some());
        assert!(
            resolver.get_nvalue().is_some(),
            "Should find nvalue on direct Projection"
        );
    }

    /// 右辺 Comparison (agg vs literal) → logical_resolver で分配され
    /// (parentdir: &: count(ext:jpg)) :> 1 のラベル比較に変換される。
    /// nvalue 付き Projection として解決される（nvalue でフィルタ済み）。
    #[test]
    fn test_resolve_nest_comparison_distributed() {
        let resolver =
            Resolver::new("parentdir: &: (count(extension:jpg) > 1)")
                .expect("nest comparison should resolve");

        // Projection として解決される（nvalue 付き比較は Projection を返す）
        assert!(
            resolver.get_projection().is_some(),
            "Should return Projection"
        );
        let tag = resolver.get_projection().unwrap();
        assert_eq!(tag.as_str(), "parentdir");

        // nvalue が付与されている
        let nvalue = resolver.get_nvalue();
        assert!(nvalue.is_some(), "Should have nvalue");
        assert!(
            matches!(
                nvalue.unwrap(),
                ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_))
            ),
            "nvalue should be Count"
        );

        // nvalue に対するフィルタ条件が保持されている
        let cond = resolver.get_nvalue_condition();
        assert!(cond.is_some(), "Should have nvalue_condition");
    }

    /// 右辺 Comparison (literal op agg) → 反転して分配
    /// `parentdir: &: (1 < count(ext:jpg))` は
    /// `(parentdir: &: count(ext:jpg)) :> 1` に正規化される
    #[test]
    fn test_resolve_nest_comparison_flipped_distributed() {
        let resolver =
            Resolver::new("parentdir: &: (1 < count(extension:jpg))")
                .expect("flipped nest comparison should resolve");

        assert!(resolver.get_projection().is_some());
        assert!(resolver.get_nvalue().is_some());
        assert!(
            resolver.get_nvalue_condition().is_some(),
            "Should have nvalue_condition"
        );
    }

    /// 右辺 Comparison (agg vs agg) → 両辺が Nest で包まれて分配
    /// `parentdir: &: (avg(size:) == sum(size:))`
    /// → `(parentdir: &: avg(size:)) := (parentdir: &: sum(size:))`
    /// 両辺が Query の場合は Phase 4 では未対応
    #[test]
    fn test_resolve_nest_comparison_agg_agg_distributed() {
        // 両辺が Query(Nest) の比較をサポート済み
        let resolver = Resolver::new(
            "parentdir: &: (avg(size:) == sum(size:))",
        )
        .expect("Query-vs-Query comparison (nested) should now resolve");

        // 解析結果が ProjectionProjectionMatch または MergedProjectionMatch であることを確認
        assert!(
            matches!(
                resolver.resolved_query,
                ResolvedNode::ProjectionProjectionMatch { .. }
                    | ResolvedNode::MergedProjectionMatch { .. }
            ),
            "Expected ProjectionProjectionMatch or MergedProjectionMatch, got: {:?}",
            resolver.resolved_query
        );
    }

    /// 右辺スカラー → nvalue: Some(Literal(100))
    #[test]
    fn test_resolve_nest_scalar_right() {
        let resolver = Resolver::new("parentdir: &: 100")
            .expect("nest with scalar right should resolve");

        assert!(resolver.get_projection().is_some());
        let nvalue = resolver.get_nvalue();
        assert!(nvalue.is_some(), "Scalar right should have nvalue");
        assert!(
            matches!(nvalue.unwrap(), ResolvedOperand::Literal(_)),
            "nvalue should be Literal(100)"
        );
    }

    /// 右辺 Calculation → nvalue: Some(Calculation(...))
    /// `parentdir: &: (size: * 2)` は深さ1、nvalue が算術式
    #[test]
    fn test_resolve_nest_calculation_right() {
        let resolver = Resolver::new("parentdir: &: (size: * 2)")
            .expect("nest with calculation right should resolve");

        assert!(resolver.get_projection().is_some());
        let nvalue = resolver.get_nvalue();
        assert!(nvalue.is_some(), "Calculation right should have nvalue");
        assert!(
            matches!(nvalue.unwrap(), ResolvedOperand::Calculation(_)),
            "nvalue should be Calculation, got: {:?}",
            resolver.get_nvalue()
        );
    }

    /// 右辺 Calculation (集約を含む) → nvalue: Some(Calculation(...))
    /// `parentdir: &: (sum(size:) * 2)` も深さ1の算術式 nvalue
    #[test]
    fn test_resolve_and_context_injection() {
        // クエリ: extension:html & parentdir: &: count(extension:jpg)
        // extension:html が parentdir: の context に注入されるべき
        let query = "extension:html & parentdir: &: count(extension:jpg)";
        let result = Resolver::new(query).expect("Should resolve query");
        let node = &result.resolved_query;

        if let ResolvedNode::And(nodes) = node {
            // nodes[0] は Match (extension:html)
            // nodes[1] は Projection (parentdir: &: count(ext:jpg))
            let filter = &nodes[0];
            let proj = &nodes[1];

            if let ResolvedNode::Projection { context, .. } = proj {
                assert!(context.is_some(), "Context should be injected");
                assert_eq!(
                    context.as_deref().unwrap(),
                    filter,
                    "Context should be the adjacent filter"
                );
            } else {
                panic!("Expected Projection node at index 1, got {:?}", proj);
            }
        } else {
            panic!("Expected And node, got {:?}", result.resolved_query);
        }
    }

    #[test]
    fn test_resolve_query_vs_query_comparison() {
        // クエリ: (parentdir: &: count(ext:jpg)) == (parentdir: &: count(ext:png))
        let query = "(parentdir: &: count(extension:jpg)) == (parentdir: &: count(extension:png))";
        // 現在は解決ロジックを実装済みなので、Ok(ProjectionProjectionMatch) が返るはず
        let result = Resolver::new(query)
            .expect("Should resolve Query vs Query comparison");
        assert!(
            matches!(
                result.resolved_query,
                ResolvedNode::ProjectionProjectionMatch { .. }
                    | ResolvedNode::MergedProjectionMatch { .. }
            ),
            "Expected ProjectionProjectionMatch or MergedProjectionMatch, got: {:?}",
            result.resolved_query
        );
    }
}

#[cfg(test)]
mod tests_integration {
    use super::*;

    #[test]
    fn test_resolver_new_applies_optimization() {
        // 同一キーのマージが最適化によって行われるクエリ
        let query =
            "parentdir: &: count(ext:rs) > 0 & parentdir: &: sum(size:) > 1000";
        let resolver = Resolver::new(query).unwrap();

        // 最適化が適用されていれば、ルートは MergedProjectionMatch になっているはず
        assert!(
            matches!(
                resolver.resolved_query,
                crate::query::ResolvedNode::MergedProjectionMatch { .. }
            ),
            "Resolved query should be optimized (merged). Got: {:?}",
            resolver.resolved_query
        );
    }
}
