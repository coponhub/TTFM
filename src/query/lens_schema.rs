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

use crate::db::Col;
use crate::query::ast::{
    BasicOp, Candidate, ComparisonNode, ComparisonOp, Operand, QueryNode,
};
use crate::query::logical_schema::{LogicalSchema, LogicalType};
use crate::query::sql::schema_pieces;
use crate::tag::{LogicalRole, TagFunction};
use crate::types::{
    Bitical, ItemId, ItemKind, Label, LabelNode, Origin, Rank, SType, TagType,
};
use duckdb::types::Value;
use indexmap::IndexMap;
use sea_query::{BinOper, Condition, Expr, ExprTrait, Func, SimpleExpr};
use std::sync::Arc;

pub(crate) fn origin_matches(id: &ItemId, target: Origin) -> bool {
    match id {
        ItemId::Stored(val) => Origin::within(*val) == target,
        ItemId::Settling(org, _) => *org == target,
        ItemId::Volatile(_) => target == Origin::User,
    }
}

pub(crate) fn definition_candidates(
    entries: &[(TagType, Rank, ItemId)],
    kind: ItemKind,
    origins: &[Origin],
) -> Vec<Candidate> {
    // registry は型のソースなので、tag 定義（`tag:"X"`）の候補にはならない。
    // Stored 定義行と使用中ペアだけが tag 定義のソース。
    if kind != ItemKind::Type {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|(_, _, id)| {
            origins.is_empty()
                || origins.iter().any(|target| origin_matches(id, *target))
        })
        .map(|(t, rank, id)| Candidate {
            name: t.as_str().to_string(),
            rank: *rank,
            id: *id,
        })
        .collect()
}

pub(crate) fn definition_reserved_names(
    entries: &[(TagType, Rank, ItemId)],
) -> Vec<String> {
    entries
        .iter()
        .map(|(t, _, _)| t.as_str().to_string())
        .collect()
}

/// タグの物理的な格納場所
#[derive(Debug, PartialEq, Clone)]
pub enum StorageMapping {
    /// oneview の専用カラム（Fixed タグ）
    Fixed(Col),
    /// oneview の汎用ラベルカラム＋タグ名（Basic タグ）
    Basic { column: Col, tag_type: String },
    /// 他のタグに展開される論理タグ（Composite タグ）
    Composite,
}

impl StorageMapping {
    /// このストレージマッピングに基づき、ラベル値を SELECT する SQL を生成します。
    /// Composite の場合は None を返します。
    pub fn to_label_select(
        &self,
        src: &crate::db::Src,
        ids_sql: sea_query::SelectStatement,
    ) -> Option<sea_query::SelectStatement> {
        use crate::query::sql::schema_pieces::{
            build_lens_select_column, build_lens_select_tag,
        };
        match self {
            StorageMapping::Fixed(col) => {
                let base = build_lens_select_column(src, *col, ids_sql);
                Some(crate::query::lens_builder::complement_type(base, *col))
            }
            StorageMapping::Basic { column, tag_type } => {
                Some(build_lens_select_tag(src, *column, tag_type, ids_sql))
            }
            StorageMapping::Composite => None,
        }
    }

    /// このストレージマッピングに基づき、指定された演算子とラベルに対する SQL 条件を生成します。
    pub fn to_condition(
        &self,
        op: ComparisonOp,
        label: &Label,
        bitical_type: crate::db::BiticalType,
    ) -> Condition {
        match self {
            StorageMapping::Fixed(col) => {
                build_column_condition(*col, op, label, bitical_type, false)
            }
            StorageMapping::Basic { column, tag_type } => Condition::all()
                .add(check_tag_match(tag_type))
                .add(build_column_condition(
                    *column,
                    op,
                    label,
                    bitical_type,
                    true,
                )),
            StorageMapping::Composite => Condition::any(),
        }
    }
}

pub(crate) fn check_tag_match(tag_type: &str) -> SimpleExpr {
    let tag_op = if crate::util::is_glob_pattern(tag_type) {
        BinOper::Custom("GLOB")
    } else {
        BinOper::Equal
    };
    schema_pieces::type_filter(tag_op, tag_type)
}

fn coerce_plain_bitical(label: &Label, logical_type: LogicalType) -> Label {
    let LabelNode::Formatted(crate::query::format::Formatted::Bitical(
        resolved,
    )) = label.node()
    else {
        return label.clone();
    };
    let matches_type = matches!(
        (logical_type, resolved),
        (LogicalType::Any, _)
            | (LogicalType::Boolean, Bitical::Boolean(_))
            | (LogicalType::Integer, Bitical::Integer(_))
            | (LogicalType::Float, Bitical::Integer(_) | Bitical::Double(_))
    );
    if matches_type {
        Label::other(resolved.clone())
    } else {
        label.clone()
    }
}

fn coerce_comparison_literals(
    mut node: ComparisonNode,
    logical_type: LogicalType,
) -> ComparisonNode {
    let coerce_operand = |op: Operand| match op {
        Operand::Literal(label) => {
            Operand::Literal(coerce_plain_bitical(&label, logical_type))
        }
        other => other,
    };
    node.first = coerce_operand(node.first);
    node.rest = node
        .rest
        .into_iter()
        .map(|(op, operand)| (op, coerce_operand(operand)))
        .collect();
    node
}

fn is_order_op(op: BinOper) -> bool {
    matches!(
        op,
        BinOper::GreaterThan
            | BinOper::GreaterThanOrEqual
            | BinOper::SmallerThan
            | BinOper::SmallerThanOrEqual
    )
}

impl Bitical {
    pub(crate) fn to_condition(
        &self,
        col: Col,
        op: BinOper,
        is_eav_col: bool,
    ) -> Condition {
        match self {
            Bitical::Integer(i) => {
                let target = if is_eav_col { Col::LabelInt } else { col };
                let mut cond = Condition::any()
                    .add(schema_pieces::col_cmp_i64(target, op, *i));
                if is_eav_col && is_order_op(op) {
                    cond = cond.add(schema_pieces::col_cmp_f64(
                        Col::LabelDouble,
                        op,
                        *i as f64,
                    ));
                }
                cond
            }
            Bitical::Double(d) => {
                let target = if is_eav_col { Col::LabelDouble } else { col };
                let mut cond = Condition::any()
                    .add(schema_pieces::col_cmp_f64(target, op, *d));
                if is_eav_col && is_order_op(op) {
                    cond = cond.add(schema_pieces::col_cmp_f64(
                        Col::LabelInt,
                        op,
                        *d,
                    ));
                }
                cond
            }
            Bitical::Boolean(b) => {
                let target = if is_eav_col { Col::LabelBool } else { col };
                Condition::any()
                    .add(schema_pieces::col_cmp_bool(target, op, *b))
            }
            Bitical::String(s) => {
                let target = if is_eav_col { Col::LabelStr } else { col };
                build_str_condition(target, op, s)
            }
            Bitical::Uuid(u) => {
                let target = if is_eav_col { Col::LabelStr } else { col };
                build_str_condition(target, op, &u.to_string())
            }
        }
    }
}

fn build_column_condition(
    col: Col,
    op: ComparisonOp,
    label: &Label,
    bitical_type: crate::db::BiticalType,
    _is_generic_context: bool,
) -> Condition {
    let bin_op = to_bin_op(op);

    // 汎用ラベルカラムか？ (Basic タグの EAV カラム)
    let is_eav_col = crate::db::BiticalType::to_columns().contains(&col);

    // 全一致 glob（`*` のみで構成されるパターン）の逆像はどの値域でも全域になる。
    if crate::util::is_full_match_glob(&label.as_str()) {
        return match bin_op {
            BinOper::Equal => Condition::all(),
            BinOper::NotEqual => Condition::any(),
            _ => label.value().to_condition(col, bin_op, is_eav_col),
        };
    }

    if let Some(cond) =
        double_decimal_glob_condition(bitical_type, col, label, bin_op)
    {
        return cond;
    }

    label.value().to_condition(col, bin_op, is_eav_col)
}

/// Double 型の値に対する小数フィールド glob（`*.5` / `2.*` 等）を値ベースの
/// 範囲・周期条件へ翻訳する。対象外（Double 以外・glob でない・フィールド内
/// 部分 glob 等）は None を返し、呼び出し元で既存の GLOB 照合にフォールバックする。
/// size（tag.rs の `SizeFn::expand_comparison`）と同じ区間の境界行列を順序演算子にも
/// 適用する（size 専用ではなく小数値一般で同じ挙動にするため）。
fn double_decimal_glob_condition(
    bitical_type: crate::db::BiticalType,
    col: Col,
    label: &Label,
    bin_op: BinOper,
) -> Option<Condition> {
    if bitical_type != crate::db::BiticalType::Double {
        return None;
    }
    let s = label.as_str();
    if !crate::util::is_glob_pattern(&s) {
        return None;
    }
    let expr = translate_double_decimal_glob(&s, col, bin_op)?;
    Some(Condition::any().add(expr))
}

/// `(expr, lo, hi)` の半開区間 `[lo, hi)` に演算子ごとの境界行列を適用する
/// （size の `size_glob_condition` と同じ行列）: Eq→区間内 / Ne→区間外 /
/// Gt・Ge→上限・下限側 / Lt・Le→下限・上限側。
fn range_op_condition(
    expr: SimpleExpr,
    op: BinOper,
    lo: f64,
    hi: f64,
) -> Option<SimpleExpr> {
    Some(match op {
        BinOper::Equal => expr.clone().gte(lo).and(expr.lt(hi)),
        BinOper::NotEqual => expr.clone().lt(lo).or(expr.gte(hi)),
        BinOper::GreaterThan => expr.gte(hi),
        BinOper::GreaterThanOrEqual => expr.gte(lo),
        BinOper::SmallerThan => expr.lt(lo),
        BinOper::SmallerThanOrEqual => expr.lt(hi),
        _ => return None,
    })
}

/// 小数部リテラル `digits`（桁数は不問）1つに対応する値の範囲
/// `[digits/10^k, (digits+1)/10^k)` を返す。
fn decimal_literal_band(digits: &str) -> (f64, f64) {
    let k = digits.len() as i32;
    let v: f64 = digits.parse().unwrap_or(0.0);
    let base = 10f64.powi(k);
    (v / base, (v + 1.0) / base)
}

/// Double カラムの値に対する数値部フィールド glob（整数部/小数部の2フィールド）を
/// SQL 条件式へ翻訳する。値ベース（表示・丸めではない）で、桁数の上限はない。
/// 小数部リテラルは負値も一致するよう絶対値の剰余で判定し、f64 の表現誤差を
/// 帯幅に比例した許容で吸収する。
fn translate_double_decimal_glob(
    pattern: &str,
    col: Col,
    op: BinOper,
) -> Option<SimpleExpr> {
    use crate::util::NumericField;
    let (int_str, dec_str) = match pattern.split_once('.') {
        Some((i, d)) => (i, Some(d)),
        None => (pattern, None),
    };
    let int_field = crate::util::parse_numeric_field(int_str)?;
    let dec_field = match dec_str {
        None => None,
        Some(d) => Some(crate::util::parse_numeric_field(d)?),
    };

    match (int_field, dec_field) {
        (NumericField::Literal(n_str), Some(NumericField::Free)) => {
            let n: f64 = n_str.parse().ok()?;
            range_op_condition(Expr::col(col).into(), op, n, n + 1.0)
        }
        (NumericField::Free, Some(NumericField::Literal(digits))) => {
            let (lo, hi) = decimal_literal_band(digits);
            let eps = (hi - lo) / 1000.0;
            let frac: SimpleExpr = Func::abs(Expr::col(col))
                .binary(BinOper::Custom("%"), Expr::val(1.0_f64));
            range_op_condition(frac, op, lo - eps, hi + eps)
        }
        _ => None,
    }
}

pub(crate) fn build_str_condition(
    col: Col,
    op: BinOper,
    val: &str,
) -> Condition {
    check_string_match(col, op, val)
        .map(|expr| Condition::any().add(expr))
        .unwrap_or_else(Condition::any)
}

pub(crate) fn check_string_match(
    col: Col,
    op: BinOper,
    val: &str,
) -> Option<SimpleExpr> {
    let has_glob = crate::util::is_glob_pattern(val);
    if !has_glob {
        return Some(schema_pieces::col_cmp_str(col, op, val));
    }

    // glob パターンは GLOB で照合する。不一致（`:^` / `:^=`）はその否定 —
    // op を捨てて GLOB に差し替えると不一致が「一致」に反転してしまう。
    let glob = schema_pieces::col_cmp_str(col, BinOper::Custom("GLOB"), val);
    Some(if op == BinOper::NotEqual {
        glob.not()
    } else {
        glob
    })
}

/// タグのメタデータ記述
#[derive(Clone)]
pub struct TagDescriptor {
    pub tag_type: TagType,
    pub storage: StorageMapping,
    pub logical_type: LogicalType,
    pub logical_function: Option<Arc<dyn TagFunction>>,
    pub sys_id: Option<i64>,
}

/// タグ知識の統合レジストリ
pub struct Lens {
    /// 登録順序を保持しつつタグ型でのルックアップも O(1) で行える IndexMap
    registry: IndexMap<TagType, TagDescriptor>,
}

impl LogicalSchema for Lens {
    fn get_logical_type(&self, tag: &TagType) -> LogicalType {
        self.look_up_or_default(tag).logical_type
    }

    fn expand_tag(
        &self,
        tag_type: &TagType,
        label: &Label,
    ) -> anyhow::Result<QueryNode> {
        if let Some(desc) = self.look_up(tag_type) {
            if let Some(func) = &desc.logical_function {
                let q = func.query();
                let label = coerce_plain_bitical(label, q.logical_type());
                let tag =
                    crate::types::TypedTag::retag(tag_type.clone(), &label);
                return q.expand(tag_type, &label, &tag, self);
            }
        }
        let label =
            coerce_plain_bitical(label, self.get_logical_type(tag_type));
        let tag = crate::types::TypedTag::retag(tag_type.clone(), &label);
        let predicate = QueryNode::Comparison(ComparisonNode {
            first: Operand::TypeRef(tag_type.clone()),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Eq),
                Operand::Literal(label),
            )],
        });
        Ok(QueryNode::TypedTag(tag.with_node(
            crate::query::Node::Expanded(Box::new(predicate)),
        )))
    }

    fn expand_projection(&self, tag_type: &TagType) -> QueryNode {
        if let Some(desc) = self.look_up(tag_type) {
            if let Some(func) = &desc.logical_function {
                return func.query().expand_projection(tag_type);
            }
        }
        QueryNode::base_nest(Operand::TypeRef(tag_type.clone()))
    }

    fn expand_comparison(
        &self,
        node: ComparisonNode,
    ) -> anyhow::Result<QueryNode> {
        let tag_type = find_tag_type_in_comparison(&node);
        let Some(tag_type) = tag_type else {
            return Ok(QueryNode::Comparison(node));
        };
        let Some(desc) = self.look_up(&tag_type) else {
            let logical_type = self.get_logical_type(&tag_type);
            return Ok(QueryNode::Comparison(coerce_comparison_literals(
                node,
                logical_type,
            )));
        };
        let Some(func) = &desc.logical_function else {
            return Ok(QueryNode::Comparison(coerce_comparison_literals(
                node,
                desc.logical_type,
            )));
        };
        func.query().expand_comparison(node)
    }

    fn iter_all_for_rank(
        &self,
    ) -> Vec<(TagType, crate::types::Rank, crate::types::ItemId)> {
        use crate::types::{ItemId, Origin};
        self.registry
            .values()
            .filter_map(|desc| {
                desc.logical_function.as_ref().map(|f| {
                    let id = match desc.sys_id {
                        Some(sys_id) => ItemId::Stored(sys_id),
                        None => ItemId::Settling(Origin::Plugin, 0),
                    };
                    (desc.tag_type.clone(), f.default_rank(), id)
                })
            })
            .collect()
    }

    fn item_kind(&self, tag_type: &TagType) -> Option<ItemKind> {
        self.look_up(tag_type)?
            .logical_function
            .as_ref()?
            .query()
            .item_kind()
    }
}

/// 比較ノード内の TypeRef から TagType を探す。
///
/// 算術（`(mtime: + 3600) :> X`）や Nest（`parentdir: &: max(mtime:) :> X`）で
/// ラップされていても見つける必要がある。ここで見つけられないと TagFn の
/// 値解釈に到達せず、同じクエリの意味が Nest の内と外で変わってしまう。
fn find_tag_type_in_comparison(node: &ComparisonNode) -> Option<TagType> {
    use crate::query::ast::AggregationNode;
    fn from_operand(op: &Operand) -> Option<TagType> {
        match op {
            Operand::TypeRef(tt) => Some(tt.clone()),
            Operand::Aggregation(agg) => from_aggregation(agg),
            Operand::Calculation(calc) => {
                from_operand(&calc.left).or_else(|| from_operand(&calc.right))
            }
            Operand::Query(q) => from_query_node(q),
            Operand::Literal(_) => None,
        }
    }
    fn from_aggregation(agg: &AggregationNode) -> Option<TagType> {
        let inner = match agg {
            AggregationNode::Count(q) => q.as_ref(),
            AggregationNode::Arithmetic { inner, .. } => inner.as_ref(),
        };
        from_query_node(inner)
    }
    fn from_query_node(node: &QueryNode) -> Option<TagType> {
        match node {
            QueryNode::Nest(nest) if nest.left.is_none() => match &nest.right {
                Operand::TypeRef(tt) => Some(tt.clone()),
                _ => None,
            },
            QueryNode::And(ns) | QueryNode::Or(ns) => {
                ns.iter().find_map(from_query_node)
            }
            QueryNode::Difference(l, _) => from_query_node(l),
            QueryNode::Aggregation(agg) => from_aggregation(agg),
            // 比較の対象になっているのは Nest の結果（右辺）。左辺は
            // グループ化のための Projection なので見ない。
            QueryNode::Nest(nest) => from_operand(&nest.right),
            _ => None,
        }
    }
    from_operand(&node.first)
        .or_else(|| node.rest.iter().find_map(|(_, op)| from_operand(op)))
}

impl Lens {
    /// 内部初期化用の一時的な空 Lens。外部からは通常 with_standard を使用してください。
    fn new_empty() -> Self {
        Self {
            registry: IndexMap::new(),
        }
    }

    /// 標準的なタグ定義を登録済みの Lens を返します。
    /// クエリ解釈を行わない、辞書単体としての Lens が必要な場合に使用します。
    pub fn base_standard() -> Self {
        let registry = crate::tag::TagRegistry::with_standard();
        Self::from_registry(&registry)
    }

    /// 既存の TagRegistry から Lens を構築します。
    /// FileManager など、すでに TagRegistry を保持している場合に使用します。
    pub fn from_registry(registry: &crate::tag::TagRegistry) -> Self {
        let mut lens = Self::new_empty();
        // Composite / Basic / Fixed タグ: TagRegistry から自動生成
        for func in registry.iter_arcs() {
            let q = func.query();
            let tag_type = TagType::from(func.name());
            let key = q.storage_key().unwrap_or(func.name()).to_string();
            let sys_id = registry.builtin_offset(func.name()).map(|offset| {
                crate::types::Origin::Builtin.block_lo() + offset as i64
            });
            let desc = match q.logical_role() {
                LogicalRole::Composite => TagDescriptor {
                    tag_type,
                    storage: StorageMapping::Composite,
                    logical_type: q.logical_type(),
                    logical_function: Some(func.clone()),
                    sys_id,
                },
                LogicalRole::Basic => {
                    let col = q.logical_type().to_bitical().to_column();
                    TagDescriptor {
                        tag_type,
                        storage: StorageMapping::Basic {
                            column: col,
                            tag_type: key,
                        },
                        logical_type: q.logical_type(),
                        logical_function: Some(func.clone()),
                        sys_id,
                    }
                }
                LogicalRole::Fixed => TagDescriptor {
                    // 物理ストレージは後続の base_column_descriptors で登録する
                    // Composite として登録することで Fixed 定義を上書きしない
                    tag_type,
                    storage: StorageMapping::Composite,
                    logical_type: q.logical_type(),
                    logical_function: Some(func.clone()),
                    sys_id,
                },
            };
            lens.register(desc);
        }
        // Fixed タグ: 専用 DB カラム定義（手動管理のまま）
        for desc in base_column_descriptors() {
            lens.register(desc);
        }
        // TypeConfig 定義（ユーザー定義・カスタム型の物理ストレージマッピング）
        for (tag_type, bt) in registry
            .type_configs()
            .iter()
            .filter(|(t, c)| {
                c.is_explicit && registry.get(t.as_str()).is_none()
            })
            .filter_map(|(t, c)| c.bitical_type.map(|b| (t, b)))
        {
            lens.register(TagDescriptor {
                tag_type: tag_type.clone(),
                storage: StorageMapping::Basic {
                    column: bt.to_column(),
                    tag_type: tag_type.as_str().to_string(),
                },
                logical_type: sql_to_logical(bt),
                logical_function: None,
                sys_id: None,
            });
        }
        lens
    }

    /// 指定されたタグが Fixed（固定 DB カラム）ストレージを持つかを返します。
    pub fn is_fixed(&self, tag: &TagType) -> bool {
        matches!(
            self.registry.get(tag).map(|d| &d.storage),
            Some(StorageMapping::Fixed(_))
        )
    }

    /// クエリ文字列を伴う、標準的な Focused Lens を生成します。
    /// パース、論理展開、物理解決すべてが完了した状態で返されます。

    /// タグ定義を登録します。既存の定義がある場合はマージします。
    pub fn register(&mut self, descriptor: TagDescriptor) {
        if let Some(existing) = self.registry.get_mut(&descriptor.tag_type) {
            // 物理ストレージ定義があれば上書き（Composite や既存の Fixed は上書きしない）
            if descriptor.storage != StorageMapping::Composite
                && !matches!(existing.storage, StorageMapping::Fixed(_))
            {
                existing.storage = descriptor.storage;
                existing.logical_type = descriptor.logical_type;
            }
            // 論理関数が提供されていれば上書き（sys_id も併せて）
            if descriptor.logical_function.is_some() {
                existing.logical_function = descriptor.logical_function;
                existing.sys_id = descriptor.sys_id;
            }
        } else {
            self.registry
                .insert(descriptor.tag_type.clone(), descriptor);
        }
    }

    /// 指定されたタグの定義を検索します。
    pub fn look_up(&self, tag: &TagType) -> Option<&TagDescriptor> {
        self.registry.get(tag)
    }

    /// 登録済みの (TagType, TagDescriptor) を反復する（read 解決の収集等で使用）。
    pub fn descriptors(
        &self,
    ) -> impl Iterator<Item = (&TagType, &TagDescriptor)> {
        self.registry.iter()
    }

    /// 指定されたタグの定義を検索し、見つからない場合はデフォルトの Basic 定義を返します。
    /// 未知のタグは Any 型として扱い、算術演算を許容します（実行時に DB がチェック）。
    pub fn look_up_or_default(&self, tag: &TagType) -> TagDescriptor {
        if let Some(desc) = self.registry.get(tag) {
            return (*desc).clone();
        }
        TagDescriptor {
            tag_type: tag.clone(),
            storage: StorageMapping::Basic {
                column: crate::db::Col::LabelStr,
                tag_type: tag.as_str().to_string(),
            },
            logical_type: LogicalType::Any, // 未知のタグは Any として扱う
            logical_function: None,
            sys_id: None,
        }
    }

    /// 特定の標準タグ（SType）に対応する物理カラムを解決します。
    pub fn resolve_col(
        &self,
        stype: crate::types::SType,
    ) -> anyhow::Result<crate::db::Col> {
        let tag = TagType::Base(stype);
        let desc = self.look_up(&tag).ok_or_else(|| {
            anyhow::anyhow!("Tag definition not found: {:?}", tag)
        })?;
        if let StorageMapping::Fixed(col) = desc.storage {
            Ok(col)
        } else {
            Err(anyhow::anyhow!(
                "Tag {:?} is not mapped to a direct column",
                tag
            ))
        }
    }

    /// 論理的なクエリタグを、Lens の定義に基づいて展開（Expand）します。

    pub fn decode_label_from_map(
        &self,
        tag_type: &TagType,
        map: &duckdb::types::OrderedMap<String, Value>,
    ) -> Option<Label> {
        let from_storage = |desc: &TagDescriptor| match &desc.storage {
            StorageMapping::Fixed(col) => {
                map.get(&col.name()).and_then(Bitical::from_scalar_db_value)
            }
            StorageMapping::Basic { column, tag_type } => {
                let type_val = map.get(&SType::Type.name())?;
                if type_val.as_str() == Some(tag_type) {
                    map.get(&column.name())
                        .and_then(Bitical::from_scalar_db_value)
                } else {
                    None
                }
            }
            StorageMapping::Composite => None,
        };

        let from_fallback = || {
            // Fallback: 物理カラム定義がない場合、汎用ラベルカラムから取得を試みる。
            // label_str は全型の VARCHAR フォールバックを兼ねるため、
            // 型付きカラムを先に評価する走査順を使う。
            crate::db::BiticalType::to_columns_scan_order()
                .iter()
                .find_map(|s| {
                    map.get(&s.name()).and_then(Bitical::from_scalar_db_value)
                })
        };

        let value = self
            .look_up(tag_type)
            .and_then(from_storage)
            .or_else(from_fallback)?;

        Some(Label::other(value))
    }

    pub fn resolve_label(
        &self,
        tag_type: &TagType,
        value: &Value,
    ) -> crate::types::TypedTag {
        let value = Bitical::from_scalar_db_value(value)
            .unwrap_or_else(|| Bitical::String(String::new()));
        crate::types::TypedTag::new(tag_type.clone(), value)
    }
}

trait ValueExt {
    fn as_str(&self) -> Option<&str>;
}

impl ValueExt for Value {
    fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// ComparisonOp を sea_query の BinOper に変換します。
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

// --- 純粋関数 (初期化用データ定義) ---

/// Fixed カラムの役割。Attribute はアイテムに実際に付与されている値として
/// `type:` プロジェクションへ合成される対象。Axis はタグ空間の座標軸
/// （type/tag/label 自体）であり、アイテムに付与されたタグではないため合成対象外。
#[derive(Clone, Copy)]
pub(crate) enum FixedRole {
    Attribute,
    Axis,
}

pub(crate) struct FixedColumn {
    pub stype: SType,
    pub col: Col,
    pub role: FixedRole,
}

/// Fixed カラムの唯一の情報源。属性か座標軸かの区別はここ一箇所にのみ持つ
/// （`base_column_descriptors` と `fixed_attributes` はどちらもここから導出する）。
const FIXED_COLUMNS: &[FixedColumn] = &[
    FixedColumn {
        stype: SType::ItemKind,
        col: Col::ItemKind,
        role: FixedRole::Attribute,
    },
    FixedColumn {
        stype: SType::Rank,
        col: Col::Rank,
        role: FixedRole::Attribute,
    },
    FixedColumn {
        stype: SType::ItemId,
        col: Col::ItemId,
        role: FixedRole::Attribute,
    },
    FixedColumn {
        stype: SType::Origin,
        col: Col::Origin,
        role: FixedRole::Attribute,
    },
    FixedColumn {
        stype: SType::Type,
        col: Col::Type,
        role: FixedRole::Axis,
    },
    FixedColumn {
        stype: SType::TypedTag,
        col: Col::TypedTag,
        role: FixedRole::Axis,
    },
    FixedColumn {
        stype: SType::Label,
        col: Col::LabelStr,
        role: FixedRole::Axis,
    },
];

fn base_column_descriptors() -> Vec<TagDescriptor> {
    FIXED_COLUMNS
        .iter()
        .map(|fc| TagDescriptor {
            tag_type: TagType::Base(fc.stype),
            storage: StorageMapping::Fixed(fc.col),
            logical_type: sql_to_logical(crate::db::BiticalType::from_col(
                fc.col,
            )),
            logical_function: None,
            sys_id: None,
        })
        .collect()
}

/// `type:` プロジェクションへの合成対象となる Fixed 属性の一覧（(SType, Col)）。
pub(crate) fn fixed_attributes() -> Vec<(SType, Col)> {
    FIXED_COLUMNS
        .iter()
        .filter_map(|fc| match fc.role {
            FixedRole::Attribute => Some((fc.stype, fc.col)),
            FixedRole::Axis => None,
        })
        .collect()
}

pub(crate) fn sql_to_logical(st: crate::db::BiticalType) -> LogicalType {
    match st {
        crate::db::BiticalType::Integer => LogicalType::Integer,
        crate::db::BiticalType::Double => LogicalType::Float,
        crate::db::BiticalType::String => LogicalType::String,
        crate::db::BiticalType::Boolean => LogicalType::Boolean,
        _ => LogicalType::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SType;

    fn expr_sql(expr: SimpleExpr) -> String {
        use sea_query::{PostgresQueryBuilder, Query};
        Query::select().expr(expr).to_string(PostgresQueryBuilder)
    }

    fn cond_where_sql(cond: Condition) -> String {
        use sea_query::{PostgresQueryBuilder, Query};
        Query::select()
            .expr(sea_query::Expr::val(1))
            .cond_where(cond)
            .to_string(PostgresQueryBuilder)
    }

    fn double_glob_sql(pattern: &str) -> String {
        double_glob_sql_with_op(pattern, crate::query::ast::BasicOp::Eq)
    }

    fn double_glob_sql_with_op(
        pattern: &str,
        op: crate::query::ast::BasicOp,
    ) -> String {
        use crate::types::{Bitical, Label};
        let label = Label::other(Bitical::String(pattern.to_string()));
        cond_where_sql(build_column_condition(
            Col::LabelDouble,
            ComparisonOp::Scalar(op),
            &label,
            crate::db::BiticalType::Double,
            true,
        ))
    }

    /// 小数部フィールド glob は Double カラムで剰余の周期条件になる。値ベースの
    /// 境界（帯幅に比例した許容込み）で、指定された桁までが一致し、それより
    /// 下の桁は自由。
    #[test]
    fn test_double_decimal_field_glob_becomes_periodic_condition() {
        let sql = double_glob_sql("*.5");
        assert!(
            !sql.contains("FALSE"),
            "小数部 glob が値条件ゼロに落ちてはいけない: {sql}"
        );
        let (lo, hi) = decimal_literal_band("5");
        let eps = (hi - lo) / 1000.0;
        assert!(
            sql.contains(&format!("{}", lo - eps)) && sql.contains(&format!("{}", hi + eps)),
            "小数部が [0.5, 0.6) 相当（表現誤差の許容込み）の周期条件になるべき: {sql}"
        );
    }

    /// 2桁指定は第2位まで絞る。
    #[test]
    fn test_double_two_digit_decimal_field_glob_narrows_range() {
        let sql = double_glob_sql("*.55");
        let (lo, hi) = decimal_literal_band("55");
        let eps = (hi - lo) / 1000.0;
        assert!(
            sql.contains(&format!("{}", lo - eps)) && sql.contains(&format!("{}", hi + eps)),
            "小数部が [0.55, 0.56) 相当（表現誤差の許容込み）に狭まるべき: {sql}"
        );
    }

    /// 負値も絶対値で照合するので `-2.5` が `*.5` に一致する。
    #[test]
    fn test_double_decimal_field_glob_uses_absolute_value() {
        let sql = double_glob_sql("*.5");
        assert!(
            sql.to_uppercase().contains("ABS"),
            "負値のために絶対値を取るべき: {sql}"
        );
    }

    /// 整数部リテラル・小数部自由は単一区間になる。
    #[test]
    fn test_double_integer_literal_decimal_free_becomes_single_range() {
        let sql = double_glob_sql("2.*");
        assert!(
            !sql.contains("FALSE"),
            "整数部リテラルの glob が値条件ゼロに落ちてはいけない: {sql}"
        );
        assert!(
            sql.contains('2') && sql.contains('3'),
            "[2.0, 3.0) の区間になるべき: {sql}"
        );
    }

    /// 範囲形（整数部リテラル・小数部自由）は size と同じ区間の境界行列を順序演算子にも
    /// 適用する: Gt→上限以上 / Ge→下限以上 / Lt→下限未満 / Le→上限未満。
    #[test]
    fn test_double_range_glob_order_ops_use_interval_bounds() {
        use crate::query::ast::BasicOp;
        let gt = double_glob_sql_with_op("2.*", BasicOp::Gt);
        assert!(
            gt.to_uppercase().contains(">= 3") || gt.contains(">=3"),
            "Gt は上限(3.0)以上になるべき: {gt}"
        );

        let ge = double_glob_sql_with_op("2.*", BasicOp::Ge);
        assert!(
            ge.contains(">= 2") && !ge.contains(">= 3"),
            "Ge は下限(2.0)以上になるべき: {ge}"
        );

        let lt = double_glob_sql_with_op("2.*", BasicOp::Lt);
        assert!(
            lt.contains("< 2") && !lt.contains("< 3"),
            "Lt は下限(2.0)未満になるべき: {lt}"
        );

        let le = double_glob_sql_with_op("2.*", BasicOp::Le);
        assert!(
            le.contains("< 3") && !le.contains("< 2"),
            "Le は上限(3.0)未満になるべき: {le}"
        );
    }

    /// 範囲形の Ne は区間外（OR）になる。
    #[test]
    fn test_double_range_glob_ne_becomes_outside_range() {
        use crate::query::ast::BasicOp;
        let ne = double_glob_sql_with_op("2.*", BasicOp::Ne);
        assert!(
            ne.contains("< 2") && (ne.contains(">= 3") || ne.contains(">=3")),
            "Ne は [2.0, 3.0) の外側（OR）になるべき: {ne}"
        );
    }

    /// 周期形（整数部自由・小数部リテラル）も同じ境界行列を、剰余（ABS(v) % 1）に適用する。
    #[test]
    fn test_double_periodic_glob_order_ops_wrap_modulo() {
        use crate::query::ast::BasicOp;
        let gt = double_glob_sql_with_op("*.5", BasicOp::Gt);
        assert!(
            gt.to_uppercase().contains("ABS"),
            "周期形は絶対値を使うべき: {gt}"
        );
        let (lo, hi) = decimal_literal_band("5");
        let eps = (hi - lo) / 1000.0;
        assert!(
            gt.contains(&format!("{}", hi + eps)),
            "Gt は帯の上限側になるべき: {gt}"
        );

        let ge = double_glob_sql_with_op("*.5", BasicOp::Ge);
        assert!(
            ge.contains(&format!("{}", lo - eps)),
            "Ge は帯の下限側になるべき: {ge}"
        );
    }

    /// 全一致 glob（`*`）は数値カラム型でも値条件を出さない（そのタグを持つ全アイテム）。
    #[test]
    fn test_build_column_condition_full_match_glob_eq_drops_value_condition() {
        use crate::types::{Bitical, Label};
        let label = Label::other(Bitical::String("*".to_string()));
        let cond = build_column_condition(
            Col::LabelInt,
            ComparisonOp::Scalar(crate::query::ast::BasicOp::Eq),
            &label,
            crate::db::BiticalType::Integer,
            true,
        );
        assert_eq!(
            cond_where_sql(cond),
            "SELECT 1 WHERE TRUE",
            "全一致 glob の一致は無条件（TRUE）になるべき"
        );
    }

    /// 全一致 glob（`*`）の不一致は FALSE（すべての値が一致するため）。
    #[test]
    fn test_build_column_condition_full_match_glob_ne_becomes_false() {
        use crate::types::{Bitical, Label};
        let label = Label::other(Bitical::String("*".to_string()));
        let cond = build_column_condition(
            Col::LabelInt,
            ComparisonOp::Scalar(crate::query::ast::BasicOp::Ne),
            &label,
            crate::db::BiticalType::Integer,
            true,
        );
        assert_eq!(
            cond_where_sql(cond),
            "SELECT 1 WHERE FALSE",
            "全一致 glob の不一致は FALSE になるべき"
        );
    }

    /// 不一致（`:^` / `:^=`）に glob パターンを与えたら**否定**になること。
    /// glob 文字を見ると op を無条件に GLOB へ差し替えていたため、
    /// 不一致が「一致」に反転していた。
    #[test]
    fn test_check_string_match_ne_with_glob_becomes_negation() {
        let expr = check_string_match(Col::LabelStr, BinOper::NotEqual, "*.rs")
            .expect("文字列カラムなので式が生成される");
        assert_eq!(
            expr_sql(expr),
            r#"SELECT NOT ("label_str" GLOB '*.rs')"#,
            "不一致 × glob は GLOB の否定になるべき"
        );
    }

    /// 一致（`:=`）は従来どおり GLOB のまま。
    #[test]
    fn test_check_string_match_eq_with_glob_stays_glob() {
        let expr = check_string_match(Col::LabelStr, BinOper::Equal, "*.rs")
            .expect("文字列カラムなので式が生成される");
        assert_eq!(expr_sql(expr), r#"SELECT "label_str" GLOB '*.rs'"#);
    }

    /// glob 文字を含まない不一致は素の `<>` のまま（既存の挙動）。
    #[test]
    fn test_check_string_match_ne_without_glob_stays_ne() {
        let expr = check_string_match(Col::LabelStr, BinOper::NotEqual, "foo")
            .expect("文字列カラムなので式が生成される");
        assert_eq!(expr_sql(expr), r#"SELECT "label_str" <> 'foo'"#);
    }

    /// テスト内で独立に計算した、指定日のローカル 23:59:59 の UTC 秒。
    /// `:>` は「その日を含まない」ので Gt の境界はこれになる。
    fn local_end_of_day(y: i32, m: u32, d: u32) -> i64 {
        use chrono::{Local, NaiveDate, TimeZone};
        let naive = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap();
        Local
            .from_local_datetime(&naive)
            .single()
            .unwrap()
            .timestamp()
    }

    /// 算術でラップされた mtime 比較（文脈 D）でも TagFn に到達し、
    /// `:>` の境界が ceiling（その日の 23:59:59）になること。
    /// 到達しないと右辺が `Label::Date` のまま残り、SQL 段で floor（00:00:00）と
    /// 比較されて 2026-02-01 12:00 のファイルが「2月1日より後」と判定される。
    #[test]
    fn test_expand_comparison_reaches_tagfn_through_calculation() {
        use crate::query::ast::{
            ArithmeticOp, BasicOp, CalculationNode, ComparisonNode,
            ComparisonOp,
        };
        use crate::types::{Label, TypedTag};

        let lens = Lens::base_standard();
        let calc = Operand::Calculation(Box::new(CalculationNode {
            left: Operand::TypeRef(TagType::Base(SType::Mtime)),
            op: ArithmeticOp::Add,
            right: Operand::Literal(TypedTag::new(SType::Size, 3600).label),
        }));
        let node = ComparisonNode {
            first: calc.clone(),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::Literal(Label::from("2026-02-01")),
            )],
        };

        let result = lens.expand_comparison(node).unwrap();
        let QueryNode::DateTimeRange { first, op, range } = result else {
            panic!("算術ラップは DateTimeRange を保つべき: {result:?}")
        };
        assert_eq!(first, calc, "算術の構造が保持されているべき");
        assert_eq!(op, BasicOp::Gt);
        let (_, end) = range.as_interval().unwrap();
        assert_eq!(
            end,
            local_end_of_day(2026, 2, 1),
            "右辺が ceiling へ翻訳されているべき"
        );
    }

    /// Nest でラップされた mtime 比較（文脈 F）でも TagFn に到達し、
    /// `:>` の境界が Nest の外（文脈 C）と一致すること。
    #[test]
    fn test_expand_comparison_reaches_tagfn_through_nest() {
        use crate::query::ast::{
            AggregationNode, ArithmeticAggOp, BasicOp, ComparisonNode,
            ComparisonOp, NestNode,
        };
        use crate::types::Label;

        let lens = Lens::base_standard();
        let max_mtime = AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Max,
            inner: Box::new(QueryNode::base_nest(Operand::TypeRef(
                TagType::Base(SType::Mtime),
            ))),
        };
        let nest = Operand::Query(Box::new(QueryNode::Nest(NestNode {
            left: Some(Box::new(QueryNode::base_nest(Operand::TypeRef(
                TagType::Base(SType::Parentdir),
            )))),
            right: Operand::Aggregation(Box::new(max_mtime)),
        })));
        let node = ComparisonNode {
            first: nest.clone(),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::Literal(Label::from("2026-02-01")),
            )],
        };

        let result = lens.expand_comparison(node).unwrap();
        let QueryNode::DateTimeRange { first, op, range } = result else {
            panic!("Nest ラップは DateTimeRange を保つべき: {result:?}")
        };
        assert_eq!(first, nest, "Nest の構造が保持されているべき");
        assert_eq!(op, BasicOp::Gt);
        let (_, end) = range.as_interval().unwrap();
        assert_eq!(
            end,
            local_end_of_day(2026, 2, 1),
            "右辺が ceiling へ翻訳されているべき（Nest の外と同じ境界）"
        );
    }

    #[test]
    fn test_lens_with_standard_includes_rank() {
        let lens = Lens::base_standard();
        let found = lens.look_up(&TagType::Base(SType::Rank)).unwrap();
        // マージ論理により、Column 定義が Virtual を上書きしているはず
        assert_eq!(found.storage, StorageMapping::Fixed(Col::Rank));
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_origin() {
        let lens = Lens::base_standard();
        let found = lens.look_up(&TagType::Base(SType::Origin)).unwrap();
        assert_eq!(found.storage, StorageMapping::Fixed(Col::Origin));
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_size() {
        let lens = Lens::base_standard();
        let found = lens.look_up(&TagType::Base(SType::Size)).unwrap();
        if let StorageMapping::Basic { column, tag_type } = &found.storage {
            assert_eq!(*column, Col::LabelInt);
            assert_eq!(tag_type, "size");
        } else {
            panic!("Expected Basic mapping for size, got {:?}", found.storage);
        }
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_directory_as_virtual() {
        let lens = Lens::base_standard();
        let found = lens.look_up(&TagType::Base(SType::Directory)).unwrap();
        assert_eq!(found.storage, StorageMapping::Composite);
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_look_up_unknown_tag_returns_none() {
        let lens = Lens::base_standard();
        let unknown = TagType::from("magic_tag_that_does_not_exist");
        assert!(lens.look_up(&unknown).is_none());
    }

    #[test]
    fn test_lens_filename_is_virtual() {
        let lens = Lens::base_standard();
        let found = lens.look_up(&TagType::Base(SType::Filename)).unwrap();
        if let StorageMapping::Basic { tag_type, .. } = &found.storage {
            assert_eq!(tag_type, "filename");
        } else {
            panic!("Expected Basic for filename, got {:?}", found.storage);
        }
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_all_standard_tags_are_resolvable() {
        let lens = Lens::base_standard();
        let standard_types = vec![
            SType::ItemId,
            SType::FileId,
            SType::Rank,
            SType::Origin,
            SType::ItemKind,
            SType::Type,
            SType::TypedTag,
            SType::Label,
            SType::Size,
            SType::Extension,
            SType::Mtime,
            SType::Path,
            SType::Filename,
            SType::Parentdir,
            SType::Stem,
            SType::IsDir,
            SType::Hash,
            SType::Content,
            SType::Directory,
            SType::Name,
        ];

        for stype in standard_types {
            let tag_type = TagType::Base(stype);
            let found = lens.look_up(&tag_type);
            assert!(
                found.is_some(),
                "Standard tag {:?} should be resolvable",
                stype
            );
        }
    }

    #[test]
    fn test_decode_label_from_map_fallback_prefers_typed_columns() {
        // oneview は全行に label_str（VARCHAR フォールバック）を設定するため、
        // fallback 走査は型付きカラムを先に評価しなければならない
        // （BiticalType::to_columns_scan_order と同じ順序）。
        let lens = Lens::base_standard();
        let tag = TagType::from("magic_tag_that_does_not_exist");
        let map = duckdb::types::OrderedMap::from(vec![
            (SType::LabelStr.name(), Value::Text("true".to_string())),
            (SType::LabelBool.name(), Value::Boolean(true)),
        ]);
        let label = lens.decode_label_from_map(&tag, &map).unwrap();
        assert_eq!(label.value(), Bitical::Boolean(true));
    }

    #[test]
    fn test_lens_iter_all_for_rank_matches_registry_default_rank_for_extension()
    {
        let registry = crate::tag::TagRegistry::with_standard();
        let lens = Lens::from_registry(&registry);
        let expected_rank = registry.get("extension").unwrap().default_rank();
        let all = lens.iter_all_for_rank();
        let found = all
            .iter()
            .find(|(t, _, _)| *t == TagType::Base(SType::Extension));
        assert_eq!(found.map(|(_, r, _)| *r), Some(expected_rank));
    }

    // Plan: 定義アイテムの id 区画 — Step 3
    // 組み込み型（hash）は iter_all_for_rank の3要素目が Stored(固定 Sys id)。
    #[test]
    fn test_lens_iter_all_for_rank_bakes_fixed_sys_id_for_builtin() {
        use crate::types::{ItemId, Origin};
        let registry = crate::tag::TagRegistry::with_standard();
        let expected_sys_id = Origin::Builtin.block_lo()
            + registry.builtin_offset("hash").unwrap() as i64;
        let lens = Lens::from_registry(&registry);
        let all = lens.iter_all_for_rank();
        let found = all
            .iter()
            .find(|(t, _, _)| *t == TagType::Base(SType::Hash));
        assert_eq!(
            found.map(|(_, _, id)| *id),
            Some(ItemId::Stored(expected_sys_id))
        );
    }

    // プラグイン登録型は固定 Sys id を持たず、Settling(Plugin, _) になる。
    #[test]
    fn test_lens_iter_all_for_rank_plugin_has_no_sys_id() {
        use crate::types::{ItemId, Origin};
        struct MockPluginTag;
        impl crate::tag::TagFunction for MockPluginTag {
            fn name(&self) -> &str {
                "mock_plugin_tag"
            }
        }
        let mut registry = crate::tag::TagRegistry::with_standard();
        registry.register_plugin(MockPluginTag);
        let lens = Lens::from_registry(&registry);
        let all = lens.iter_all_for_rank();
        let found = all
            .iter()
            .find(|(t, _, _)| *t == TagType::from("mock_plugin_tag"));
        assert!(matches!(
            found.map(|(_, _, id)| *id),
            Some(ItemId::Settling(Origin::Plugin, _))
        ));
    }

    #[test]
    fn test_lens_get_logical_type() {
        let lens = Lens::base_standard();
        // size: is Integer
        assert_eq!(
            lens.get_logical_type(&TagType::Base(SType::Size)),
            LogicalType::Integer
        );
        // path: is String
        assert_eq!(
            lens.get_logical_type(&TagType::Base(SType::Path)),
            LogicalType::String
        );
        // is_dir: is Boolean
        assert_eq!(
            lens.get_logical_type(&TagType::Base(SType::IsDir)),
            LogicalType::Boolean
        );
    }

    #[test]
    fn test_check_string_match_caret_is_literal_not_prefix_glob() {
        let expr =
            check_string_match(Col::LabelStr, BinOper::Equal, "^foo").unwrap();
        let sql = sea_query::Query::select()
            .expr(expr)
            .to_string(sea_query::PostgresQueryBuilder);
        assert!(
            sql.contains("'^foo'"),
            "value should stay literal '^foo': {}",
            sql
        );
        assert!(
            !sql.contains("GLOB"),
            "must not convert to GLOB prefix match: {}",
            sql
        );
    }
}
