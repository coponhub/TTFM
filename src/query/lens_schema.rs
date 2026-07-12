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
use crate::query::ast::{ComparisonNode, ComparisonOp, Operand, QueryNode};
use crate::query::logical_schema::{LogicalSchema, LogicalType};
use crate::query::sql::schema_pieces;
use crate::tag::{LogicalRole, TagFunction};
use crate::types::{Bitical, Label, SType, TagType};
use duckdb::types::Value;
use indexmap::IndexMap;
use sea_query::{BinOper, Condition, SimpleExpr};
use std::sync::Arc;

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
    let tag_op = if tag_type.contains('*')
        || tag_type.contains('?')
        || tag_type.contains('[')
    {
        BinOper::Custom("GLOB")
    } else {
        BinOper::Equal
    };
    schema_pieces::type_filter(tag_op, tag_type)
}

impl Bitical {
    /// 値の物理型に応じた列一致条件を返す。GLOB/汎用値パース等の型別ルールは
    /// `build_int_condition`/`build_str_condition` に委譲する。
    pub(crate) fn to_condition(
        &self,
        col: Col,
        op: BinOper,
        bitical_type: crate::db::BiticalType,
        is_eav_col: bool,
    ) -> Condition {
        match self {
            Bitical::Integer(i) => build_int_condition(col, op, *i, is_eav_col),
            Bitical::Boolean(b) => build_str_condition(
                col,
                op,
                &b.to_string(),
                bitical_type,
                is_eav_col,
            ),
            Bitical::Double(d) => schema_pieces::build_double_condition(op, *d),
            Bitical::String(s) => {
                build_str_condition(col, op, s, bitical_type, is_eav_col)
            }
            Bitical::Uuid(u) => build_str_condition(
                col,
                op,
                &u.to_string(),
                bitical_type,
                is_eav_col,
            ),
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
    let is_eav_col = col == Col::LabelStr
        || col == Col::LabelInt
        || col == Col::LabelDouble
        || col == Col::LabelBool;

    // `Label::Literal`（quoted）は完全一致検索、それ以外の String は通常検索。
    // `.value()` 経由だと Literal 性が失われる（Bitical に Literal 変種が無い）ため、
    // まず `label` 自体で判定する。
    if let Label::Literal(_, s) = label {
        return build_literal_condition(col, bin_op, s, is_eav_col);
    }

    label
        .value()
        .to_condition(col, bin_op, bitical_type, is_eav_col)
}

pub(crate) fn build_int_condition(
    col: Col,
    op: BinOper,
    val: i64,
    is_generic: bool,
) -> Condition {
    let mut cond = Condition::any();
    if col != Col::LabelStr {
        cond = cond.add(schema_pieces::col_cmp_i64(col, op, val));
    }
    if is_generic {
        cond = cond.add(schema_pieces::col_cmp_f64(
            Col::LabelDouble,
            op,
            val as f64,
        ));
        if col == Col::LabelStr {
            cond = cond.add(schema_pieces::col_cmp_i64(Col::LabelInt, op, val));
        }
    }
    cond
}

pub(crate) fn build_str_condition(
    col: Col,
    op: BinOper,
    val: &str,
    bitical_type: crate::db::BiticalType,
    is_generic: bool,
) -> Condition {
    let string_cond = check_string_match(col, op, val, bitical_type)
        .map(|expr| Condition::any().add(expr));
    let generic_conds = if is_generic {
        try_parse_generic_value_as_cond(col, op, val)
    } else {
        None
    };

    [string_cond, generic_conds]
        .into_iter()
        .flatten()
        .fold(Condition::any(), |acc, cond| acc.add(cond))
}

pub(crate) fn check_string_match(
    col: Col,
    op: BinOper,
    val: &str,
    bitical_type: crate::db::BiticalType,
) -> Option<SimpleExpr> {
    let is_numeric_field = matches!(
        bitical_type,
        crate::db::BiticalType::Integer | crate::db::BiticalType::Double
    );

    if is_numeric_field {
        return None;
    }

    let (val_str, effective_op) = if val.starts_with('^') {
        (format!("{}*", &val[1..]), BinOper::Custom("GLOB"))
    } else if val.contains('*') || val.contains('?') || val.contains('[') {
        (val.to_string(), BinOper::Custom("GLOB"))
    } else {
        (val.to_string(), op)
    };

    Some(schema_pieces::col_cmp_str(col, effective_op, &val_str))
}

pub(crate) fn try_parse_generic_value_as_cond(
    col: Col,
    op: BinOper,
    val: &str,
) -> Option<Condition> {
    val.parse::<i64>()
        .ok()
        .map(|i| {
            let mut c = Condition::any()
                .add(schema_pieces::col_cmp_i64(Col::LabelInt, op, i))
                .add(schema_pieces::col_cmp_f64(
                    Col::LabelDouble,
                    op,
                    i as f64,
                ));
            if col == Col::LabelStr {
                c = c.add(schema_pieces::col_cmp_i64(Col::LabelInt, op, i));
            }
            c
        })
        .or_else(|| {
            val.parse::<f64>().ok().map(|f| {
                Condition::any().add(schema_pieces::col_cmp_f64(
                    Col::LabelDouble,
                    op,
                    f,
                ))
            })
        })
        .or_else(|| {
            match val {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }
            .map(|b| {
                Condition::any().add(schema_pieces::col_cmp_bool(
                    Col::LabelBool,
                    op,
                    b,
                ))
            })
        })
}

pub(crate) fn build_literal_condition(
    col: Col,
    op: BinOper,
    val: &str,
    is_generic: bool,
) -> Condition {
    let literal_cond = schema_pieces::col_cmp_str(col, op, val);
    let generic_conds = if is_generic {
        try_parse_generic_value_as_cond(col, op, val)
    } else {
        None
    };

    std::iter::once(Condition::any().add(literal_cond))
        .chain(generic_conds)
        .fold(Condition::any(), |acc, cond| acc.add(cond))
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

impl TagDescriptor {
    /// 論理型から物理型（BiticalType）への一方向マッピングを提供します。
    pub fn logical_to_sql(lt: LogicalType) -> crate::db::BiticalType {
        use crate::db::BiticalType;
        match lt {
            LogicalType::Integer => BiticalType::Integer,
            LogicalType::Float => BiticalType::Double,
            LogicalType::String => BiticalType::String,
            LogicalType::Boolean => BiticalType::Boolean,
            LogicalType::Any => BiticalType::String,
        }
    }

    /// このタグの物理型（BiticalType）を返します。
    pub fn sql_type(&self) -> crate::db::BiticalType {
        Self::logical_to_sql(self.logical_type)
    }
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

    fn expand_tag(&self, tag_type: &TagType, label: &Label) -> QueryNode {
        if let Some(desc) = self.look_up(tag_type) {
            if let Some(func) = &desc.logical_function {
                let q = func.query();
                // normalize_label を適用してから expand する
                let normalized = q.normalize_label(label);
                let tag = crate::types::TypedTag::retag(
                    tag_type.clone(),
                    &normalized,
                );
                return q.expand(tag_type, &normalized, &tag, self);
            }
        }
        QueryNode::TypedTag(crate::types::TypedTag::retag(
            tag_type.clone(),
            label,
        ))
    }

    fn expand_projection(&self, tag_type: &TagType) -> QueryNode {
        if let Some(desc) = self.look_up(tag_type) {
            if let Some(func) = &desc.logical_function {
                return func.query().expand_projection(tag_type);
            }
        }
        QueryNode::Projection(Operand::TypeRef(tag_type.clone()))
    }

    fn normalize_label_any(&self, label: &Label) -> Label {
        if matches!(label, Label::Literal(..)) {
            return label.clone();
        }
        // TagRegistry と同様に登録の逆順で走査する
        for desc in self.registry.values().rev() {
            if let Some(func) = &desc.logical_function {
                let normalized = func.query().normalize_label(label);
                if normalized != *label {
                    return normalized;
                }
            }
        }
        label.clone()
    }

    fn expand_comparison(&self, node: ComparisonNode) -> QueryNode {
        let tag_type = find_tag_type_in_comparison(&node);
        let Some(tag_type) = tag_type else {
            return QueryNode::Comparison(node);
        };
        let Some(desc) = self.look_up(&tag_type) else {
            return QueryNode::Comparison(node);
        };
        let Some(func) = &desc.logical_function else {
            return QueryNode::Comparison(node);
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
}

/// 比較ノード内の TypeRef から TagType を探す。
fn find_tag_type_in_comparison(node: &ComparisonNode) -> Option<TagType> {
    fn from_operand(op: &Operand) -> Option<TagType> {
        use crate::query::ast::AggregationNode;
        match op {
            Operand::TypeRef(tt) => Some(tt.clone()),
            Operand::Aggregation(agg) => {
                let inner = match agg.as_ref() {
                    AggregationNode::Count(q) => q.as_ref(),
                    AggregationNode::Arithmetic { inner, .. } => inner.as_ref(),
                };
                from_query_node(inner)
            }
            _ => None,
        }
    }
    fn from_query_node(node: &QueryNode) -> Option<TagType> {
        match node {
            QueryNode::Projection(Operand::TypeRef(tt)) => Some(tt.clone()),
            QueryNode::And(ns) | QueryNode::Or(ns) => {
                ns.iter().find_map(from_query_node)
            }
            QueryNode::Difference(l, _) => from_query_node(l),
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
        // Fixed タグ: 専用 DB カラム定義（手動管理のまま）
        for desc in base_column_descriptors() {
            lens.register(desc);
        }
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
                    let col = logical_type_to_col(q.logical_type());
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
                    // 物理ストレージは base_column_descriptors で登録済み
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
        lens
    }

    /// クエリ文字列を伴う、標準的な Focused Lens を生成します。
    /// パース、論理展開、物理解決すべてが完了した状態で返されます。

    /// タグ定義を登録します。既存の定義がある場合はマージします。
    pub fn register(&mut self, descriptor: TagDescriptor) {
        if let Some(existing) = self.registry.get_mut(&descriptor.tag_type) {
            // 物理ストレージ定義があれば上書き（Virtual は物理を上書きしない）
            if descriptor.storage != StorageMapping::Composite {
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
            // Fallback: 物理カラム定義がない場合、汎用ラベルカラムから取得を試みる
            [
                SType::LabelInt,
                SType::LabelStr,
                SType::LabelBool,
                SType::LabelDouble,
            ]
            .iter()
            .find_map(|s| {
                map.get(&s.name()).and_then(Bitical::from_scalar_db_value)
            })
        };

        let value = self
            .look_up(tag_type)
            .and_then(from_storage)
            .or_else(from_fallback)?;

        Some(Label::resolve(tag_type.clone(), value))
    }

    pub fn resolve_label(&self, tag_type: &TagType, value: &Value) -> Label {
        let value = Bitical::from_scalar_db_value(value)
            .unwrap_or_else(|| Bitical::String(String::new()));
        Label::resolve(tag_type.clone(), value)
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

pub(crate) fn logical_type_to_col(lt: LogicalType) -> Col {
    use crate::db::BiticalType;
    match TagDescriptor::logical_to_sql(lt) {
        BiticalType::Integer => Col::LabelInt,
        BiticalType::Double => Col::LabelDouble,
        BiticalType::Boolean => Col::LabelBool,
        _ => Col::LabelStr,
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
    fn test_logical_to_sql_mapping() {
        assert_eq!(
            TagDescriptor::logical_to_sql(LogicalType::Integer),
            crate::db::BiticalType::Integer
        );
        assert_eq!(
            TagDescriptor::logical_to_sql(LogicalType::Float),
            crate::db::BiticalType::Double
        );
        assert_eq!(
            TagDescriptor::logical_to_sql(LogicalType::String),
            crate::db::BiticalType::String
        );
        assert_eq!(
            TagDescriptor::logical_to_sql(LogicalType::Boolean),
            crate::db::BiticalType::Boolean
        );
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
}
