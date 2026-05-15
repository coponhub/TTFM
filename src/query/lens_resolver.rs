//! # 物理解決器（Lens Resolver）
//!
//! Query AST を OneView の物理構造にマッピングします。
//!
//! ## 責務
//!
//! 1. **StorageMappingの決定: Fixed/Basic/Composite の判定
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
use duckdb::types::Value;
use sea_query::{BinOper, Condition, Expr, SimpleExpr};

/// 物理マッピングが解決された後のクエリノード。
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedNode {
    And(Vec<ResolvedNode>),
    Or(Vec<ResolvedNode>),
    Difference(Box<ResolvedNode>, Box<ResolvedNode>),
    /// 投影クエリ用（ネスト構造を含む）。
    /// 深さ1の純粋なプロジェクション（例: `extension:`）も `keys: vec![op]` として表現する。
    Nest {
        keys: Vec<ResolvedOperand>,
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
    /// 算術演算同士の比較 (例: (size: - 100) :> (size: * 0.1))
    CalculationCalculationMatch {
        left_calc: ResolvedCalculationNode,
        op: ComparisonOp,
        right_calc: ResolvedCalculationNode,
    },
    /// リテラル同士のスカラー比較 (例: 10 > 2)
    ScalarMatch {
        left: Label,
        op: ComparisonOp,
        right: Label,
    },
    /// ラベル集合演算（Projection / Nest 同士の And/Or/Difference）。
    /// 例: `cat: & flavor:` → Intersect, `cat: | flavor:` → Union, `fruit: -: veggie:` → Except
    LabelSetOp {
        op: LabelSetOpKind,
        operands: Vec<ResolvedNode>,
    },
    /// nvalue 付き Nest への比較。フィルタされたラベル集合を返す。
    /// 例: `(parentdir: &: count(ext:jpg)) :> 10`
    NestMatch {
        keys: Vec<ResolvedOperand>,
        nvalue: ResolvedOperand,
        op: ComparisonOp,
        label: Label,
        /// このプロジェクション（およびそのnvalue集計）に適用されるべきフィルタ
        context: Option<Box<ResolvedNode>>,
    },
    /// nvalue 付き Nest 同士の比較または算術演算。
    /// - op: Comparison → フィルタ結果（どのグループが条件を満たすか）
    /// - op: Arithmetic → nvalue 付き Nest 結果（各グループの計算値）
    NestNestMatch {
        left_keys: Vec<ResolvedOperand>,
        left_nvalue: ResolvedOperand,
        left_context: Option<Box<ResolvedNode>>,
        op: NestMatchOp,
        right_keys: Vec<ResolvedOperand>,
        right_nvalue: ResolvedOperand,
        right_context: Option<Box<ResolvedNode>>,
    },
    /// マージされた nvalue 付き Nest マッチ。
    /// 共通の GROUP BY キー（operand）に対して複数の条件を AND/OR で適用する。
    MergedNestMatch {
        keys: Vec<ResolvedOperand>,
        matches: Vec<NestMatchCondition>,
        is_or: bool,
    },
}

/// ラベル集合演算の種類。
#[derive(Debug, Clone, PartialEq)]
pub enum LabelSetOpKind {
    Intersect,
    Union,
    Except,
}

/// nvalue 付き Nest 同士を結ぶ演算子。
/// 比較（フィルタ）を扱う。
#[derive(Debug, Clone, PartialEq)]
pub enum NestMatchOp {
    Comparison(ComparisonOp),
}

/// 同一 operand（GROUP BY キー）に対する検索条件。
#[derive(Debug, Clone, PartialEq)]
pub struct NestMatchCondition {
    pub nvalue: ResolvedOperand,
    pub op: NestMatchOp,
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
    /// 集約ノードの内部 ResolvedNode を返します。
    pub fn inner_node(&self) -> &ResolvedNode {
        match self {
            ResolvedAggregationNode::Count(inner) => inner,
            ResolvedAggregationNode::Arithmetic { inner, .. } => inner,
        }
    }

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
    /// タグ参照（投影キー）
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

    /// タグ参照が含まれているかチェックします（EAV 計算比較用）。
    pub fn contains_tag(&self) -> bool {
        match self {
            ResolvedOperand::TagRef { storage, .. } => {
                matches!(storage, StorageMapping::Basic { .. })
            }
            ResolvedOperand::Calculation(calc) => calc.contains_tag(),
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

    /// オペランドの値（Value）を人間が読みやすいラベル文字列に変換します。
    pub fn resolve_label(&self, lens: &Lens, value: &Value) -> String {
        match self {
            ResolvedOperand::TagRef { tag_type, .. } => {
                lens.resolve_label(tag_type, value).to_string()
            }
            _ => {
                // Calculation 等の場合はデフォルトの 'nvalue' として解決（数値フォーマット等）。
                lens.resolve_label(&TagType::from("nvalue"), value)
                    .to_string()
            }
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

    /// 直接の子オペランドを返す。
    /// - `Calculation`: left / right
    /// - `Aggregation`: 集約内部のプロジェクションオペランド（存在する場合）
    pub fn children(&self) -> Vec<&ResolvedOperand> {
        match self {
            ResolvedOperand::Calculation(calc) => vec![&calc.left, &calc.right],
            ResolvedOperand::Aggregation(agg) => {
                let inner = match agg {
                    ResolvedAggregationNode::Count(node) => node,
                    ResolvedAggregationNode::Arithmetic { inner, .. } => inner,
                };
                let (_, _, operand) = inner.extract_agg_parts();
                operand.map(|op| vec![op]).unwrap_or_default()
            }
            _ => vec![],
        }
    }

    /// ツリーを後順（post-order）で fold する。
    /// 葉から順に `f(node, child_results)` が呼ばれる。
    pub fn fold<T, F>(&self, f: &F) -> T
    where
        F: Fn(&ResolvedOperand, Vec<T>) -> T,
    {
        let child_results =
            self.children().into_iter().map(|c| c.fold(f)).collect();
        f(self, child_results)
    }

    /// 深さ優先（前順）で全オペランドを列挙する。
    pub fn walk(&self) -> Vec<&ResolvedOperand> {
        let mut result = vec![self];
        for child in self.children() {
            result.extend(child.walk());
        }
        result
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

    /// タグ参照が含まれているかチェックします（EAV 計算比較用）。
    pub fn contains_tag(&self) -> bool {
        self.left.contains_tag() || self.right.contains_tag()
    }

    pub fn to_condition(&self) -> Condition {
        self.left.to_condition().add(self.right.to_condition())
    }
}

impl ResolvedNode {
    /// 直接の子 `ResolvedNode` を返す。
    /// `build_pick_sql` が再帰する対象のみを含む。
    pub fn children(&self) -> Vec<&ResolvedNode> {
        match self {
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().collect()
            }
            ResolvedNode::LabelSetOp { operands, .. } => {
                operands.iter().collect()
            }
            ResolvedNode::Difference(l, r) => vec![l.as_ref(), r.as_ref()],

            ResolvedNode::Nest {
                context: Some(ctx), ..
            } => {
                vec![ctx.as_ref()]
            }
            _ => vec![],
        }
    }

    /// ツリーを後順（post-order）で fold する。
    /// 葉から順に `f(node, child_results)` が呼ばれる。
    pub fn fold<T, F>(&self, f: &F) -> T
    where
        F: Fn(&ResolvedNode, Vec<T>) -> T,
    {
        let child_results =
            self.children().into_iter().map(|c| c.fold(f)).collect();
        f(self, child_results)
    }

    /// 深さ優先（前順）で全ノードを列挙する。
    /// Aggregation 系ノードの inner_node にも降りる。
    /// fold() は children() ベースのまま変更しない。
    pub fn walk(&self) -> Vec<&ResolvedNode> {
        let mut result = vec![self];
        for child in self.children() {
            result.extend(child.walk());
        }
        // children() に含まれない Aggregation の inner_node にも降りる
        match self {
            ResolvedNode::Aggregation(agg)
            | ResolvedNode::AggregationMatch { agg, .. }
            | ResolvedNode::AggregationTagMatch { agg, .. }
            | ResolvedNode::AggregationCalculationMatch { agg, .. } => {
                result.extend(agg.inner_node().walk());
            }
            ResolvedNode::AggregationAggregationMatch {
                left, right, ..
            } => {
                result.extend(left.inner_node().walk());
                result.extend(right.inner_node().walk());
            }
            _ => {}
        }
        result
    }

    /// 自身、または入れ子のいずれかに Projection / NestMatch を含むかチェックします。
    pub fn is_projection_recursive(&self) -> bool {
        match self {
            ResolvedNode::Nest { .. }
            | ResolvedNode::NestMatch { .. }
            | ResolvedNode::NestNestMatch { .. }
            | ResolvedNode::MergedNestMatch { .. }
            | ResolvedNode::LabelSetOp { .. } => true,
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().any(|n| n.is_projection_recursive())
            }
            ResolvedNode::Difference(l, r) => {
                l.is_projection_recursive() || r.is_projection_recursive()
            }

            _ => false,
        }
    }

    /// 自身、または入れ子の全ての Projection / NestMatch にコンテキストを注入します。
    pub fn inject_context(&mut self, context: ResolvedNode) {
        match self {
            ResolvedNode::Nest {
                context: ref mut ctx,
                ..
            }
            | ResolvedNode::NestMatch {
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
            ResolvedNode::NestNestMatch {
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
            ResolvedNode::LabelSetOp { operands, .. } => {
                for op in operands {
                    op.inject_context(context.clone());
                }
            }
            ResolvedNode::Difference(l, r) => {
                l.inject_context(context.clone());
                r.inject_context(context);
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

            ResolvedNode::Nest { keys, context, .. } => {
                let mut cond = keys.first().unwrap().to_condition();
                if let Some(ctx) = context {
                    cond = cond.add(ctx.to_condition());
                }
                cond
            }
            ResolvedNode::NestMatch { keys, context, .. } => {
                let mut cond = Condition::all();
                for k in keys {
                    cond = cond.add(k.to_condition());
                }
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
            | ResolvedNode::CalculationCalculationMatch { .. }
            | ResolvedNode::NestNestMatch { .. }
            | ResolvedNode::MergedNestMatch { .. }
            | ResolvedNode::ScalarMatch { .. }
            | ResolvedNode::LabelSetOp { .. } => {
                // 算術演算や集約比較は、単一の WHERE 句の Condition だけでは不十分な場合が多いため、
                // build_pick_sql 側で完全に SelectStatement を構築する。
                // 連結用には Condition::any() を返しておく。
                Condition::any()
            }
        }
    }

    /// このノードが投影（Nest）を目的としている場合、その対象の型を返します。
    pub fn get_projection(&self) -> Option<TagType> {
        match self {
            ResolvedNode::Nest { keys, .. } => {
                extract_tag_type_from_operand(keys.first().unwrap())
            }
            ResolvedNode::NestMatch { keys, .. } => {
                extract_tag_type_from_operand(keys.first().unwrap())
            }
            ResolvedNode::NestNestMatch { .. } => None, // 比較演算子 → フラットリスト (Lv.1)
            ResolvedNode::MergedNestMatch { keys, .. } => {
                extract_tag_type_from_operand(keys.first().unwrap())
            }
            // Intersect: 常に先頭オペランドの投影型を返す
            // 仕様: Proj/Nest の & 演算は結果が必ず Lv.2+ Projection になる
            ResolvedNode::LabelSetOp {
                op: LabelSetOpKind::Intersect,
                operands,
            } => operands.first().and_then(|n| n.get_projection()),
            // Union:
            //   同一キー構造 → Lv.n（ラベル値の和集合）
            //   Lv.2 | Lv.2（異なるキー）→ Lv.2（混合 Projection）
            //   Lv.3+ | Lv.3+（共通プレフィックスなし）→ Lv.1（フラット、None）
            ResolvedNode::LabelSetOp {
                op: LabelSetOpKind::Union,
                operands,
            } => {
                let first =
                    operands.first().and_then(|n| n.get_projection())?;
                if operands
                    .iter()
                    .all(|n| n.get_projection() == Some(first.clone()))
                {
                    Some(first)
                } else {
                    // Lv.2 | Lv.2（異なるキー）→ Lv.2 混合 Projection
                    let all_lv2 = operands
                        .iter()
                        .all(|n| get_nest_keys_len(n) == Some(1));
                    if all_lv2 {
                        Some(first)
                    } else {
                        None
                    }
                }
            }
            // Except: 左辺が Proj/Nest なら左辺の投影型を返す
            //   仕様: Lv.n(Proj/Nest) -: Lv.m → Lv.n
            //         TypedTag -: Proj → Lv.1 (左辺の get_projection が None → None)
            ResolvedNode::LabelSetOp {
                op: LabelSetOpKind::Except,
                operands,
            } => operands.first().and_then(|n| n.get_projection()),
            ResolvedNode::And(nodes) => {
                nodes.iter().find_map(|n| n.get_projection())
            }
            ResolvedNode::Or(nodes) => {
                // Or は全オペランドが同一ルートタグの場合のみ Some を返す（混在は None → 2-C フラット）
                let first = nodes.first().and_then(|n| n.get_projection())?;
                if nodes
                    .iter()
                    .all(|n| n.get_projection() == Some(first.clone()))
                {
                    Some(first)
                } else {
                    None
                }
            }
            ResolvedNode::Difference(l, _) => l.get_projection(),
            _ => None,
        }
    }

    /// Or が異なるタグ型のオペランドを混在させる「混在投影」クエリかどうかを返します。
    pub fn is_mixed_projection_query(&self) -> bool {
        match self {
            ResolvedNode::Or(nodes) => {
                // すべてのオペランドが同一ルートタグを持てば混在ではない
                let projs: Vec<_> =
                    nodes.iter().map(|n| n.get_projection()).collect();
                let first = projs.first().and_then(|t| t.clone());
                first.is_none() || projs.iter().any(|t| *t != first)
            }
            ResolvedNode::And(nodes) => {
                nodes.iter().any(|n| n.is_mixed_projection_query())
            }
            _ => false,
        }
    }

    pub fn get_nvalue(&self) -> Option<&ResolvedOperand> {
        match self {
            ResolvedNode::Nest { nvalue, .. } => nvalue.as_ref(),
            ResolvedNode::NestMatch { nvalue, .. } => Some(nvalue),
            ResolvedNode::NestNestMatch { left_nvalue, .. } => {
                Some(left_nvalue)
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_nvalue())
            }
            ResolvedNode::Difference(l, _) => l.get_nvalue(),
            ResolvedNode::MergedNestMatch { matches, .. } => {
                matches.first().map(|m| &m.nvalue)
            }
            _ => None,
        }
    }

    /// nvalueの再構築（主に算術演算のNestNestMatch用）
    /// get_nvalue() は参照を返すが、NestNestMatch(Arithmetic) の場合は
    /// 左右のオペランドを結合したCalculationを所有権付きで返す必要があるため新設。
    pub fn get_nvalue_combined(&self) -> Option<ResolvedOperand> {
        match self {
            ResolvedNode::Nest { nvalue, .. } => nvalue.clone(),
            ResolvedNode::NestMatch { nvalue, .. } => Some(nvalue.clone()),
            ResolvedNode::NestNestMatch { left_nvalue, .. } => {
                Some(left_nvalue.clone())
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_nvalue_combined())
            }
            ResolvedNode::Difference(l, _) => l.get_nvalue_combined(),
            ResolvedNode::MergedNestMatch { matches, .. } => {
                matches.first().map(|m| m.nvalue.clone())
            }
            _ => None,
        }
    }

    /// Nest が Calculation を含むかどうか（ラベルグループ取得時に使用）
    pub fn get_projection_operand(&self) -> Option<&ResolvedOperand> {
        match self {
            ResolvedNode::Nest { keys, .. } => keys.first(),
            ResolvedNode::NestMatch { keys, .. } => keys.first(),
            ResolvedNode::NestNestMatch { left_keys, .. } => left_keys.first(),
            ResolvedNode::MergedNestMatch { keys, .. } => keys.first(),
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_projection_operand())
            }
            ResolvedNode::Difference(l, _) => l.get_projection_operand(),
            _ => None,
        }
    }

    pub fn get_projection_operands(&self) -> Option<&[ResolvedOperand]> {
        match self {
            ResolvedNode::Nest { keys, .. }
            | ResolvedNode::NestMatch { keys, .. }
            | ResolvedNode::MergedNestMatch { keys, .. } => {
                Some(keys.as_slice())
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_projection_operands())
            }
            ResolvedNode::Difference(l, _) => l.get_projection_operands(),
            node => node
                .get_projection_operand()
                .map(|op| std::slice::from_ref(op)),
        }
    }

    /// ノードからコンテキスト（フィルタ）を取得します。
    pub fn get_context(&self) -> Option<&ResolvedNode> {
        match self {
            ResolvedNode::Nest { context, .. }
            | ResolvedNode::NestMatch { context, .. } => context.as_deref(),
            ResolvedNode::NestNestMatch { left_context, .. } => {
                left_context.as_deref()
            }
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                // Nest を含まず、単にフィルタとして機能する要素をコンテキストとしてみなす
                // ※現在は簡略化のため、最初のノードを返すか、さらなるロジックが必要
                // issue #4 の解決のためには、ここを適切に処理する必要がある
                nodes.iter().find_map(|n| n.get_context())
            }
            _ => None,
        }
    }

    /// 集約 inner ノードとして見たときのフィルタコンテキストを返します。
    /// `And([filter, Nest])` ラッパーでは And の filter 側ノードを返します。
    /// 通常の Nest / NestMatch では .context フィールドを返します。
    pub fn get_agg_context(&self) -> Option<&ResolvedNode> {
        match self {
            ResolvedNode::Nest { context, .. }
            | ResolvedNode::NestMatch { context, .. } => context.as_deref(),
            ResolvedNode::And(nodes) => {
                nodes.iter().find_map(|n| n.get_agg_context())
            }
            _ => None,
        }
    }

    /// ネストされた Nest を再帰的に探索して返します。
    pub fn get_nested_projection(&self) -> Option<&ResolvedOperand> {
        match self {
            ResolvedNode::Nest { keys, .. } => keys.first(),
            ResolvedNode::NestMatch { keys, .. } => keys.first(),
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_nested_projection())
            }
            ResolvedNode::Difference(l, _) => l.get_nested_projection(),
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

            // 1. 自身が Nest/NestMatch の場合はフィルタなし
            if matches!(
                self,
                ResolvedNode::Nest { .. } | ResolvedNode::NestMatch { .. }
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

    /// NestMatch の比較条件を再帰的に探索して返します。
    pub fn get_nvalue_condition(&self) -> Option<(&ComparisonOp, &Label)> {
        match self {
            ResolvedNode::NestMatch { op, label, .. } => Some((op, label)),
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_nvalue_condition())
            }
            ResolvedNode::Difference(l, _) => l.get_nvalue_condition(),
            ResolvedNode::MergedNestMatch { matches, .. } => {
                matches.iter().find_map(|m| {
                    let crate::query::lens_resolver::NestMatchOp::Comparison(
                        op,
                    ) = &m.op;
                    if let crate::query::ResolvedOperand::Literal(label) =
                        &m.right
                    {
                        return Some((op, label));
                    }
                    None
                })
            }
            _ => None,
        }
    }

    /// スカラー集計・計算の結果ラベル型を返す。
    /// 集計に使用したラベルが単一種類の場合のみ Some(TagType) を返す。
    /// count() / Literal は型を伝播しない。
    pub fn get_scalar_result_label_type(&self) -> Option<TagType> {
        match self {
            ResolvedNode::Aggregation(ResolvedAggregationNode::Count(_)) => None,
            ResolvedNode::Aggregation(ResolvedAggregationNode::Arithmetic {
                inner, ..
            }) => {
                let (_, _, operand) = inner.extract_agg_parts();
                let operand = operand?;
                let mut types = collect_tag_types_from_operand(operand);
                types.sort();
                types.dedup();
                if types.len() == 1 { Some(types.remove(0)) } else { None }
            }
            // sum(size:) + count() 等、top-level Nest{keys:[Calculation(Agg,...)]}
            ResolvedNode::Nest { keys, nvalue: None, .. } => {
                let op = keys.first()?;
                if !op.contains_aggregation() && !op.is_pure_scalar() {
                    return None;
                }
                let mut types = collect_tag_types_from_operand(op);
                types.sort();
                types.dedup();
                if types.len() == 1 { Some(types.remove(0)) } else { None }
            }
            _ => None,
        }
    }

    /// このノードがラベル集合演算（LabelSetOp）かどうかを返します。
    pub fn is_label_set_op(&self) -> bool {
        matches!(self, ResolvedNode::LabelSetOp { .. })
    }

    /// ラベル集合演算ノードを探索して返す（And でラップされている場合も透過する）。
    pub fn get_label_set_op(&self) -> Option<&ResolvedNode> {
        match self {
            ResolvedNode::LabelSetOp { .. } => Some(self),
            ResolvedNode::And(nodes) => {
                nodes.iter().find_map(|n| n.get_label_set_op())
            }
            _ => None,
        }
    }

    /// LabelSetOp 積集合のオペランドとして使える純粋な Projection ノードを返す。
    ///
    /// - `Nest { nvalue: None, .. }` → `Some(self.clone())`
    /// - `LabelSetOp { .. }` → `Some(self.clone())`
    /// - `And([filters..., Nest{nvalue:None}])` → Nest を取り出して返す
    ///   （`extension:` が `And([is_dir:false, Nest{ext, ctx}])` に展開されるケース）
    /// - その他 → `None`
    pub fn as_label_set_op_operand(&self) -> Option<ResolvedNode> {
        match self {
            ResolvedNode::Nest { nvalue: None, .. } => Some(self.clone()),
            ResolvedNode::LabelSetOp { .. } => Some(self.clone()),
            ResolvedNode::And(nodes) => {
                let nests: Vec<_> = nodes
                    .iter()
                    .filter(|n| {
                        matches!(n, ResolvedNode::Nest { nvalue: None, .. })
                    })
                    .collect();
                let non_proj: Vec<_> = nodes
                    .iter()
                    .filter(|n| !n.is_projection_recursive())
                    .collect();
                // フィルタ1つ以上 + 純粋 Nest 1つ のパターンのみ許容
                if nests.len() == 1 && non_proj.len() == nodes.len() - 1 {
                    Some(nests[0].clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// ResolvedNode が Nest である場合の keys.len() を返す（And ラッパーを透過）
/// Union の Lv 判定に使用: Lv.2 = Some(1), Lv.3+ = Some(n>=2), Proj/Nest 以外 = None
fn get_nest_keys_len(node: &ResolvedNode) -> Option<usize> {
    match node {
        ResolvedNode::Nest { keys, .. } => Some(keys.len()),
        ResolvedNode::And(nodes) => {
            nodes.iter().find_map(|n| get_nest_keys_len(n))
        }
        _ => None,
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

/// Operand ツリーに含まれる TagRef のタグ型を収集する。
/// - Calculation: 左右を展開してキューに積む
/// - Aggregation(Arithmetic): extract_agg_parts() で内部 operand を取り出してキューに積む
/// - Aggregation(Count) / Literal: 型を持たないため空
fn collect_tag_types_from_operand(root: &ResolvedOperand) -> Vec<TagType> {
    let mut result = Vec::new();
    let mut queue: Vec<ResolvedOperand> = vec![root.clone()];

    while let Some(op) = queue.pop() {
        match op {
            ResolvedOperand::TagRef { tag_type, .. } => {
                result.push(tag_type);
            }
            ResolvedOperand::Calculation(calc) => {
                queue.push(calc.left.clone());
                queue.push(calc.right.clone());
            }
            ResolvedOperand::Aggregation(
                ResolvedAggregationNode::Arithmetic { ref inner, .. },
            ) => {
                if let (_, _, Some(operand)) = inner.extract_agg_parts() {
                    queue.push(operand.clone());
                }
            }
            ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_))
            | ResolvedOperand::Literal(_) => {}
        }
    }

    result
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
    // Nest は「存在する」ことが条件
    match storage {
        StorageMapping::Fixed(col) => {
            Condition::all().add(Expr::col(*col).is_not_null())
        }
        StorageMapping::Basic { tag_type, .. } => {
            Condition::all().add(check_tag_match(tag_type))
        }
        StorageMapping::Composite => Condition::any(),
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
            StorageMapping::Basic {
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
                    StorageMapping::Basic {
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

            // Phase 2（純粋ケース）: フィルタなしで 2+ Projection → LabelSetOp { Intersect }
            // as_label_set_op_operand() で And([filter, Nest]) → Nest の正規化も行う
            if filters.is_empty() && projections.len() >= 2 {
                let operands: Vec<_> = projections
                    .iter()
                    .filter_map(|&idx| resolved[idx].as_label_set_op_operand())
                    .collect();
                if operands.len() == projections.len() {
                    return Ok(ResolvedNode::LabelSetOp {
                        op: LabelSetOpKind::Intersect,
                        operands,
                    });
                }
            }

            if !filters.is_empty() && !projections.is_empty() {
                let context = if filters.len() == 1 {
                    filters.remove(0)
                } else {
                    ResolvedNode::And(filters)
                };

                for &idx in &projections {
                    resolved[idx].inject_context(context.clone());
                }

                // Phase 2（フィルタ付きケース）: コンテキスト注入後も 2+ Projection →
                // LabelSetOp { Intersect }（各オペランドにフィルタが注入済み）
                if projections.len() >= 2 {
                    let operands: Vec<_> = projections
                        .iter()
                        .filter_map(|&idx| {
                            resolved[idx].as_label_set_op_operand()
                        })
                        .collect();
                    if operands.len() == projections.len() {
                        return Ok(ResolvedNode::LabelSetOp {
                            op: LabelSetOpKind::Intersect,
                            operands,
                        });
                    }
                }
            }

            Ok(ResolvedNode::And(resolved))
        }
        QueryNode::Or(nodes) => {
            let mut resolved = Vec::new();
            for n in nodes {
                resolved.push(resolve_query_node(lens, n)?);
            }
            // すべてのノードが Projection の場合 LabelSetOp{Union} に変換（型違いも許容）
            let operands: Vec<_> = resolved
                .iter()
                .filter_map(|n| n.as_label_set_op_operand())
                .collect();
            if operands.len() == resolved.len() && operands.len() >= 2 {
                return Ok(ResolvedNode::LabelSetOp {
                    op: LabelSetOpKind::Union,
                    operands,
                });
            }
            Ok(ResolvedNode::Or(resolved))
        }
        QueryNode::Difference(l, r) => {
            let rl = resolve_query_node(lens, *l)?;
            let rr = resolve_query_node(lens, *r)?;
            // 両辺が Projection の場合 LabelSetOp{Except} に変換
            if let (Some(lo), Some(ro)) =
                (rl.as_label_set_op_operand(), rr.as_label_set_op_operand())
            {
                return Ok(ResolvedNode::LabelSetOp {
                    op: LabelSetOpKind::Except,
                    operands: vec![lo, ro],
                });
            }
            Ok(ResolvedNode::Difference(Box::new(rl), Box::new(rr)))
        }

        // Projection(Calculation{Query(Nest(A,agg1)), op, Query(Nest(A,agg2))})
        // → logical_resolver が算術分配した形式。resolve_projection_arithmetic に委譲。
        QueryNode::Projection(Operand::Calculation(calc))
            if calc_has_nest_operands(&calc) =>
        {
            resolve_projection_arithmetic(lens, *calc)
        }
        QueryNode::Projection(op) => {
            let resolved_op = resolve_operand(lens, &op)?;
            Ok(ResolvedNode::Nest {
                keys: vec![resolved_op],
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
/// 深さ1: `Nest(Proj, Agg/Scalar)` → `Nest { keys, nvalue }`
/// 深さ2+
fn merge_nest_keys(
    left: Vec<ResolvedOperand>,
    right: Vec<ResolvedOperand>,
) -> Vec<ResolvedOperand> {
    let mut merged = left;
    for r in right {
        if !merged.contains(&r) {
            merged.push(r);
        }
    }
    merged
}

fn resolve_nest(
    lens: &Lens,
    nest: crate::query::ast::NestNode,
) -> Result<ResolvedNode> {
    if std::env::var("TTFM_DEBUG").is_ok() {
        println!("DEBUG resolve_nest: entering");
    }
    let left = resolve_query_node(lens, *nest.left)?;
    let right_node = *nest.right;

    let (keys, context) = extract_projection_operand_with_context(left)?;

    match right_node {
        // 深さ1: 右辺が Aggregation → nvalue 付き Nest
        QueryNode::Aggregation(agg) => {
            let resolved_agg = resolve_aggregation(lens, agg)?;
            Ok(ResolvedNode::Nest {
                keys,
                nvalue: Some(ResolvedOperand::Aggregation(resolved_agg)),
                context,
            })
        }
        // 深さ1: 右辺が Projection(Literal) → nvalue にスカラー値を付与
        QueryNode::Projection(Operand::Literal(label)) => {
            Ok(ResolvedNode::Nest {
                keys,
                nvalue: Some(ResolvedOperand::Literal(label)),
                context,
            })
        }
        // 深さ1: 右辺が Projection(Calculation)
        QueryNode::Projection(Operand::Calculation(calc)) => {
            let resolved_calc = resolve_calculation(lens, *calc)?;
            let operand = ResolvedOperand::Calculation(Box::new(resolved_calc));

            // 投影演算（タグ参照を含む）の場合、現在のグループ階層を深化させる。
            // これにより、グループ内の各アイテムに対して計算が行われ、結果がリストとして返される。
            if !operand.is_pure_scalar() {
                let mut new_keys = keys;
                new_keys.push(operand);
                Ok(ResolvedNode::Nest {
                    keys: new_keys,
                    nvalue: None,
                    context,
                })
            } else {
                // スカラー演算（集約やリテラルのみ）の場合、現在のグループの評価値として保持する。
                Ok(ResolvedNode::Nest {
                    keys,
                    nvalue: Some(operand),
                    context,
                })
            }
        }
        // 右辺を一般的に解決し、その結果に応じて分岐する（深さ2+）
        right_node => {
            let resolved_right = resolve_query_node(lens, right_node)?;
            match resolved_right {
                ResolvedNode::Nest {
                    keys: inner_keys,
                    nvalue: inner_nvalue,
                    context: inner_ctx,
                } => {
                    // プロジェクションの場合はキーをマージ
                    let merged_keys = merge_nest_keys(keys, inner_keys);
                    if std::env::var("TTFM_DEBUG").is_ok() {
                        println!(
                            "DEBUG resolve_nest: merged_keys.len() = {}",
                            merged_keys.len()
                        );
                    }
                    let merged_ctx = match (context, inner_ctx) {
                        (None, ic) => ic,
                        (c, None) => c,
                        (Some(c), Some(ic)) => {
                            Some(Box::new(ResolvedNode::And(vec![*c, *ic])))
                        }
                    };
                    Ok(ResolvedNode::Nest {
                        keys: merged_keys,
                        nvalue: inner_nvalue,
                        context: merged_ctx,
                    })
                }
                // And([filters..., Nest{inner_keys, nvalue, ctx}]) の形:
                // extension: などが And([is_dir:false, Nest{extension}]) に展開された場合
                ResolvedNode::And(nodes)
                    if nodes
                        .iter()
                        .any(|n| matches!(n, ResolvedNode::Nest { .. })) =>
                {
                    let mut inner_keys = Vec::new();
                    let mut inner_nvalue = None;
                    let mut inner_ctx: Option<Box<ResolvedNode>> = None;
                    let mut filters = Vec::new();
                    for n in nodes {
                        match n {
                            ResolvedNode::Nest {
                                keys: k,
                                nvalue: nv,
                                context: c,
                            } => {
                                inner_keys = k;
                                inner_nvalue = nv;
                                inner_ctx = c;
                            }
                            other => filters.push(other),
                        }
                    }
                    let merged_keys = merge_nest_keys(keys, inner_keys);
                    // フィルタを context にマージ
                    let filter_node = match filters.len() {
                        0 => None,
                        1 => Some(Box::new(filters.remove(0))),
                        _ => Some(Box::new(ResolvedNode::And(filters))),
                    };
                    let base_ctx = match (context, inner_ctx) {
                        (None, ic) => ic,
                        (c, None) => c,
                        (Some(c), Some(ic)) => {
                            Some(Box::new(ResolvedNode::And(vec![*c, *ic])))
                        }
                    };
                    let merged_ctx = match (base_ctx, filter_node) {
                        (None, f) => f,
                        (c, None) => c,
                        (Some(c), Some(f)) => {
                            Some(Box::new(ResolvedNode::And(vec![*c, *f])))
                        }
                    };
                    Ok(ResolvedNode::Nest {
                        keys: merged_keys,
                        nvalue: inner_nvalue,
                        context: merged_ctx,
                    })
                }
                // LabelSetOp{Union, [Nest{k1}, Nest{k2}, ...]} が右辺に来た場合:
                // 左辺キーを各オペランドにマージして LabelSetOp に昇格させる。
                // 例: `parentdir: &: (tagA: | tagB:)`
                //     → LabelSetOp{Union, [Nest{parentdir+tagA}, Nest{parentdir+tagB}]}
                ResolvedNode::LabelSetOp {
                    op: LabelSetOpKind::Union,
                    operands: right_operands,
                } if right_operands.iter().all(|n| {
                    matches!(
                        n,
                        ResolvedNode::Nest { .. } | ResolvedNode::And(_)
                    )
                }) =>
                {
                    let merged_operands: Vec<ResolvedNode> = right_operands
                        .into_iter()
                        .map(|rn| {
                            // 右辺の各 Nest のキー/コンテキストを取得し、左辺キーとマージ
                            let (inner_keys, inner_nvalue, inner_ctx) = match rn
                            {
                                ResolvedNode::Nest {
                                    keys: k,
                                    nvalue: nv,
                                    context: c,
                                } => (k, nv, c),
                                ResolvedNode::And(nodes) => {
                                    let mut k = Vec::new();
                                    let mut nv = None;
                                    let mut c = None;
                                    for n in nodes {
                                        match n {
                                            ResolvedNode::Nest {
                                                keys: ik,
                                                nvalue: inv,
                                                context: ic,
                                            } => {
                                                k = ik;
                                                nv = inv;
                                                c = ic;
                                            }
                                            other => {
                                                c = Some(Box::new(match c {
                                                    None => other,
                                                    Some(ec) => {
                                                        ResolvedNode::And(vec![
                                                            *ec, other,
                                                        ])
                                                    }
                                                }));
                                            }
                                        }
                                    }
                                    (k, nv, c)
                                }
                                other => (vec![], None, Some(Box::new(other))),
                            };
                            let merged_keys =
                                merge_nest_keys(keys.clone(), inner_keys);
                            let merged_ctx = match (context.clone(), inner_ctx)
                            {
                                (None, ic) => ic,
                                (c, None) => c,
                                (Some(c), Some(ic)) => {
                                    Some(Box::new(ResolvedNode::And(vec![
                                        *c, *ic,
                                    ])))
                                }
                            };
                            ResolvedNode::Nest {
                                keys: merged_keys,
                                nvalue: inner_nvalue,
                                context: merged_ctx,
                            }
                        })
                        .collect();
                    Ok(ResolvedNode::LabelSetOp {
                        op: LabelSetOpKind::Union,
                        operands: merged_operands,
                    })
                }
                // 右辺が Aggregation に解決された場合 (expand_query_node 経由で And([filter, Agg]) 等):
                // nvalue として保持する（QueryNode::Aggregation の直接ケースと同じ扱い）
                ResolvedNode::Aggregation(agg) => Ok(ResolvedNode::Nest {
                    keys,
                    nvalue: Some(ResolvedOperand::Aggregation(agg)),
                    context,
                }),
                filtered_node => {
                    // プロジェクションでない（単なるフィルタなどの）場合は context に追加
                    let merged_ctx = match context {
                        None => Some(Box::new(filtered_node)),
                        Some(c) => Some(Box::new(ResolvedNode::And(vec![
                            *c,
                            filtered_node,
                        ]))),
                    };
                    Ok(ResolvedNode::Nest {
                        keys,
                        nvalue: None,
                        context: merged_ctx,
                    })
                }
            }
        }
    }
}

/// Calculation 内に Query(Nest(...)) オペランドが含まれるか確認するヘルパー。
fn calc_has_nest_operands(calc: &crate::query::ast::CalculationNode) -> bool {
    operand_has_nest(&calc.left) || operand_has_nest(&calc.right)
}

fn operand_has_nest(op: &Operand) -> bool {
    match op {
        Operand::Query(node) => query_node_contains_nest(node.as_ref()),
        Operand::Calculation(calc) => {
            operand_has_nest(&calc.left) || operand_has_nest(&calc.right)
        }
        _ => false,
    }
}

/// QueryNode が Nest を含むか判定（And/Or 透過）
fn query_node_contains_nest(node: &QueryNode) -> bool {
    match node {
        QueryNode::Nest(_) => true,
        QueryNode::And(nodes) | QueryNode::Or(nodes) => {
            nodes.iter().any(|n| query_node_contains_nest(n))
        }
        _ => false,
    }
}

/// Projection(Calculation{Query(Nest(A,agg1)), op, Query(Nest(B,agg2))}) を
/// 両辺のキーをマージした Nest に解決する。
/// キー: left_keys ∪ right_keys、nvalue: left_nv op right_nv
fn resolve_mixed_key_arithmetic(
    left_nest: ResolvedNode,
    right_nest: ResolvedNode,
    op: crate::query::ast::ArithmeticOp,
) -> Result<ResolvedNode> {
    // And([Nest, Filter]) パターンも透過的に処理する (Issue #4)
    let (left_keys, left_nvalue, left_ctx) =
        extract_nvalue_projection_parts(left_nest)?;
    let (right_keys, right_nvalue, right_ctx) =
        extract_nvalue_projection_parts(right_nest)?;
    // 左辺 nvalue と右辺 nvalue の演算結果を nvalue とする
    let calc_operand =
        ResolvedOperand::Calculation(Box::new(ResolvedCalculationNode {
            left: left_nvalue,
            op,
            right: right_nvalue,
        }));
    // 両辺のキーをマージして多段 Nest を構成し、演算結果を nvalue に置く
    let merged_keys = merge_nest_keys(left_keys, right_keys);
    // 両辺のコンテキスト（path フィルタ等）をマージ
    let merged_ctx = match (left_ctx, right_ctx) {
        (None, None) => None,
        (Some(c), None) | (None, Some(c)) => Some(c),
        (Some(l), Some(r)) => Some(Box::new(ResolvedNode::And(vec![*l, *r]))),
    };
    Ok(ResolvedNode::Nest {
        keys: merged_keys,
        nvalue: Some(calc_operand),
        context: merged_ctx,
    })
}

fn resolve_projection_arithmetic(
    lens: &Lens,
    calc: crate::query::ast::CalculationNode,
) -> Result<ResolvedNode> {
    let arith_op = calc.op;
    let (left_key, left_nv, left_ctx) =
        resolve_nest_operand_extract_key(lens, calc.left.clone())?;
    let (right_key, right_nv, right_ctx) =
        resolve_nest_operand_extract_key(lens, calc.right.clone())?;

    validate_calculation_types(&left_nv, &right_nv, arith_op)?;

    let final_left_key = left_key.clone().or_else(|| right_key.clone());
    let final_right_key = right_key.or(left_key);

    match (final_left_key, final_right_key) {
        (Some(lk), Some(rk)) if lk == rk => {
            // 同一キー演算: resolve_nest_operand_extract_key で得たコンテキストをマージ
            let merged_ctx = match (left_ctx, right_ctx) {
                (None, None) => None,
                (Some(c), None) | (None, Some(c)) => Some(c),
                (Some(l), Some(r)) => {
                    Some(Box::new(ResolvedNode::And(vec![*l, *r])))
                }
            };

            Ok(ResolvedNode::Nest {
                keys: vec![lk],
                nvalue: Some(ResolvedOperand::Calculation(Box::new(
                    ResolvedCalculationNode {
                        left: left_nv,
                        op: arith_op,
                        right: right_nv,
                    },
                ))),
                context: merged_ctx,
            })
        }
        (Some(_), Some(_)) => {
            // 異種キー演算: NestNestMatch::Arithmetic を返す代わりに、
            // 左辺 Nest の keys に演算結果を追加して1段深い Nest を返す
            let left_nest = match calc.left {
                Operand::Query(node) => resolve_query_node(lens, *node)?,
                op => resolve_query_node(lens, QueryNode::Projection(op))?,
            };
            let right_nest = match calc.right {
                Operand::Query(node) => resolve_query_node(lens, *node)?,
                op => resolve_query_node(lens, QueryNode::Projection(op))?,
            };
            resolve_mixed_key_arithmetic(left_nest, right_nest, arith_op)
        }
        _ => bail!("Could not extract key from either side of arithmetic Nest"),
    }
}

/// Operand を解決し、GROUP BY キー (operand) と nvalue を返す。
/// Nest(A, agg) → (A_resolved, agg_resolved)
/// Calculation{...} → (common_key, Calculation{resolved_agg, op, resolved_agg})
fn resolve_nest_operand_extract_key(
    lens: &Lens,
    operand: Operand,
) -> Result<(
    Option<ResolvedOperand>,
    ResolvedOperand,
    Option<Box<ResolvedNode>>,
)> {
    match operand {
        Operand::Query(node) => {
            let resolved = resolve_query_node(lens, *node)?;
            // And([Nest, Filter]) パターンも透過的に処理する (Issue #4)
            match extract_nvalue_projection_parts(resolved) {
                Ok((mut keys, nv, ctx)) => Ok((Some(keys.remove(0)), nv, ctx)),
                Err(e) => bail!(
                    "Arithmetic Nest operand must resolve to Nest with nvalue: {}",
                    e
                ),
            }
        }
        Operand::Calculation(calc) => {
            let arith_op = calc.op;
            let (left_key, left_nv, left_ctx) =
                resolve_nest_operand_extract_key(lens, calc.left)?;
            let (right_key, right_nv, right_ctx) =
                resolve_nest_operand_extract_key(lens, calc.right)?;

            let key = left_key.or(right_key);
            let merged_ctx = match (left_ctx, right_ctx) {
                (None, None) => None,
                (Some(c), None) | (None, Some(c)) => Some(c),
                (Some(l), Some(r)) => {
                    Some(Box::new(ResolvedNode::And(vec![*l, *r])))
                }
            };

            validate_calculation_types(&left_nv, &right_nv, arith_op)?;
            let combined = ResolvedOperand::Calculation(Box::new(
                ResolvedCalculationNode {
                    left: left_nv,
                    op: arith_op,
                    right: right_nv,
                },
            ));
            Ok((key, combined, merged_ctx))
        }
        Operand::Literal(l) => Ok((None, ResolvedOperand::Literal(l), None)),
        _ => bail!(
            "Expected Query(Nest), Calculation or Literal in arithmetic Nest"
        ),
    }
}

/// Nest の左辺から ResolvedOperand と、付随するコンテキスト (And などのフィルタ) を抽出するヘルパー。
fn extract_projection_operand_with_context(
    left: ResolvedNode,
) -> Result<(Vec<ResolvedOperand>, Option<Box<ResolvedNode>>)> {
    match left {
        ResolvedNode::Nest { keys, context, .. } => Ok((keys, context)),
        ResolvedNode::And(mut nodes) => {
            let nest_idx = nodes
                .iter()
                .position(|n| matches!(n, ResolvedNode::Nest { .. }));
            if let Some(idx) = nest_idx {
                let nest = nodes.remove(idx);
                let ResolvedNode::Nest { keys, context, .. } = nest else {
                    unreachable!()
                };

                // 残りのノード（is_dir:false などのフィルタ）をコンテキストとして統合
                let filter_ctx = if nodes.is_empty() {
                    None
                } else if nodes.len() == 1 {
                    Some(Box::new(nodes.remove(0)))
                } else {
                    Some(Box::new(ResolvedNode::And(nodes)))
                };

                // context と filter_ctx をマージ
                let merged_ctx = match (context, filter_ctx) {
                    (None, fc) => fc,
                    (pc, None) => pc,
                    (Some(pc), Some(fc)) => {
                        Some(Box::new(ResolvedNode::And(vec![*pc, *fc])))
                    }
                };
                Ok((keys, merged_ctx))
            } else {
                bail!("Nest left side And must contain a Nest node")
            }
        }
        _ => bail!("Nest left side must resolve to Nest or And"),
    }
}

/// nvalue 付き Nest を ResolvedNode から抽出するヘルパー。
///
/// `extension:` などは展開後に `And([TypedTag(is_dir:false), Nest { nvalue }])` に
/// なるため、And の中から Nest を取り出し、残りをコンテキストフィルタとして返す。
///
/// # 戻り値
/// `(operand, nvalue, context)` — context は is_dir:false などのフィルタノード
pub(crate) fn extract_nvalue_projection_parts(
    node: ResolvedNode,
) -> Result<(
    Vec<ResolvedOperand>,
    ResolvedOperand,
    Option<Box<ResolvedNode>>,
)> {
    match node {
        ResolvedNode::Nest {
            keys,
            nvalue: Some(nvalue),
            context,
        } => Ok((keys, nvalue, context.clone())),
        ResolvedNode::Nest { nvalue: None, .. } => {
            // nvalue: None の Nest は build_deduplicated_agg_subquery で処理するため Err を返す
            bail!("Nest has no nvalue")
        }
        ResolvedNode::NestMatch {
            keys,
            nvalue,
            context,
            ..
        } => Ok((keys, nvalue, context)),
        ResolvedNode::MergedNestMatch { keys, matches, .. } => {
            if let Some(first_match) = matches.first() {
                Ok((
                    keys,
                    first_match.nvalue.clone(),
                    first_match.context.clone(),
                ))
            } else {
                bail!("MergedNestMatch is empty")
            }
        }
        ResolvedNode::And(mut nodes) => {
            // nvalue 付き Nest または NestMatch の位置を探す。
            // NestMatch / Nest{nvalue:Some} / MergedNestMatch を優先し、
            // Nest{nvalue:None} はフォールバックとする。
            let proj_idx = nodes
                .iter()
                .position(|n| {
                    matches!(
                        n,
                        ResolvedNode::NestMatch { .. }
                            | ResolvedNode::MergedNestMatch { .. }
                            | ResolvedNode::Nest { nvalue: Some(_), .. }
                    )
                })
                .or_else(|| {
                    nodes
                        .iter()
                        .position(|n| matches!(n, ResolvedNode::Nest { nvalue: None, .. }))
                });
            if let Some(idx) = proj_idx {
                let proj = nodes.remove(idx);
                let (keys, nv, proj_ctx) = match proj {
                    ResolvedNode::Nest {
                        keys,
                        nvalue: Some(nvalue),
                        context,
                    } => (keys, nvalue, context),
                    ResolvedNode::Nest { nvalue: None, .. } => {
                        bail!("Nest has no nvalue")
                    }
                    ResolvedNode::NestMatch {
                        keys,
                        nvalue: nv,
                        context,
                        ..
                    } => (keys, nv, context),
                    ResolvedNode::MergedNestMatch {
                        keys,
                        mut matches,
                        ..
                    } => {
                        let first = matches.remove(0);
                        (keys, first.nvalue, first.context)
                    }
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
                Ok((keys, nv, merged_ctx))
            } else {
                // 直接の子に Nest/NestMatch がない場合、ネストした And を再帰的に探索する。
                // 例: Count(And([And([..., NestMatch{...}]), Match(path)]))
                let nested_idx =
                    nodes.iter().position(|n| matches!(n, ResolvedNode::And(_)));
                if let Some(idx) = nested_idx {
                    let nested = nodes.remove(idx);
                    let (keys, nv, nested_ctx) =
                        extract_nvalue_projection_parts(nested)?;
                    // 残りの要素を追加フィルタとして統合
                    let extra_filter = match nodes.len() {
                        0 => None,
                        1 => Some(Box::new(nodes.remove(0))),
                        _ => Some(Box::new(ResolvedNode::And(nodes))),
                    };
                    let merged_ctx = match (nested_ctx, extra_filter) {
                        (None, f) => f,
                        (c, None) => c,
                        (Some(c), Some(f)) => {
                            Some(Box::new(ResolvedNode::And(vec![*c, *f])))
                        }
                    };
                    Ok((keys, nv, merged_ctx))
                } else {
                    bail!("And node does not contain a nvalue-bearing Nest")
                }
            }
        }
        other => bail!(
            "Both sides of Nest comparison must resolve to nvalue-bearing Nest, got: {:?}",
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
        Operand::Query(node) => {
            // Query(Nest with nvalue) が Calculation 内に現れる場合（expand_nest 後）、
            // Nest の nvalue オペランドを抽出して返す。
            let resolved = resolve_query_node(lens, *node)?;
            let nvalue = resolved.get_nvalue().cloned().or_else(|| {
                // And([filter, Nest{nvalue}]) パターン
                if let ResolvedNode::And(ref nodes) = resolved {
                    nodes.iter().find_map(|n| n.get_nvalue()).cloned()
                } else {
                    None
                }
            });
            nvalue.ok_or_else(|| anyhow::anyhow!(
                "Operand::Query inside Calculation must resolve to Nest with nvalue, got: {:?}",
                resolved
            ))
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
        // (nest_calc) :> 100 → Nest 算術演算子の NestMatch に変換
        (Operand::Calculation(calc), Operand::Literal(lab))
            if calc_has_nest_operands(&calc) =>
        {
            let arithmetic_op = calc.op;
            let resolved = resolve_projection_arithmetic(lens, *calc)?;
            match resolved {
                // 異なるキーの算術演算: NestNestMatch 経由 (old path)
                ResolvedNode::NestNestMatch {
                    left_keys,
                    left_nvalue,
                    left_context,
                    op: _,
                    right_keys: _,
                    right_nvalue,
                    right_context: _,
                } => Ok(ResolvedNode::NestMatch {
                    keys: left_keys, // Arithmetic results usually keep left key
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
                // 同一キーの算術演算: Nest { keys: [k], nvalue: calc } として返る
                ResolvedNode::Nest {
                    keys,
                    nvalue: Some(nv),
                    context,
                } => Ok(ResolvedNode::NestMatch {
                    keys,
                    nvalue: nv,
                    op,
                    label: lab,
                    context,
                }),
                // 多段キー (keys > 1): 比較演算子は未サポート
                ResolvedNode::Nest { ref keys, .. } if keys.len() > 1 => {
                    let tag_name = |op: &ResolvedOperand| match op {
                        ResolvedOperand::TagRef { tag_type, .. } => {
                            format!("{}", tag_type)
                        }
                        other => format!("{:?}", other),
                    };
                    Err(crate::query::error::mismatched_arithmetic_keys(
                        &tag_name(&keys[0]),
                        &tag_name(&keys[1]),
                    ))
                }
                _ => Ok(resolved),
            }
        }
        // (1 + 2) :> size:
        (Operand::Calculation(calc), Operand::Literal(lab)) => {
            crate::query::error::check_label_calc_not_scalar(&calc, &op)?;
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
        // 100 :< (nest_calc) → Nest 算術演算子の NestMatch に変換（flip）
        (Operand::Literal(lab), Operand::Calculation(calc))
            if calc_has_nest_operands(&calc) =>
        {
            let arithmetic_op = calc.op;
            let resolved = resolve_projection_arithmetic(lens, *calc)?;
            match resolved {
                // 異なるキーの算術演算: NestNestMatch 経由 (old path)
                ResolvedNode::NestNestMatch {
                    left_keys,
                    left_nvalue,
                    left_context,
                    op: _,
                    right_keys: _,
                    right_nvalue,
                    right_context: _,
                } => Ok(ResolvedNode::NestMatch {
                    keys: left_keys,
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
                // 同一キーの算術演算: Nest { keys: [k], nvalue: calc } として返る
                ResolvedNode::Nest {
                    keys,
                    nvalue: Some(nv),
                    context,
                } => Ok(ResolvedNode::NestMatch {
                    keys,
                    nvalue: nv,
                    op: flip_op(op),
                    label: lab,
                    context,
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
        // nvalue 付き Nest への比較
        // (parentdir: &: count(ext:jpg)) :> 10 → NestMatch
        (Operand::Query(node), Operand::Literal(lit)) => {
            let resolved = resolve_query_node(lens, *node)?;
            match resolved {
                ResolvedNode::Nest {
                    mut keys,
                    nvalue,
                    context,
                } => {
                    // 比較値は常に Nest の「最後の値」。
                    // nvalue はその最後の値を明示したもの。ない場合は keys の最後が最後の値。
                    let nv = nvalue.unwrap_or_else(|| {
                        keys.pop().expect("Nest must have at least one key")
                    });
                    Ok(ResolvedNode::NestMatch {
                        keys,
                        nvalue: nv,
                        op,
                        label: lit,
                        context,
                    })
                }
                ResolvedNode::And(ref nodes)
                    if nodes.iter().any(|n| n.get_projection().is_some()) =>
                {
                    Ok(resolved)
                }
                _ => bail!(
                    "Query operand in comparison must resolve to Nest, got: {:?}",
                    resolved
                ),
            }
        }
        (Operand::Literal(lit), Operand::Query(node)) => {
            let resolved = resolve_query_node(lens, *node)?;
            match resolved {
                ResolvedNode::Nest {
                    mut keys,
                    nvalue,
                    context,
                } => {
                    let nv = nvalue.unwrap_or_else(|| {
                        keys.pop().expect("Nest must have at least one key")
                    });
                    Ok(ResolvedNode::NestMatch {
                        keys,
                        nvalue: nv,
                        op: flip_op(op),
                        label: lit,
                        context,
                    })
                }
                ResolvedNode::And(ref nodes)
                    if nodes.iter().any(|n| n.get_projection().is_some()) =>
                {
                    Ok(resolved)
                }
                _ => bail!(
                    "Query operand in comparison must resolve to Nest, got: {:?}",
                    resolved
                ),
            }
        }
        // Query vs Query (両辺が Nest、比較演算子)
        // extension: などが And([is_dir:false, Nest { nvalue }]) に展開されるケースも処理する
        (Operand::Query(l_node), Operand::Query(r_node)) => {
            let res_l = resolve_query_node(lens, *l_node)?;
            let res_r = resolve_query_node(lens, *r_node)?;

            let (left_keys, left_nvalue, left_context) =
                extract_nvalue_projection_parts(res_l)?;
            let (right_keys, right_nvalue, right_context) =
                extract_nvalue_projection_parts(res_r)?;

            Ok(ResolvedNode::NestNestMatch {
                left_keys,
                left_nvalue,
                left_context,
                op: NestMatchOp::Comparison(op),
                right_keys,
                right_nvalue,
                right_context,
            })
        }
        // (size: - 100) :> (size: * 0.1)
        (Operand::Calculation(l_calc), Operand::Calculation(r_calc)) => {
            let left_calc = resolve_calculation(lens, *l_calc)?;
            let right_calc = resolve_calculation(lens, *r_calc)?;
            Ok(ResolvedNode::CalculationCalculationMatch {
                left_calc,
                op,
                right_calc,
            })
        }
        // (Query(Nest), >, Calculation(Query(Nest), /, Literal))
        // expand_nest 後に Nest が Calculation 内に Query として現れるパターン
        (Operand::Query(l_node), Operand::Calculation(calc)) => {
            let res_l = resolve_query_node(lens, *l_node)?;
            let (left_keys, left_nvalue, left_context) =
                extract_nvalue_projection_parts(res_l)?;
            let right_calc = resolve_calculation(lens, *calc)?;
            let right_nvalue =
                ResolvedOperand::Calculation(Box::new(right_calc));
            Ok(ResolvedNode::NestNestMatch {
                left_keys: left_keys.clone(),
                left_nvalue,
                left_context: left_context.clone(),
                op: NestMatchOp::Comparison(op),
                right_keys: left_keys,
                right_nvalue,
                right_context: left_context,
            })
        }
        (Operand::Calculation(calc), Operand::Query(r_node)) => {
            let res_r = resolve_query_node(lens, *r_node)?;
            let (right_keys, right_nvalue, right_context) =
                extract_nvalue_projection_parts(res_r)?;
            let left_calc = resolve_calculation(lens, *calc)?;
            let left_nvalue = ResolvedOperand::Calculation(Box::new(left_calc));
            Ok(ResolvedNode::NestNestMatch {
                left_keys: right_keys.clone(),
                left_nvalue,
                left_context: right_context.clone(),
                op: NestMatchOp::Comparison(op),
                right_keys,
                right_nvalue,
                right_context,
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
            StorageMapping::Basic {
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

    /// ラベル集合演算クエリかどうかを返す（And でラップされた内部の LabelSetOp も検出する）
    pub fn is_label_set_op(&self) -> bool {
        self.resolved_query.get_label_set_op().is_some()
    }

    /// ラベル集合演算ノードを返す（And[LabelSetOp, filter] の場合も内部を返す）
    pub fn get_label_set_op_node(&self) -> Option<&ResolvedNode> {
        self.resolved_query.get_label_set_op()
    }

    /// ラベル集合演算が Intersect（`&` 結合）かどうかを返す
    pub fn is_label_set_intersect(&self) -> bool {
        matches!(
            self.resolved_query.get_label_set_op(),
            Some(ResolvedNode::LabelSetOp {
                op: LabelSetOpKind::Intersect,
                ..
            })
        )
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

    /// スカラー集計結果のラベル型を返す。
    /// 集計に使用したラベルが単一種類の場合のみ Some(TagType) を返す。
    pub fn get_scalar_result_label_type(&self) -> Option<TagType> {
        self.resolved_query.get_scalar_result_label_type()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SqlType;
    use crate::query::ast::ArithmeticOp;
    use crate::query::lens_schema::StorageMapping;
    use crate::query::lens_schema::{build_int_condition, build_str_condition};
    use crate::types::{Label, SType};

    #[test]
    fn test_is_pure_scalar() {
        // Literal is pure scalar
        let lit = ResolvedOperand::Literal(Label::from(10));
        assert!(lit.is_pure_scalar());

        // TagRef is NOT pure scalar
        let tag = ResolvedOperand::TagRef {
            tag_type: TagType::Base(SType::Size),
            storage: StorageMapping::Fixed(crate::db::Col::LabelInt),
            sql_type: SqlType::BIGINT,
        };
        assert!(!tag.is_pure_scalar());

        // Aggregation is pure scalar
        let agg = ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(
            Box::new(ResolvedNode::And(vec![])),
        ));
        assert!(agg.is_pure_scalar());

        // Calculation of pure scalars is pure scalar
        let calc_pure =
            ResolvedOperand::Calculation(Box::new(ResolvedCalculationNode {
                left: lit.clone(),
                op: ArithmeticOp::Add,
                right: agg.clone(),
            }));
        assert!(calc_pure.is_pure_scalar());

        // Calculation with TagRef is NOT pure scalar
        let calc_not_pure =
            ResolvedOperand::Calculation(Box::new(ResolvedCalculationNode {
                left: tag.clone(),
                op: ArithmeticOp::Add,
                right: lit.clone(),
            }));
        assert!(!calc_not_pure.is_pure_scalar());
    }

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
    fn test_extract_agg_parts_logic() {
        use crate::db::Col;
        use crate::query::lens_schema::StorageMapping;
        use crate::types::{Label, SType, TagType};

        // Prepare nodes
        // Case 1: And(Nest, Filter)
        let projection = ResolvedNode::Nest {
            keys: vec![ResolvedOperand::TagRef {
                tag_type: TagType::Base(SType::Size),
                storage: StorageMapping::Fixed(Col::Size),
                sql_type: SqlType::BIGINT,
            }],
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
            &StorageMapping::Fixed(Col::Size),
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
                // size: は Basic（EAV）として保存されている
                assert!(matches!(storage, StorageMapping::Basic { .. }));
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
        let node = ResolvedNode::Nest {
            keys: vec![operand.clone()],
            nvalue: None,
            context: None,
        };

        // 修正後の期待値: (Some(storage), None, Some(operand))
        let (storage, filter, res_op) = node.extract_agg_parts();
        assert!(storage.is_some());
        assert!(matches!(storage.unwrap(), StorageMapping::Basic { .. }));
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
        // parentdir: &: count(extension:jpg) → Nest { nvalue: Some(Count) }
        let resolver = Resolver::new("parentdir: &: count(extension:jpg)")
            .expect("nest with agg should resolve");
        match &resolver.resolved_query {
            ResolvedNode::Nest { nvalue, keys, .. } => {
                assert!(nvalue.is_some(), "nvalue should be present");
                match nvalue.as_ref().unwrap() {
                    ResolvedOperand::Aggregation(
                        ResolvedAggregationNode::Count(_),
                    ) => {}
                    other => panic!("Expected Count nvalue, got {:?}", other),
                }
                // operand は parentdir の TagRef
                match keys.first().unwrap() {
                    ResolvedOperand::TagRef { tag_type, .. } => {
                        assert_eq!(tag_type.as_str(), "parentdir");
                    }
                    _ => panic!(
                        "Expected TagRef operand, got {:?}",
                        keys.first().unwrap()
                    ),
                }
            }
            other => panic!("Expected Nest with nvalue, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_nest_sum_nvalue() {
        // project: &: sum(size:) → Nest { nvalue: Some(Sum) }
        let resolver = Resolver::new("project: &: sum(size:)")
            .expect("nest with sum should resolve");
        match &resolver.resolved_query {
            ResolvedNode::Nest { nvalue, keys, .. } => {
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
                match keys.first().unwrap() {
                    ResolvedOperand::TagRef { tag_type, .. } => {
                        assert_eq!(tag_type.as_str(), "project");
                    }
                    _ => panic!("Expected TagRef operand"),
                }
            }
            other => panic!("Expected Nest with nvalue, got {:?}", other),
        }
    }

    #[test]
    fn test_projection_no_regression() {
        // size: → Nest { nvalue: None }
        let resolver =
            Resolver::new("size:").expect("simple projection should resolve");
        match &resolver.resolved_query {
            ResolvedNode::Nest { nvalue, .. } => {
                assert!(
                    nvalue.is_none(),
                    "plain projection should have no nvalue"
                );
            }
            other => panic!("Expected Nest, got {:?}", other),
        }
    }

    /// `extension:` のように展開後 And([filter, Nest]) になるケースでも
    /// get_nvalue() が再帰的に nvalue を取得できることを確認
    #[test]
    fn test_get_nvalue_through_and() {
        // extension: は And([is_dir:false, Nest(extension)]) に展開される
        let resolver = Resolver::new("extension: &: count(name:test)").unwrap();

        // get_projection は And 内の Projection を見つける
        assert!(
            resolver.get_projection().is_some(),
            "Should find projection inside And"
        );

        // get_nvalue も And 内の Nest の nvalue を見つける
        assert!(
            resolver.get_nvalue().is_some(),
            "Should find nvalue inside And (extension: expands to And)"
        );
    }

    /// And でラップされていない純粋な Nest の nvalue も取得できる
    #[test]
    fn test_get_nvalue_direct_projection() {
        // parentdir は展開後も Nest のまま (And でラップされない)
        let resolver =
            Resolver::new("parentdir: &: count(extension:jpg)").unwrap();

        assert!(resolver.get_projection().is_some());
        assert!(
            resolver.get_nvalue().is_some(),
            "Should find nvalue on direct Nest"
        );
    }

    /// 右辺 Comparison (agg vs literal) → logical_resolver で分配され
    /// (parentdir: &: count(ext:jpg)) :> 1 のラベル比較に変換される。
    /// nvalue 付き Nest として解決される（nvalue でフィルタ済み）。
    #[test]
    fn test_resolve_nest_comparison_distributed() {
        let resolver =
            Resolver::new("parentdir: &: (count(extension:jpg) > 1)")
                .expect("nest comparison should resolve");

        // Nest として解決される（nvalue 付き比較は Projection を返す）
        assert!(resolver.get_projection().is_some(), "Should return Nest");
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

        // 解析結果が NestNestMatch または MergedNestMatch であることを確認
        assert!(
            matches!(
                resolver.resolved_query,
                ResolvedNode::NestNestMatch { .. }
                    | ResolvedNode::MergedNestMatch { .. }
            ),
            "Expected NestNestMatch or MergedNestMatch, got: {:?}",
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
        // TypeRef を含む Calculation は純粋スカラーではないため、
        // nvalue ではなく複合キーの一つとして Nest を深化させる。
        // QUERY.md Level n 例: `project: &: (extension: + 1)`
        let resolver = Resolver::new("parentdir: &: (size: * 2)")
            .expect("nest with calculation right should resolve");

        assert!(resolver.get_projection().is_some());
        // size: * 2 は TypeRef を含むため key として扱われる → nvalue は None
        assert!(
            resolver.get_nvalue().is_none(),
            "TypeRef calculation should be a key (Level n), not nvalue, got: {:?}",
            resolver.get_nvalue()
        );
        // Nest { keys: [parentdir, Calculation(size*2)], nvalue: None } になる
        let operands = resolver.resolved_query.get_projection_operands();
        assert!(
            matches!(operands, Some(s) if s.len() >= 2),
            "size: * 2 should be added as a key (depth 2+), got operands: {:?}",
            operands
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
            // nodes[1] は Nest (parentdir: &: count(ext:jpg))
            let filter = &nodes[0];
            let proj = &nodes[1];

            if let ResolvedNode::Nest { context, .. } = proj {
                assert!(context.is_some(), "Context should be injected");
                assert_eq!(
                    context.as_deref().unwrap(),
                    filter,
                    "Context should be the adjacent filter"
                );
            } else {
                panic!("Expected Nest node at index 1, got {:?}", proj);
            }
        } else {
            panic!("Expected And node, got {:?}", result.resolved_query);
        }
    }

    #[test]
    fn test_resolve_query_vs_query_comparison() {
        // クエリ: (parentdir: &: count(ext:jpg)) == (parentdir: &: count(ext:png))
        let query = "(parentdir: &: count(extension:jpg)) == (parentdir: &: count(extension:png))";
        // 現在は解決ロジックを実装済みなので、Ok(NestNestMatch) が返るはず
        let result = Resolver::new(query)
            .expect("Should resolve Query vs Query comparison");
        assert!(
            matches!(
                result.resolved_query,
                ResolvedNode::NestNestMatch { .. }
                    | ResolvedNode::MergedNestMatch { .. }
            ),
            "Expected NestNestMatch or MergedNestMatch, got: {:?}",
            result.resolved_query
        );
    }

    #[test]
    fn test_resolve_nest_depth1_produces_nest_variant() {
        let query = "parentdir: &: count()";
        let result = Resolver::new(query).expect("Should resolve query");
        match result.resolved_query {
            ResolvedNode::Nest { keys, nvalue, .. } => {
                assert_eq!(keys.len(), 1);
                assert!(nvalue.is_some());
            }
            _ => {
                panic!("Expected Nest variant, got {:?}", result.resolved_query)
            }
        }
    }

    #[test]
    fn test_resolve_projection_produces_nest_variant() {
        let query = "extension:";
        let result = Resolver::new(query).expect("Should resolve query");
        match result.resolved_query {
            ResolvedNode::And(ref nodes) => {
                let has_nest = nodes.iter().any(|n| {
                    matches!(n, ResolvedNode::Nest { nvalue: None, .. })
                });
                assert!(has_nest, "Expected Nest variant with nvalue: None in And node. Got: {:?}", nodes);
            }
            ResolvedNode::Nest { nvalue, .. } => {
                assert!(nvalue.is_none());
            }
            _ => panic!(
                "Expected Nest variant (or And containing Nest), got {:?}",
                result.resolved_query
            ),
        }
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

        // 最適化が適用されていれば、ルートは MergedNestMatch になっているはず
        assert!(
            matches!(
                resolver.resolved_query,
                crate::query::ResolvedNode::MergedNestMatch { .. }
            ),
            "Resolved query should be optimized (merged). Got: {:?}",
            resolver.resolved_query
        );
    }

    #[test]
    fn test_resolve_nest_depth2_produces_nest_with_two_keys() {
        let query = "parentdir: &: filename:";
        let resolver = Resolver::new(query).unwrap();
        assert!(
            matches!(
                resolver.resolved_query,
                crate::query::ResolvedNode::Nest { ref keys, .. }
                    if keys.len() == 2
            ),
            "Depth 2 nest should resolve to Nest with 2 keys, got: {:?}",
            resolver.resolved_query
        );
    }

    #[test]
    fn test_resolve_nest_left_is_nest() {
        let query = "(parentdir: &: filename:) &: extension:";
        let resolver = Resolver::new(query).unwrap();
        assert!(
            matches!(
                resolver.resolved_query,
                crate::query::ResolvedNode::Nest { ref keys, .. }
                    if keys.len() >= 2
            ),
            "Left-is-Nest should produce Nest with 2+ keys, got: {:?}",
            resolver.resolved_query
        );
    }

    #[test]
    fn test_mixed_key_arithmetic_returns_deeper_nest() {
        let query = "(parentdir: &: count()) + (extension: &: count())";
        let resolver = Resolver::new(query).unwrap();
        // extension: expands to And([is_dir:false, Nest(extension)]), so the result
        // may be And([filter, Nest { keys: [parentdir, extension], ... }]).
        // Find the Nest node inside And if necessary.
        let inner_nest = match &resolver.resolved_query {
            crate::query::ResolvedNode::And(nodes) => nodes
                .iter()
                .find(|n| matches!(n, crate::query::ResolvedNode::Nest { .. })),
            n @ crate::query::ResolvedNode::Nest { .. } => Some(n),
            _ => None,
        };
        assert!(
            matches!(inner_nest, Some(crate::query::ResolvedNode::Nest { keys, .. }) if keys.len() >= 2),
            "Mixed-key arithmetic should produce a deeper Nest with >=2 keys, got: {:?}",
            resolver.resolved_query
        );
    }

    // ── LabelSetOp (Phase 2) ─────────────────────────────────────────────────

    // ── LabelSetOp メソッド挙動 ───────────────────────────────────────────────

    fn make_nest(tag: &str) -> ResolvedNode {
        make_nest_with_keys(tag, 1)
    }

    fn make_nest_with_keys(root_tag: &str, key_count: usize) -> ResolvedNode {
        use crate::query::lens_schema::StorageMapping;
        let make_key = |t: &str| ResolvedOperand::TagRef {
            tag_type: crate::types::TagType::from(t),
            storage: StorageMapping::Basic {
                column: crate::db::Col::LabelStr,
                tag_type: t.to_string(),
            },
            sql_type: crate::db::SqlType::VARCHAR,
        };
        let keys = (0..key_count)
            .map(|i| {
                if i == 0 {
                    make_key(root_tag)
                } else {
                    make_key(&format!("key{}", i))
                }
            })
            .collect();
        ResolvedNode::Nest {
            keys,
            nvalue: None,
            context: None,
        }
    }

    #[test]
    fn test_label_set_op_is_projection_recursive() {
        let node = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![make_nest("cat"), make_nest("flavor")],
        };
        assert!(
            node.is_projection_recursive(),
            "LabelSetOp should be projection_recursive"
        );
    }

    #[test]
    fn test_label_set_op_get_projection_returns_first_operand_for_intersect() {
        // Intersect: 常に先頭オペランドの投影型を返す (仕様: & 結果は必ず Projection)
        // 異なるルートタグ (1-key×2)
        let node_diff_root = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![make_nest("cat"), make_nest("flavor")],
        };
        assert!(
            node_diff_root.get_projection().is_some(),
            "LabelSetOp{{Intersect}} with different root tags should return Some"
        );
        // 同一ルートタグ (1-key×2: name: & name: 相当)
        let node_same_1key = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![make_nest("tagA"), make_nest("tagA")],
        };
        assert!(
            node_same_1key.get_projection().is_some(),
            "LabelSetOp{{Intersect}} same root 1-key should return Some"
        );
        // 同一ルートタグ (2-key×2)
        let node_same_2key = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![
                make_nest_with_keys("tagA", 2),
                make_nest_with_keys("tagA", 2),
            ],
        };
        assert!(
            node_same_2key.get_projection().is_some(),
            "LabelSetOp{{Intersect}} same root 2-key should return Some"
        );
        // 同一ルートタグ (2-key + 3-key)
        let node_deep = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![
                make_nest_with_keys("tagA", 2),
                make_nest_with_keys("tagA", 3),
            ],
        };
        assert!(
            node_deep.get_projection().is_some(),
            "LabelSetOp{{Intersect}} 2+3 key should return Some"
        );
    }

    #[test]
    fn test_label_set_op_union_get_projection() {
        // Lv.2 | Lv.2 (異なるキー) → Lv.2 混合 Projection → Some
        let lv2_union = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Union,
            operands: vec![make_nest("cat"), make_nest("flavor")],
        };
        assert!(
            lv2_union.get_projection().is_some(),
            "Lv.2 | Lv.2 (different keys): should return Some (mixed Projection)"
        );

        // Lv.3 | Lv.3 (異なるキー、共通プレフィックスなし) → Lv.1 フラット → None
        let lv3_union = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Union,
            operands: vec![
                make_nest_with_keys("cat", 2),
                make_nest_with_keys("shape", 2),
            ],
        };
        assert!(
            lv3_union.get_projection().is_none(),
            "Lv.3 | Lv.3 (different keys, no common prefix): should return None (flat)"
        );
    }

    #[test]
    fn test_label_set_op_inject_context_propagates_to_all_operands() {
        use crate::query::lens_schema::StorageMapping;

        let filter = ResolvedNode::Match {
            tag_type: crate::types::TagType::from("path"),
            storage: StorageMapping::Basic {
                column: crate::db::Col::LabelStr,
                tag_type: "path".to_string(),
            },
            sql_type: crate::db::SqlType::VARCHAR,
            op: crate::query::ast::ComparisonOp::Label(
                crate::query::ast::BasicOp::Eq,
            ),
            label: crate::types::Label::resolve(
                crate::types::TagType::from("path"),
                crate::types::LabelValue::String("/foo/*".to_string()),
            ),
        };

        let mut node = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![make_nest("cat"), make_nest("flavor")],
        };
        node.inject_context(filter);

        let ResolvedNode::LabelSetOp { operands, .. } = &node else {
            panic!("Expected LabelSetOp");
        };
        for (i, op) in operands.iter().enumerate() {
            assert!(
                matches!(op, ResolvedNode::Nest { context: Some(_), .. }),
                "operand[{}] should have context after inject_context, got: {:?}", i, op
            );
        }
    }

    // ── ヘルパー ──────────────────────────────────────────────────────────────

    /// Nest の最初のキーのタグ型文字列を取得する
    fn first_key_tag(node: &ResolvedNode) -> Option<String> {
        if let ResolvedNode::Nest { keys, .. } = node {
            if let Some(ResolvedOperand::TagRef { tag_type, .. }) = keys.first()
            {
                return Some(tag_type.to_string());
            }
        }
        None
    }

    // ── LabelSetOp ────────────────────────────────────────────────────────────

    #[test]
    fn test_and_two_projections_produces_label_set_op_intersect() {
        // And([Proj(cat), Proj(flavor)]) → LabelSetOp { Intersect, [Nest{cat}, Nest{flavor}] }
        let resolver = Resolver::new("cat: & flavor:").unwrap();
        let ResolvedNode::LabelSetOp { op, operands } =
            &resolver.resolved_query
        else {
            panic!("Expected LabelSetOp, got: {:?}", resolver.resolved_query);
        };
        assert_eq!(*op, LabelSetOpKind::Intersect);
        assert_eq!(operands.len(), 2);
        // 各オペランドが単一キーの Nest であることを確認
        let tags: Vec<_> = operands.iter().filter_map(first_key_tag).collect();
        assert!(
            tags.contains(&"cat".to_string()),
            "operands: {:?}",
            operands
        );
        assert!(
            tags.contains(&"flavor".to_string()),
            "operands: {:?}",
            operands
        );
        assert!(resolver.is_label_set_op());
    }

    #[test]
    fn test_and_proj_nest_produces_label_set_op_intersect() {
        // And([Proj(tagA), Nest{tagA,tagB}]) → LabelSetOp { Intersect, 2 operands }
        let resolver = Resolver::new("tagA: & (tagA: &: tagB:)").unwrap();
        let ResolvedNode::LabelSetOp { op, operands } =
            &resolver.resolved_query
        else {
            panic!("Expected LabelSetOp, got: {:?}", resolver.resolved_query);
        };
        assert_eq!(*op, LabelSetOpKind::Intersect);
        assert_eq!(operands.len(), 2);
        // 2番目のオペランドは 2 キー Nest（tagA &: tagB）
        let second = &operands[1];
        assert!(
            matches!(second, ResolvedNode::Nest { keys, .. } if keys.len() == 2),
            "second operand should be 2-key Nest, got: {:?}",
            second
        );
    }

    #[test]
    fn test_and_nest_nest_produces_label_set_op_intersect() {
        // And([Nest{tagA,tagB}, Nest{tagA,tagC}]) → LabelSetOp { Intersect, 2 operands }
        let resolver =
            Resolver::new("(tagA: &: tagB:) & (tagA: &: tagC:)").unwrap();
        let ResolvedNode::LabelSetOp { op, operands } =
            &resolver.resolved_query
        else {
            panic!("Expected LabelSetOp, got: {:?}", resolver.resolved_query);
        };
        assert_eq!(*op, LabelSetOpKind::Intersect);
        assert_eq!(operands.len(), 2);
        // 両オペランドとも 2 キー Nest
        for (i, op_node) in operands.iter().enumerate() {
            assert!(
                matches!(op_node, ResolvedNode::Nest { keys, .. } if keys.len() == 2),
                "operand[{}] should be 2-key Nest, got: {:?}",
                i,
                op_node
            );
        }
    }

    #[test]
    fn test_and_proj_proj_with_filter_context_injected() {
        // (cat: & flavor:) & path:foo/* — フィルタが各オペランドのコンテキストに注入される
        let resolver = Resolver::new("cat: & flavor: & path:foo/*").unwrap();
        let ResolvedNode::LabelSetOp { op, operands } =
            &resolver.resolved_query
        else {
            panic!("Expected LabelSetOp, got: {:?}", resolver.resolved_query);
        };
        assert_eq!(*op, LabelSetOpKind::Intersect);
        assert_eq!(operands.len(), 2);
        // 各オペランドにパスフィルタが context として注入されていることを確認
        for (i, op_node) in operands.iter().enumerate() {
            assert!(
                matches!(
                    op_node,
                    ResolvedNode::Nest {
                        context: Some(_),
                        ..
                    }
                ),
                "operand[{}] should have context injected, got: {:?}",
                i,
                op_node
            );
        }
    }

    #[test]
    fn test_and_expanded_proj_and_proj_produces_label_set_op() {
        // extension: は And([is_dir:false, Nest{ext}]) に展開される。
        // extension: & size: → And([And([is_dir:false, Nest{ext}]), Nest{size}])
        // as_label_set_op_operand() で And ラッパーを透過して LabelSetOp になるべき。
        let resolver = Resolver::new("extension: & size:").unwrap();
        assert!(
            matches!(
                &resolver.resolved_query,
                ResolvedNode::LabelSetOp {
                    op: LabelSetOpKind::Intersect,
                    ..
                }
            ),
            "extension: & size: should resolve to LabelSetOp{{Intersect}}, got: {:?}",
            resolver.resolved_query
        );
        assert!(resolver.is_label_set_op());
    }

    #[test]
    fn test_and_nest2_nest3_produces_label_set_op_intersect() {
        // Nest{2keys} & Nest{3keys} → LabelSetOp { Intersect, 2 operands }
        // (tagA: &: tagB:) & (tagA: &: tagC: &: tagD:)
        let resolver =
            Resolver::new("(tagA: &: tagB:) & (tagA: &: tagC: &: tagD:)")
                .unwrap();
        let ResolvedNode::LabelSetOp { op, operands } =
            &resolver.resolved_query
        else {
            panic!("Expected LabelSetOp, got: {:?}", resolver.resolved_query);
        };
        assert_eq!(*op, LabelSetOpKind::Intersect);
        assert_eq!(operands.len(), 2);
        // 第1オペランド: 2キー Nest
        assert!(
            matches!(&operands[0], ResolvedNode::Nest { keys, .. } if keys.len() == 2),
            "first operand should be 2-key Nest, got: {:?}",
            operands[0]
        );
        // 第2オペランド: 3キー Nest
        assert!(
            matches!(&operands[1], ResolvedNode::Nest { keys, .. } if keys.len() == 3),
            "second operand should be 3-key Nest, got: {:?}",
            operands[1]
        );
    }

    #[test]
    fn test_and_proj_typedtag_does_not_produce_label_set_op() {
        // Proj & TypedTag → LabelSetOp にはならず Nest にコンテキスト注入される
        // 単一 Projection の場合は And ラッパーが剥がれ Nest{ctx:TypedTag} として返る
        let resolver = Resolver::new("cat: & animal:dog").unwrap();
        assert!(
            !resolver.is_label_set_op(),
            "cat: & animal:dog should NOT be LabelSetOp, got: {:?}",
            resolver.resolved_query
        );
        // Nest に TypedTag フィルタが context として注入されていることを確認
        let has_context = match &resolver.resolved_query {
            ResolvedNode::Nest {
                context: Some(_), ..
            } => true,
            ResolvedNode::And(nodes) => nodes.iter().any(|n| {
                matches!(
                    n,
                    ResolvedNode::Nest {
                        context: Some(_),
                        ..
                    }
                )
            }),
            _ => false,
        };
        assert!(
            has_context,
            "cat: should have context injected, got: {:?}",
            resolver.resolved_query
        );
    }
}

#[cfg(test)]
mod tests_walk_fold {
    use super::*;
    use crate::query::ast::ArithmeticOp;
    use crate::types::{Label, LabelValue, SType, TagType};

    fn leaf(name: &str) -> ResolvedNode {
        ResolvedNode::ColumnMatch {
            tag: SType::Name,
            label: Label::resolve(
                TagType::from(name),
                LabelValue::String(name.to_string()),
            ),
        }
    }

    fn lit(n: i64) -> ResolvedOperand {
        ResolvedOperand::Literal(Label::from(n))
    }

    fn calc(left: ResolvedOperand, right: ResolvedOperand) -> ResolvedOperand {
        ResolvedOperand::Calculation(Box::new(ResolvedCalculationNode {
            left,
            op: ArithmeticOp::Add,
            right,
        }))
    }

    // ── ResolvedNode::walk ────────────────────────────────────────────────

    #[test]
    fn test_node_walk_leaf() {
        let n = leaf("a");
        let got = n.walk();
        assert_eq!(got.len(), 1);
        assert!(std::ptr::eq(got[0], &n));
    }

    #[test]
    fn test_node_walk_and() {
        let a = leaf("a");
        let b = leaf("b");
        let root = ResolvedNode::And(vec![a, b]);
        // 前順: root, child0, child1
        let got = root.walk();
        assert_eq!(got.len(), 3);
        assert!(matches!(got[0], ResolvedNode::And(_)));
        assert!(matches!(got[1], ResolvedNode::ColumnMatch { .. }));
        assert!(matches!(got[2], ResolvedNode::ColumnMatch { .. }));
    }

    #[test]
    fn test_node_walk_nested() {
        // And([Or([a, b]), c])  → And + Or + a + b + c = 5 nodes
        let inner = ResolvedNode::Or(vec![leaf("a"), leaf("b")]);
        let root = ResolvedNode::And(vec![inner, leaf("c")]);
        assert_eq!(root.walk().len(), 5);
    }

    // ── ResolvedNode::fold ────────────────────────────────────────────────

    #[test]
    fn test_node_fold_count() {
        // And([a, Or([b, c])]) = 5 nodes
        let root = ResolvedNode::And(vec![
            leaf("a"),
            ResolvedNode::Or(vec![leaf("b"), leaf("c")]),
        ]);
        let count = root.fold(&|_node, child_counts: Vec<usize>| {
            1 + child_counts.into_iter().sum::<usize>()
        });
        assert_eq!(count, 5);
    }

    #[test]
    fn test_node_fold_depth() {
        let root = ResolvedNode::And(vec![
            leaf("a"),
            ResolvedNode::Or(vec![leaf("b"), leaf("c")]),
        ]);
        // depth = max child depth + 1
        let depth = root.fold(&|_node, child_depths: Vec<usize>| {
            1 + child_depths.into_iter().max().unwrap_or(0)
        });
        // And depth=3: Or depth=2: leaves depth=1
        assert_eq!(depth, 3);
    }

    #[test]
    fn test_node_fold_postorder() {
        // fold は後順（子が先に処理される）ことを確認
        use std::cell::RefCell;
        let order: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let root = ResolvedNode::And(vec![leaf("a"), leaf("b")]);
        root.fold(&|node, _: Vec<()>| match node {
            ResolvedNode::And(_) => order.borrow_mut().push("And".to_string()),
            ResolvedNode::ColumnMatch { label, .. } => {
                order.borrow_mut().push(label.as_str().to_string())
            }
            _ => {}
        });
        // 後順なので "a", "b", "And" の順
        assert_eq!(*order.borrow(), vec!["a", "b", "And"]);
    }

    // ── ResolvedOperand::walk ─────────────────────────────────────────────

    #[test]
    fn test_operand_walk_leaf() {
        let op = lit(1);
        let got = op.walk();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn test_operand_walk_calc() {
        // (1 + 2)  → 3 nodes
        let op = calc(lit(1), lit(2));
        assert_eq!(op.walk().len(), 3);
    }

    #[test]
    fn test_operand_walk_nested_calc() {
        // ((1 + 2) + 3)  → 5 nodes
        let op = calc(calc(lit(1), lit(2)), lit(3));
        assert_eq!(op.walk().len(), 5);
    }

    // ── ResolvedOperand::fold ─────────────────────────────────────────────

    #[test]
    fn test_operand_fold_sum_literals() {
        // fold で Literal の合計を計算
        let op = calc(calc(lit(1), lit(2)), lit(3));
        let sum = op.fold(&|node, child_sums: Vec<i64>| match node {
            ResolvedOperand::Literal(label) => label.as_i64(),
            _ => child_sums.into_iter().sum(),
        });
        assert_eq!(sum, 6);
    }

    #[test]
    fn test_operand_fold_count_leaves() {
        let op = calc(calc(lit(1), lit(2)), lit(3));
        let count = op.fold(&|node, child_counts: Vec<usize>| match node {
            ResolvedOperand::Calculation(_) => child_counts.into_iter().sum(),
            _ => 1,
        });
        assert_eq!(count, 3);
    }

    #[test]
    fn test_operand_fold_postorder() {
        use std::cell::RefCell;
        let order: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let op = calc(lit(1), lit(2));
        op.fold(&|node, _: Vec<()>| match node {
            ResolvedOperand::Literal(label) => {
                order.borrow_mut().push(label.as_i64().to_string())
            }
            ResolvedOperand::Calculation(_) => {
                order.borrow_mut().push("calc".to_string())
            }
            _ => {}
        });
        assert_eq!(*order.borrow(), vec!["1", "2", "calc"]);
    }

    // ─── get_scalar_result_label_type ───────────────────────────────────────

    #[test]
    fn test_get_scalar_result_label_type_size() {
        use crate::types::{SType, TagType};
        let r = Resolver::new("sum(size:)").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            Some(TagType::Base(SType::Size)),
            "sum(size:) should yield Some(Size)"
        );
        let r = Resolver::new("avg(size:)").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            Some(TagType::Base(SType::Size)),
            "avg(size:) should yield Some(Size)"
        );
        let r = Resolver::new("min(size:)").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            Some(TagType::Base(SType::Size)),
            "min(size:) should yield Some(Size)"
        );
    }

    #[test]
    fn test_get_scalar_result_label_type_mtime() {
        use crate::types::{SType, TagType};
        let r = Resolver::new("max(mtime:)").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            Some(TagType::Base(SType::Mtime)),
            "max(mtime:) should yield Some(Mtime)"
        );
    }

    #[test]
    fn test_get_scalar_result_label_type_count_is_none() {
        let r = Resolver::new("count(extension:rs)").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            None,
            "count() should yield None (no type propagation)"
        );
        let r = Resolver::new("count()").unwrap();
        assert_eq!(r.get_scalar_result_label_type(), None);
    }

    #[test]
    fn test_get_scalar_result_label_type_mixed_is_none() {
        // sum(size: + mtime:) — two different tag types → None
        // Note: arithmetic inside sum() does not require extra parens
        let r = Resolver::new("sum(size: + mtime:)").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            None,
            "sum(size: + mtime:) should yield None (2 types)"
        );
    }

    #[test]
    fn test_get_scalar_result_label_type_calc_with_literal() {
        use crate::types::{SType, TagType};
        // sum(size: - 1000) — Calculation(TagRef{size}, Literal) → size only
        let r = Resolver::new("sum(size: - 1000)").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            Some(TagType::Base(SType::Size)),
            "sum(size: - 1000) should yield Some(Size)"
        );
    }

    #[test]
    fn test_get_scalar_result_label_type_outer_calc() {
        use crate::types::{SType, TagType};
        // sum(size:) + count() — Calculation of Agg{size} + Agg{Count} → size only
        let r = Resolver::new("sum(size:) + count()").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            Some(TagType::Base(SType::Size)),
            "sum(size:) + count() should yield Some(Size)"
        );
        // sum(size:) + sum(mtime:) → None (2 types)
        let r = Resolver::new("sum(size:) + sum(mtime:)").unwrap();
        assert_eq!(
            r.get_scalar_result_label_type(),
            None,
            "sum(size:) + sum(mtime:) should yield None"
        );
    }
}
