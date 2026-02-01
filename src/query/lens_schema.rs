use crate::db::Col;
use crate::query::ast::{ComparisonOp, QueryNode};
use crate::query::functions::*;
use crate::query::logical_resolver::{LogicalSchema, LogicalType};
use crate::query::QueryFunction;
use crate::types::{Label, LabelValue, SType, TagType};
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

pub(crate) fn check_tag_match(tag_key: &str) -> SimpleExpr {
    let mut tag_op = BinOper::Equal;
    if tag_key.contains('*') || tag_key.contains('?') || tag_key.contains('[') {
        tag_op = BinOper::Custom("GLOB");
    }
    Expr::col(crate::db::Col::Type).binary(tag_op, tag_key)
}

pub(crate) fn build_column_condition(
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

pub(crate) fn build_int_condition(
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
        .flatten() // None を除去
        .fold(Condition::any(), |acc, cond| acc.add(cond))
}

pub(crate) fn check_string_match(
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

pub(crate) fn try_parse_generic_value_as_cond(
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

pub(crate) fn build_literal_condition(
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
    pub logical_type: LogicalType,
    pub logical_function: Option<Arc<dyn QueryFunction>>,
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
    registry: HashMap<TagType, TagDescriptor>,
}

impl LogicalSchema for Lens {
    fn get_logical_type(&self, tag: &TagType) -> LogicalType {
        self.look_up_or_default(tag).logical_type
    }

    fn expand_tag(&self, tag_type: &TagType, label: &Label) -> QueryNode {
        if let Some(desc) = self.look_up(tag_type) {
            if let Some(func) = &desc.logical_function {
                return func.expand(label);
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
                return func.expand_projection(tag_type.clone());
            }
        }
        QueryNode::Projection(crate::query::ast::Operand::TypeRef(
            tag_type.clone(),
        ))
    }
}

impl Lens {
    /// 内部初期化用の一時的な空 Lens。外部からは通常 with_standard を使用してください。
    fn new_empty() -> Self {
        Self {
            registry: HashMap::new(),
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

    /// タグ定義を登録します。既存の定義がある場合はマージします。
    pub fn register(&mut self, descriptor: TagDescriptor) {
        if let Some(existing) = self.registry.get_mut(&descriptor.tag_type) {
            // 物理ストレージ定義があれば上書き（Virtual は物理を上書きしない）
            if descriptor.storage != StorageMapping::Virtual {
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
            logical_type: LogicalType::String,
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
            logical_type: sql_to_logical(col.sql_type()),
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
                logical_type: sql_to_logical(col.sql_type()),
                logical_function: None,
            }
        })
        .chain(std::iter::once(TagDescriptor {
            tag_type: TagType::Base(SType::Filename),
            storage: StorageMapping::RowTag {
                column: Col::LabelStr,
                tag_key: "name".to_string(),
            },
            logical_type: LogicalType::String,
            logical_function: Some(Arc::new(FilenameQuery)),
        }))
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
            // Virtual タグのデフォルトは文字列
            logical_type: LogicalType::String,
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
