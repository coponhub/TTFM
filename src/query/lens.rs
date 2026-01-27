use crate::db::Col;
use crate::query::ast::ComparisonOp;
use crate::query::functions::*;
use crate::query::QueryFunction;
use crate::types::{Label, LabelValue, SType, TagType};
use anyhow::Result;
use duckdb::types::Value;
use sea_query::{BinOper, Condition, Expr, SimpleExpr};
use std::collections::HashMap;
use std::sync::Arc;

/// タグの物理的な格納場所
#[derive(Debug, PartialEq, Clone)]
pub enum StorageMapping {
    /// oneview の直接のカラム
    Column(Col),
    /// oneview の行ベースのタグ (特定のラベルカラム + タグ名)
    RowTag { column: Col, tag_key: String },
    /// 他のタグに展開される論理タグ
    Virtual,
}

impl StorageMapping {
    /// このストレージマッピングに基づき、指定された演算子とラベルに対する SQL 条件を生成します。
    pub fn to_condition(
        &self,
        op: ComparisonOp,
        label: &Label,
        sql_type: crate::db::SqlType,
    ) -> Condition {
        match self {
            StorageMapping::Column(col) => {
                build_column_condition(*col, op, label, sql_type, false)
            }
            StorageMapping::RowTag { column, tag_key } => {
                Condition::all().add(check_tag_match(tag_key)).add(
                    build_column_condition(*column, op, label, sql_type, true),
                )
            }
            StorageMapping::Virtual => Condition::any(),
        }
    }
}

/// 物理マッピングが解決された後のクエリノード。
#[derive(Debug, Clone)]
pub enum ResolvedNode {
    And(Vec<ResolvedNode>),
    Or(Vec<ResolvedNode>),
    Difference(Box<ResolvedNode>, Box<ResolvedNode>),
    Complement(Box<ResolvedNode>),
    /// 投影クエリ用。
    Projection(TagType, StorageMapping),
    /// 物理カラムへの直接マッチ。
    ColumnMatch {
        tag: crate::types::SType,
        label: Label,
    },
    /// 物理的な条件。
    Match {
        tag_type: TagType,
        storage: StorageMapping,
        sql_type: crate::db::SqlType,
        op: ComparisonOp,
        label: Label,
    },
}

impl ResolvedNode {
    /// このノードを sea_query::Condition に変換します。
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
            ResolvedNode::Projection(_tt, storage) => cond_projection(storage),
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
        }
    }

    /// このノードが投影（Projection）を目的としている場合、その対象の型を返します。
    pub fn get_projection(&self) -> Option<TagType> {
        match self {
            ResolvedNode::Projection(tt, _) => Some(tt.clone()),
            ResolvedNode::And(nodes) | ResolvedNode::Or(nodes) => {
                nodes.iter().find_map(|n| n.get_projection())
            }
            ResolvedNode::Difference(l, _) | ResolvedNode::Complement(l) => {
                l.get_projection()
            }
            _ => None,
        }
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
        StorageMapping::RowTag { tag_key, .. } => {
            Condition::all().add(check_tag_match(tag_key))
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
        crate::types::LabelValue::String(s)
        | crate::types::LabelValue::Literal(s) => Expr::val(s),
    };
    // ColumnMatch の場合は型固有のルールは適用せず、単純にマッピング
    Condition::all().add(Expr::col(col).eq(val))
}

fn check_tag_match(tag_key: &str) -> SimpleExpr {
    let mut tag_op = BinOper::Equal;
    if tag_key.contains('*') || tag_key.contains('?') || tag_key.contains('[') {
        tag_op = BinOper::Custom("GLOB");
    }
    Expr::col(crate::db::Col::Type).binary(tag_op, tag_key)
}

fn build_column_condition(
    col: Col,
    op: ComparisonOp,
    label: &Label,
    sql_type: crate::db::SqlType,
    _is_generic_context: bool,
) -> Condition {
    let bin_op = to_bin_op(op);

    // 汎用カラムか？ (oneviewのRowTag用カラム)
    let is_generic_row_col = col == Col::LabelStr
        || col == Col::LabelInt
        || col == Col::LabelDouble
        || col == Col::LabelBool;

    match label.value() {
        crate::types::LabelValue::Integer(i) => {
            build_int_condition(col, bin_op, i, is_generic_row_col)
        }
        crate::types::LabelValue::Boolean(b) => build_str_condition(
            col,
            bin_op,
            &b.to_string(),
            sql_type,
            is_generic_row_col,
        ),
        crate::types::LabelValue::String(s) => {
            build_str_condition(col, bin_op, &s, sql_type, is_generic_row_col)
        }
        crate::types::LabelValue::Literal(s) => {
            build_literal_condition(col, bin_op, &s, is_generic_row_col)
        }
    }
}

fn build_int_condition(
    col: Col,
    op: BinOper,
    val: i64,
    is_generic: bool,
) -> Condition {
    let mut cond = Condition::any();
    if col != Col::LabelStr {
        cond = cond.add(Expr::col(col).binary(op, Expr::val(val)));
    }
    if is_generic {
        cond = cond
            .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(val as f64)));
        if col == Col::LabelStr {
            cond =
                cond.add(Expr::col(Col::LabelInt).binary(op, Expr::val(val)));
        }
    }
    cond
}

fn build_str_condition(
    col: Col,
    op: BinOper,
    val: &str,
    sql_type: crate::db::SqlType,
    is_generic: bool,
) -> Condition {
    let string_cond = check_string_match(col, op, val, sql_type)
        .map(|expr| Condition::any().add(expr));
    let generic_conds = if is_generic {
        try_parse_generic_value_as_cond(col, op, val)
    } else {
        None
    };

    [string_cond, generic_conds]
        .into_iter()
        .flatten() // None を除去
        .fold(Condition::any(), |acc, cond| acc.add(cond))
}

fn check_string_match(
    col: Col,
    op: BinOper,
    val: &str,
    sql_type: crate::db::SqlType,
) -> Option<sea_query::SimpleExpr> {
    let is_numeric_field = matches!(
        sql_type,
        crate::db::SqlType::BIGINT | crate::db::SqlType::DOUBLE
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

    Some(Expr::col(col).binary(effective_op, val_str))
}

fn try_parse_generic_value_as_cond(
    col: Col,
    op: BinOper,
    val: &str,
) -> Option<Condition> {
    // Try Integer -> Float -> Boolean chain
    val.parse::<i64>()
        .ok()
        .map(|i| {
            let mut c = Condition::any()
                .add(Expr::col(Col::LabelInt).binary(op, Expr::val(i)))
                .add(
                    Expr::col(Col::LabelDouble).binary(op, Expr::val(i as f64)),
                );
            if col == Col::LabelStr {
                c = c.add(Expr::col(Col::LabelInt).binary(op, Expr::val(i)));
            }
            c
        })
        .or_else(|| {
            val.parse::<f64>().ok().map(|f| {
                Condition::any()
                    .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(f)))
            })
        })
        .or_else(|| {
            match val {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }
            .map(|b| {
                Condition::any()
                    .add(Expr::col(Col::LabelBool).binary(op, Expr::val(b)))
            })
        })
}

fn build_literal_condition(
    col: Col,
    op: BinOper,
    val: &str,
    is_generic: bool,
) -> Condition {
    let literal_cond = Expr::col(col).binary(op, Expr::val(val));
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
    pub sql_type: crate::db::SqlType,
    pub logical_function: Option<Arc<dyn QueryFunction>>,
}

/// タグ知識の統合レジストリ
pub struct Lens {
    registry: HashMap<TagType, TagDescriptor>,
    pub expanded_query: crate::query::ast::QueryNode,
    pub resolved_query: ResolvedNode,
}

impl Lens {
    /// 内部初期化用の一時的な空 Lens。外部からは通常 with_standard を使用してください。
    fn new_empty() -> Self {
        Self {
            registry: HashMap::new(),
            expanded_query: crate::query::ast::QueryNode::And(vec![]),
            resolved_query: ResolvedNode::And(vec![]),
        }
    }

    /// 標準的なタグ定義を登録済みの Lens を返します。
    /// クエリ解釈を行わない、辞書単体としての Lens が必要な場合に使用します。
    pub fn base_standard() -> Self {
        let mut lens = Self::new_empty();
        for desc in base_column_descriptors() {
            lens.register(desc);
        }
        for desc in row_tag_descriptors() {
            lens.register(desc);
        }
        for desc in virtual_tag_descriptors() {
            lens.register(desc);
        }
        lens
    }

    /// クエリ文字列を伴う、標準的な Focused Lens を生成します。
    /// パース、論理展開、物理解決すべてが完了した状態で返されます。
    pub fn with_standard(query: &str) -> Result<Self> {
        let base = Self::base_standard();
        let node = if query.trim().is_empty() {
            crate::query::ast::QueryNode::And(vec![])
        } else {
            crate::query::parse(query)?
        };

        let expanded = base.expand(node)?;
        let resolved = base.resolve(expanded.clone())?;

        Ok(Self {
            registry: base.registry,
            expanded_query: expanded,
            resolved_query: resolved,
        })
    }

    /// タグ定義を登録します。既存の定義がある場合はマージします。
    pub fn register(&mut self, descriptor: TagDescriptor) {
        if let Some(existing) = self.registry.get_mut(&descriptor.tag_type) {
            // 物理ストレージ定義があれば上書き（Virtual は物理を上書きしない）
            if descriptor.storage != StorageMapping::Virtual {
                existing.storage = descriptor.storage;
                existing.sql_type = descriptor.sql_type;
            }
            // 論理関数が提供されていれば上書き
            if descriptor.logical_function.is_some() {
                existing.logical_function = descriptor.logical_function;
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

    /// 指定されたタグの定義を検索し、見つからない場合はデフォルトの RowTag 定義を返します。
    pub fn look_up_or_default(&self, tag: &TagType) -> TagDescriptor {
        if let Some(desc) = self.registry.get(tag) {
            return (*desc).clone();
        }
        TagDescriptor {
            tag_type: tag.clone(),
            storage: StorageMapping::RowTag {
                column: crate::db::Col::LabelStr,
                tag_key: tag.as_str().to_string(),
            },
            sql_type: crate::db::SqlType::VARCHAR,
            logical_function: None,
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
        if let StorageMapping::Column(col) = desc.storage {
            Ok(col)
        } else {
            Err(anyhow::anyhow!(
                "Tag {:?} is not mapped to a direct column",
                tag
            ))
        }
    }

    /// 論理的なクエリタグを、Lens の定義に基づいて展開（Expand）します。
    pub fn expand(
        &self,
        node: crate::query::ast::QueryNode,
    ) -> anyhow::Result<crate::query::ast::QueryNode> {
        expand_query_node(self, node)
    }

    /// 展開済みノードを、物理的な所在（StorageMapping）を持つ ResolvedNode へ解決します。
    pub fn resolve(
        &self,
        node: crate::query::ast::QueryNode,
    ) -> anyhow::Result<ResolvedNode> {
        resolve_query_node(self, node)
    }

    /// クエリがプロジェクション（投影）を目的としている場合、その対象の型を返します。
    pub fn get_projection(&self) -> Option<TagType> {
        self.resolved_query.get_projection()
    }

    pub fn decode_label_from_map(
        &self,
        tag_type: &TagType,
        map: &duckdb::types::OrderedMap<String, Value>,
    ) -> Option<Label> {
        let from_storage = |desc: &TagDescriptor| match &desc.storage {
            StorageMapping::Column(col) => {
                map.get(&col.name()).and_then(val_to_label_value)
            }
            StorageMapping::RowTag { column, tag_key } => {
                let type_val = map.get(&SType::Type.name())?;
                if type_val.as_str() == Some(tag_key) {
                    map.get(&column.name()).and_then(val_to_label_value)
                } else {
                    None
                }
            }
            StorageMapping::Virtual => None,
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
            .find_map(|s| map.get(&s.name()).and_then(val_to_label_value))
        };

        let label_val = self
            .look_up(tag_type)
            .and_then(from_storage)
            .or_else(from_fallback)?;

        Some(Label::resolve(tag_type.clone(), label_val))
    }

    pub fn resolve_label(&self, tag_type: &TagType, value: &Value) -> Label {
        let label_val = val_to_label_value(value)
            .unwrap_or(LabelValue::String(String::new()));
        Label::resolve(tag_type.clone(), label_val)
    }
}

fn val_to_label_value(val: &Value) -> Option<LabelValue> {
    match val {
        Value::Text(s) => Some(LabelValue::String(s.clone())),
        Value::BigInt(i) => Some(LabelValue::Integer(*i)),
        Value::Double(d) => Some(LabelValue::String(d.to_string())),
        Value::Boolean(b) => Some(LabelValue::Boolean(*b)),
        _ => None,
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

fn expand_query_node(
    lens: &Lens,
    node: crate::query::ast::QueryNode,
) -> anyhow::Result<crate::query::ast::QueryNode> {
    use crate::query::ast::QueryNode;
    match node {
        QueryNode::TypedTag(tt) => {
            if let Some(desc) = lens.look_up(&tt.label.tag_type()) {
                if let Some(func) = &desc.logical_function {
                    return Ok(func.expand(&tt.label));
                }
            }
            Ok(QueryNode::TypedTag(tt))
        }
        QueryNode::Projection(tagtype) => {
            if let Some(desc) = lens.look_up(&tagtype) {
                if let Some(func) = &desc.logical_function {
                    return Ok(func.expand_projection(tagtype.clone()));
                }
            }
            Ok(QueryNode::Projection(tagtype))
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
            let reg = QueryFunctionRegistry::with_standard();
            let expanded_node: crate::query::ast::QueryNode = crate::query::functions::expand_comparison_node(cmp, &reg);
            Ok(expanded_node)
        }
        other => Ok(other),
    }
}


fn resolve_query_node(
    lens: &Lens,
    node: crate::query::ast::QueryNode,
) -> anyhow::Result<ResolvedNode> {
    use crate::query::ast::QueryNode;
    match node {
        QueryNode::TypedTag(tt) => {
            let tag_type = tt.label.tag_type();
            let (storage, sql_type) = match lens.look_up(&tag_type) {
                Some(desc) => (desc.storage.clone(), desc.sql_type),
                None => (
                    StorageMapping::RowTag {
                        column: crate::db::Col::LabelStr,
                        tag_key: tag_type.as_str().to_string(),
                    },
                    crate::db::SqlType::VARCHAR,
                ),
            };
            Ok(ResolvedNode::Match {
                tag_type,
                storage,
                sql_type,
                op: ComparisonOp::Eq,
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
                sql_type: desc.sql_type,
                op: ComparisonOp::Eq,
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
        QueryNode::Projection(tt) => {
            let storage = match lens.look_up(&tt) {
                Some(desc) => desc.storage.clone(),
                None => StorageMapping::RowTag {
                    column: crate::db::Col::LabelStr,
                    tag_key: tt.as_str().to_string(),
                },
            };
            Ok(ResolvedNode::Projection(tt, storage))
        }
    }
}

fn resolve_comparison(
    lens: &Lens,
    cmp: crate::query::ast::ComparisonNode,
) -> anyhow::Result<ResolvedNode> {
    let mut nodes = Vec::new();
    let mut current_left = cmp.first;

    for (op, right) in cmp.rest {
        nodes.push(resolve_single_match(
            lens,
            current_left,
            op,
            right.clone(),
        )?);
        current_left = right;
    }

    if nodes.len() == 1 {
        Ok(nodes.pop().unwrap())
    } else {
        Ok(ResolvedNode::And(nodes))
    }
}

fn resolve_single_match(
    lens: &Lens,
    left: crate::query::ast::Operand,
    op: crate::query::ast::ComparisonOp,
    right: crate::query::ast::Operand,
) -> anyhow::Result<ResolvedNode> {
    use crate::query::ast::Operand;

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
        _ => Err(anyhow::anyhow!("Unsupported comparison pattern")),
    }
}

fn get_storage_and_type(
    lens: &Lens,
    tt: &crate::types::TagType,
) -> (StorageMapping, crate::db::SqlType) {
    match lens.look_up(tt) {
        Some(desc) => (desc.storage.clone(), desc.sql_type),
        None => (
            StorageMapping::RowTag {
                column: crate::db::Col::LabelStr,
                tag_key: tt.as_str().to_string(),
            },
            crate::db::SqlType::VARCHAR,
        ),
    }
}

pub fn flip_op(
    op: crate::query::ast::ComparisonOp,
) -> crate::query::ast::ComparisonOp {
    match op {
        ComparisonOp::Gt => ComparisonOp::Lt,
        ComparisonOp::Ge => ComparisonOp::Le,
        ComparisonOp::Lt => ComparisonOp::Gt,
        ComparisonOp::Le => ComparisonOp::Ge,
        other => other,
    }
}

/// ComparisonOp を sea_query の BinOper に変換します。
pub fn to_bin_op(op: ComparisonOp) -> BinOper {
    match op {
        ComparisonOp::Eq => BinOper::Equal,
        ComparisonOp::Ne => BinOper::NotEqual,
        ComparisonOp::Gt => BinOper::GreaterThan,
        ComparisonOp::Ge => BinOper::GreaterThanOrEqual,
        ComparisonOp::Lt => BinOper::SmallerThan,
        ComparisonOp::Le => BinOper::SmallerThanOrEqual,
    }
}

// --- 純粋関数 (初期化用データ定義) ---

fn base_column_descriptors() -> Vec<TagDescriptor> {
    let cols = vec![
        (SType::ItemId, Col::ItemId),
        (SType::Rank, Col::Rank),
        (SType::Origin, Col::Origin),
        (SType::ItemKind, Col::ItemKind),
        (SType::Type, Col::Type),
        (SType::TypedTag, Col::TypedTag),
        (SType::Label, Col::LabelStr),
        (SType::ScanHash, Col::ScanHash),
    ];

    cols.into_iter()
        .map(|(stype, col)| TagDescriptor {
            tag_type: TagType::Base(stype),
            storage: StorageMapping::Column(col),
            sql_type: col.sql_type(),
            logical_function: None,
        })
        .collect()
}

fn row_tag_descriptors() -> Vec<TagDescriptor> {
    let tags = vec![
        (SType::Path, Col::LabelStr),
        (SType::Parentdir, Col::LabelStr),
        (SType::Stem, Col::LabelStr),
        (SType::Extension, Col::LabelStr),
        (SType::IsDir, Col::LabelBool),
        (SType::Size, Col::LabelInt),
        (SType::Mtime, Col::LabelInt),
        (SType::Hash, Col::LabelStr),
        (SType::Content, Col::LabelStr),
        (SType::Name, Col::LabelStr),
        (SType::TypeFromExt, Col::LabelStr),
        (SType::SizeStr, Col::LabelStr),
        (SType::ModifiedStr, Col::LabelStr),
        (SType::FileId, Col::LabelStr),
    ];

    tags.into_iter()
        .map(|(stype, col)| {
            let key: &'static str = stype.into();
            TagDescriptor {
                tag_type: TagType::Base(stype),
                storage: StorageMapping::RowTag {
                    column: col,
                    tag_key: key.to_string(),
                },
                sql_type: col.sql_type(),
                logical_function: None,
            }
        })
        .chain(std::iter::once(TagDescriptor {
            tag_type: TagType::Base(SType::Filename),
            storage: StorageMapping::RowTag {
                column: Col::LabelStr,
                tag_key: "name".to_string(),
            },
            sql_type: crate::db::SqlType::VARCHAR,
            logical_function: Some(Arc::new(FilenameQuery)),
        }))
        .collect()
}

fn virtual_tag_descriptors() -> Vec<TagDescriptor> {
    let v_tags: Vec<(SType, Arc<dyn QueryFunction>)> = vec![
        (SType::Directory, Arc::new(DirectoryQuery)),
        (SType::Extension, Arc::new(ExtensionQuery)),
        (SType::Path, Arc::new(PathQuery)),
        (SType::Parentdir, Arc::new(ParentDirQuery)),
        (SType::Size, Arc::new(SizeQuery)),
        (SType::Mtime, Arc::new(MtimeQuery)),
        (SType::ItemKind, Arc::new(ItemKindQuery)),
        (SType::Rank, Arc::new(RankQuery)),
        (SType::Origin, Arc::new(OriginQuery)),
        (SType::Type, Arc::new(TypeQuery)),
        (SType::Label, Arc::new(LabelQuery)),
        (SType::TypedTag, Arc::new(TypedTagQuery)),
    ];

    v_tags
        .into_iter()
        .map(|(stype, func)| TagDescriptor {
            tag_type: TagType::Base(stype),
            storage: StorageMapping::Virtual,
            // Virtual タグの型は一概に言えないが、検索時は文字列として扱われることが多い
            // 必要に応じてマッピングを変えるが、デフォルトは VARCHAR
            sql_type: crate::db::SqlType::VARCHAR,
            logical_function: Some(func),
        })
        .collect()
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
        assert_eq!(found.storage, StorageMapping::Column(Col::Rank));
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_origin() {
        let lens = Lens::base_standard();
        let found = lens.look_up(&TagType::Base(SType::Origin)).unwrap();
        assert_eq!(found.storage, StorageMapping::Column(Col::Origin));
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_size() {
        let lens = Lens::base_standard();
        let found = lens.look_up(&TagType::Base(SType::Size)).unwrap();
        if let StorageMapping::RowTag { column, tag_key } = &found.storage {
            assert_eq!(*column, Col::LabelInt);
            assert_eq!(tag_key, "size");
        } else {
            panic!("Expected RowTag mapping for size, got {:?}", found.storage);
        }
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_directory_as_virtual() {
        let lens = Lens::base_standard();
        let found = lens.look_up(&TagType::Base(SType::Directory)).unwrap();
        assert_eq!(found.storage, StorageMapping::Virtual);
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
        // Virtual が最後に登録され、かつマージにより以前の RowTag を上書きしない（関数だけ上書き）
        // ...はずだが、今回の実装では descriptor.storage != Virtual の時だけ物理をを優先。
        // Filename は RowTag -> Virtual の順に登録される。
        // RowTag 登録時: storage=RowTag
        // Virtual 登録時: storage=Virtual なので existing.storage は更新されない。
        // 結果、物理情報（RowTag）を保持しつつ論理関数を持つ。
        if let StorageMapping::RowTag { tag_key, .. } = &found.storage {
            assert_eq!(tag_key, "name");
        } else {
            panic!("Expected RowTag for filename, got {:?}", found.storage);
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
            SType::TypeFromExt,
            SType::SizeStr,
            SType::ModifiedStr,
            SType::Directory,
            SType::Name,
            SType::ScanHash,
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
}

#[cfg(test)]
mod helper_tests {
    use super::*;
    use sea_query::BinOper;

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
            crate::db::SqlType::VARCHAR,
            true,
        );
        let debug_str = format!("{:?}", cond);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_decode_label_with_virtual_fallback() {
        let lens = Lens::base_standard();
        // SType::Content は定義上 DefinitionOnly or Virtual だが、
        // 物理的に "label_str" に値が入っていれば取得できるべき

        let tag_type = TagType::Base(SType::Content);

        // 擬似的な DuckDB の Row 構造 (STRUCT)
        use duckdb::types::OrderedMap;
        let map = OrderedMap::from(vec![(
            "label_str".to_string(),
            Value::Text("some_content".to_string()),
        )]);
        let _map_val = Value::Struct(map.clone());
        let decoded = lens.decode_label_from_map(&tag_type, &map);
        assert!(decoded.is_some());

        let label = decoded.unwrap();
        // Other(Content, String("some_content")) になるはず
        assert_eq!(label.tag_type(), tag_type);
        assert_eq!(label.as_str(), "some_content");
    }
}
