use crate::db::Col;
use crate::query::ast::{ComparisonNode, ComparisonOp, Operand, QueryNode};
use crate::query::logical_schema::{LogicalSchema, LogicalType};
use crate::query::sql::schema_pieces;
use crate::tag::{LogicalRole, TagFunction};
use crate::types::{Label, LabelValue, SType, TagType};
use duckdb::types::Value;
use sea_query::{BinOper, Condition, SimpleExpr};
use indexmap::IndexMap;
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
                Some(build_lens_select_column(src, *col, ids_sql))
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
        sql_type: crate::db::SqlType,
    ) -> Condition {
        match self {
            StorageMapping::Fixed(col) => {
                build_column_condition(*col, op, label, sql_type, false)
            }
            StorageMapping::Basic { column, tag_type } => {
                Condition::all().add(check_tag_match(tag_type)).add(
                    build_column_condition(*column, op, label, sql_type, true),
                )
            }
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

fn build_column_condition(
    col: Col,
    op: ComparisonOp,
    label: &Label,
    sql_type: crate::db::SqlType,
    _is_generic_context: bool,
) -> Condition {
    let bin_op = to_bin_op(op);

    // 汎用ラベルカラムか？ (Basic タグの EAV カラム)
    let is_eav_col = col == Col::LabelStr
        || col == Col::LabelInt
        || col == Col::LabelDouble
        || col == Col::LabelBool;

    match label.value() {
        LabelValue::Integer(i) => {
            build_int_condition(col, bin_op, i, is_eav_col)
        }
        LabelValue::Boolean(b) => build_str_condition(
            col,
            bin_op,
            &b.to_string(),
            sql_type,
            is_eav_col,
        ),
        LabelValue::Double(bits) => {
            schema_pieces::build_double_condition(bin_op, bits)
        }
        LabelValue::Null => schema_pieces::build_null_condition(),
        LabelValue::String(s) => {
            build_str_condition(col, bin_op, &s, sql_type, is_eav_col)
        }
        LabelValue::Literal(s) => {
            build_literal_condition(col, bin_op, &s, is_eav_col)
        }
        LabelValue::Date(dt) => {
            build_int_condition(col, bin_op, dt.to_timestamp(), is_eav_col)
        }
    }
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
        .flatten()
        .fold(Condition::any(), |acc, cond| acc.add(cond))
}

pub(crate) fn check_string_match(
    col: Col,
    op: BinOper,
    val: &str,
    sql_type: crate::db::SqlType,
) -> Option<SimpleExpr> {
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
}

impl TagDescriptor {
    /// 論理型から物理型（SqlType）への一方向マッピングを提供します。
    pub fn logical_to_sql(lt: LogicalType) -> crate::db::SqlType {
        use crate::db::SqlType;
        match lt {
            LogicalType::Integer => SqlType::BIGINT,
            LogicalType::Float => SqlType::DOUBLE,
            LogicalType::String => SqlType::VARCHAR,
            LogicalType::Boolean => SqlType::BOOLEAN,
            LogicalType::Any => SqlType::VARCHAR,
        }
    }

    /// このタグの物理型（SqlType）を返します。
    pub fn sql_type(&self) -> crate::db::SqlType {
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
                if let Some(q) = func.query() {
                    // normalize_label を適用してから expand する
                    let normalized = q.normalize_label(label);
                    let tag = crate::types::TypedTag::new(
                        tag_type.clone(),
                        normalized.clone(),
                    );
                    return q.expand(tag_type, &normalized, &tag);
                }
            }
        }
        QueryNode::TypedTag(crate::types::TypedTag::new(
            tag_type.clone(),
            label.clone(),
        ))
    }

    fn expand_projection(&self, tag_type: &TagType) -> QueryNode {
        if let Some(desc) = self.look_up(tag_type) {
            if let Some(func) = &desc.logical_function {
                if let Some(q) = func.query() {
                    return q.expand_projection(tag_type);
                }
            }
        }
        QueryNode::Projection(Operand::TypeRef(tag_type.clone()))
    }

    fn normalize_label_any(&self, label: &Label) -> Label {
        if matches!(label.value(), LabelValue::Literal(_)) {
            return label.clone();
        }
        // TagRegistry と同様に登録の逆順で走査する
        for desc in self.registry.values().rev() {
            if let Some(func) = &desc.logical_function {
                if let Some(q) = func.query() {
                    let normalized = q.normalize_label(label);
                    if normalized != *label {
                        return normalized;
                    }
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
        let Some(q) = func.query() else {
            return QueryNode::Comparison(node);
        };
        q.expand_comparison(node)
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
            let Some(q) = func.query() else { continue };
            let tag_type = TagType::from(func.name());
            let key = q.storage_key().unwrap_or(func.name()).to_string();
            let desc = match q.logical_role() {
                LogicalRole::Composite => TagDescriptor {
                    tag_type,
                    storage: StorageMapping::Composite,
                    logical_type: q.logical_type(),
                    logical_function: Some(func.clone()),
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
                    }
                }
                LogicalRole::Fixed => TagDescriptor {
                    // 物理ストレージは base_column_descriptors で登録済み
                    // Composite として登録することで Fixed 定義を上書きしない
                    tag_type,
                    storage: StorageMapping::Composite,
                    logical_type: q.logical_type(),
                    logical_function: Some(func.clone()),
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
                map.get(&col.name()).and_then(val_to_label_value)
            }
            StorageMapping::Basic { column, tag_type } => {
                let type_val = map.get(&SType::Type.name())?;
                if type_val.as_str() == Some(tag_type) {
                    map.get(&column.name()).and_then(val_to_label_value)
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
        Value::Int(i) => Some(LabelValue::Integer(*i as i64)),
        Value::Float(f) => Some(LabelValue::Double((*f as f64).to_bits())),
        Value::Double(d) => Some(LabelValue::Double(d.to_bits())),
        Value::Boolean(b) => Some(LabelValue::Boolean(*b)),
        Value::Null => Some(LabelValue::Null),
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

fn logical_type_to_col(lt: LogicalType) -> Col {
    use crate::db::SqlType;
    match TagDescriptor::logical_to_sql(lt) {
        SqlType::BIGINT => Col::LabelInt,
        SqlType::DOUBLE => Col::LabelDouble,
        SqlType::BOOLEAN => Col::LabelBool,
        _ => Col::LabelStr,
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
            storage: StorageMapping::Fixed(col),
            logical_type: sql_to_logical(col.sql_type()),
            logical_function: None,
        })
        .collect()
}

fn sql_to_logical(st: crate::db::SqlType) -> LogicalType {
    match st {
        crate::db::SqlType::BIGINT => LogicalType::Integer,
        crate::db::SqlType::DOUBLE => LogicalType::Float,
        crate::db::SqlType::VARCHAR => LogicalType::String,
        crate::db::SqlType::BOOLEAN => LogicalType::Boolean,
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
            assert_eq!(tag_type, "name");
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

    #[test]
    fn test_logical_to_sql_mapping() {
        assert_eq!(
            TagDescriptor::logical_to_sql(LogicalType::Integer),
            crate::db::SqlType::BIGINT
        );
        assert_eq!(
            TagDescriptor::logical_to_sql(LogicalType::Float),
            crate::db::SqlType::DOUBLE
        );
        assert_eq!(
            TagDescriptor::logical_to_sql(LogicalType::String),
            crate::db::SqlType::VARCHAR
        );
        assert_eq!(
            TagDescriptor::logical_to_sql(LogicalType::Boolean),
            crate::db::SqlType::BOOLEAN
        );
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
