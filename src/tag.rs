use crate::indexing::functions::ScanRole;
use crate::query::ast::{
    BasicOp, ComparisonNode, ComparisonOp, Operand, QueryNode,
};
use crate::query::logical_resolver::LogicalType;
use crate::types::{Label, LabelValue, Rank, SType, TagType, TypedTag};
use anyhow::Result;
use path_slash::PathExt as _;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

// ============================================================
// DisplayFormat / DisplayFormats
// ============================================================

/// タグ値の表示フォーマット定義。
pub struct DisplayFormat {
    pub id: &'static str,
    pub label: &'static str,
}

impl Default for DisplayFormat {
    fn default() -> Self {
        DisplayFormat {
            id: "raw",
            label: "Raw",
        }
    }
}

/// 利用可能な表示フォーマット一覧とデフォルト。
pub struct DisplayFormats {
    pub default: DisplayFormat,
    pub options: Vec<DisplayFormat>,
}

impl Default for DisplayFormats {
    fn default() -> Self {
        Self {
            default: DisplayFormat::default(),
            options: vec![],
        }
    }
}

// ============================================================
// Index trait
// ============================================================

/// インデックス時のファイルからタグ値を抽出するロジック。
pub trait Index: Send + Sync {
    fn role(&self) -> ScanRole {
        ScanRole::Other
    }

    fn extract(&self, path: &Path) -> Result<Option<LabelValue>>;

    /// パスのみから値を生成できる場合に返す（移動検知など）。
    fn extract_from_path(&self, _path: &Path) -> Option<LabelValue> {
        None
    }

    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::DEFAULT
    }
}

// ============================================================
// LogicalRole
// ============================================================

/// タグの論理的な格納役割。Lens が TagDescriptor を自動生成するために使用。
pub enum LogicalRole {
    /// 他タグへ展開する論理タグ。物理ストレージなし（例: directory:）。
    Composite,
    /// 汎用行テーブルに格納。デフォルト。プラグイン定義タグも含む。
    Basic,
    /// 専用 DB カラム（rank, origin 等）。宣言のみ。カラム定義は base_column_descriptors が管理。
    Fixed,
}

// ============================================================
// Query trait
// ============================================================

/// クエリ展開と正規化のロジック。
pub trait Query: Send + Sync {
    /// タグを QueryNode へ展開する。デフォルトはそのまま TypedTag。
    fn expand(
        &self,
        _tagtype: &TagType,
        _label: &Label,
        tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::TypedTag(tag.clone())
    }

    /// Projection（type:形式）を QueryNode へ展開する。
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }

    /// ラベル値を正規化する（例: "1MB" → 1048576）。
    fn normalize_label(&self, label: &Label) -> Label {
        label.clone()
    }

    /// タグのストレージ上の役割。Lens が Descriptor を自動生成するために使用。
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Basic
    }

    /// タグ値の論理型。算術演算の型チェックとカラム選択に使用。デフォルトは Any。
    fn logical_type(&self) -> LogicalType {
        LogicalType::Any
    }

    /// DB の `type` カラムに格納されるキー。None = タグ名をそのまま使用。
    /// FilenameFn のみ Some("name") を返す（旧来の DB キー互換）。
    fn storage_key(&self) -> Option<&'static str> {
        None
    }
}

// ============================================================
// Display trait
// ============================================================

/// タグ値の表示フォーマット変換ロジック。
/// `show` メソッド名は std::fmt::Display::fmt との混同を避けるため採用。
pub trait Display: Send + Sync {
    fn formats(&self) -> DisplayFormats {
        DisplayFormats::default()
    }

    fn show(&self, value: &LabelValue, format: DisplayFormat) -> String;
}

// ============================================================
// TagFunction trait
// ============================================================

/// タグの統合定義単位。Index・Query・Display の3コンポーネントを束ねる。
pub trait TagFunction: Send + Sync {
    fn name(&self) -> &str;

    fn index(&self) -> Option<&dyn Index> {
        None
    }

    fn query(&self) -> Option<&dyn Query> {
        None
    }

    fn display(&self) -> Option<&dyn Display> {
        None
    }
}

// ============================================================
// TagRegistry
// ============================================================

/// TagFunction を一括管理するレジストリ。
pub struct TagRegistry {
    functions: HashMap<String, Arc<dyn TagFunction>>,
    /// Phase 4 で削除予定: インデックス側ブリッジ
    inner: crate::FunctionRegistry,
}

impl Default for TagRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TagRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
            inner: crate::FunctionRegistry::new(),
        }
    }

    pub fn register(&mut self, func: impl TagFunction + 'static) {
        self.functions
            .insert(func.name().to_string(), Arc::new(func));
    }

    pub fn get(&self, name: &str) -> Option<&dyn TagFunction> {
        self.functions.get(name).map(|f| f.as_ref())
    }

    pub fn get_arc(&self, name: &str) -> Option<Arc<dyn TagFunction>> {
        self.functions.get(name).cloned()
    }

    pub fn iter_arcs(
        &self,
    ) -> impl Iterator<Item = Arc<dyn TagFunction>> + '_ {
        self.functions.values().cloned()
    }

    /// 標準タグを全登録したレジストリを返す。
    pub fn with_standard() -> Self {
        let mut reg = Self::new();
        reg.register(DirectoryFn);
        reg.register(FilenameFn);
        reg.register(ExtensionFn);
        reg.register(PathFn);
        reg.register(ParentDirFn);
        reg.register(StemFn);
        reg.register(IsDirFn);
        reg.register(HashFn);
        reg.register(ContentFn);
        reg.register(FileIdFn);
        reg.register(NameFn);
        reg.register(SizeFn);
        reg.register(MtimeFn);
        reg.register(ItemKindFn);
        reg.register(RankFn);
        reg.register(OriginFn);
        reg.register(TypeFn);
        reg.register(LabelFn);
        reg.register(TypedTagFn);
        reg.inner = crate::FunctionRegistry::with_standard();
        reg
    }

    // ---- Phase 4 削除予定ブリッジメソッド ----

    pub fn get_all_columns(&self) -> Vec<crate::taggers::ColumnDef> {
        self.inner.get_all_columns()
    }

    pub fn process_file(
        &self,
        path: &Path,
    ) -> Result<Vec<crate::taggers::TagValue>> {
        self.inner.process_file(path)
    }

    pub fn all_indexing_functions(
        &self,
    ) -> &[Box<dyn crate::indexing::functions::IndexingFunction>] {
        self.inner.all_functions()
    }

    pub fn register_plugin(
        &mut self,
        func: Box<dyn crate::indexing::functions::IndexingFunction>,
    ) {
        self.inner.register(func);
    }
}

// ============================================================
// ttql_parse / ttql!
// ============================================================

/// TTQL クエリ文字列を QueryNode に変換する。
/// パースエラー時はパニックする（expand() 内での使用を想定）。
pub fn ttql_parse(query: &str) -> QueryNode {
    crate::parse(query).unwrap_or_else(|e| panic!("{e}"))
}

/// format!() を内包する ttql_parse 呼び出しマクロ。
/// 例: ttql!("is_dir:false & {}", tag_name)
#[macro_export]
macro_rules! ttql {
    ($($arg:tt)*) => {
        $crate::tag::ttql_parse(&::std::format!($($arg)*))
    };
}

// ============================================================
// Standard tag implementations
// ============================================================

fn normalize_path_str(s: &str) -> String {
    Path::new(s).to_slash_lossy().to_string()
}

pub(crate) struct DirectoryFn;
impl TagFunction for DirectoryFn {
    fn name(&self) -> &str {
        SType::Directory.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for DirectoryFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Composite
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::Filename, label.clone())),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, true)),
        ])
    }
    fn expand_projection(&self, _tt: &TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, true)),
            QueryNode::Projection(Operand::from(TagType::Base(SType::Filename))),
        ])
    }
}

pub(crate) struct FilenameFn;
impl TagFunction for FilenameFn {
    fn name(&self) -> &str {
        SType::Filename.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for FilenameFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn storage_key(&self) -> Option<&'static str> {
        Some("name")
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::Filename, label.clone())),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
        ])
    }
    fn expand_projection(&self, _tt: &TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
            QueryNode::Projection(Operand::from(TagType::Base(SType::Filename))),
        ])
    }
}

pub(crate) struct ExtensionFn;
impl TagFunction for ExtensionFn {
    fn name(&self) -> &str {
        SType::Extension.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for ExtensionFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn normalize_label(&self, label: &Label) -> Label {
        let s = label.as_str().to_lowercase();
        let s = s.trim_start_matches('.');
        Label::Extension(s.to_string())
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        let label = self.normalize_label(label);
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::Extension, label)),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
        ])
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
            QueryNode::Projection(Operand::from(tagtype.clone())),
        ])
    }
}

pub(crate) struct PathFn;
impl TagFunction for PathFn {
    fn name(&self) -> &str {
        SType::Path.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for PathFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        let normalized = normalize_path_str(&label.as_str());
        let lv = match label.value() {
            LabelValue::Literal(_) => LabelValue::Literal(normalized),
            _ => LabelValue::String(normalized),
        };
        QueryNode::TypedTag(TypedTag::new(SType::Path, lv))
    }
}

pub(crate) struct ParentDirFn;
impl TagFunction for ParentDirFn {
    fn name(&self) -> &str {
        SType::Parentdir.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for ParentDirFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        let normalized = normalize_path_str(&label.as_str());
        let lv = match label.value() {
            LabelValue::Literal(_) => LabelValue::Literal(normalized),
            _ => LabelValue::String(normalized),
        };
        QueryNode::TypedTag(TypedTag::new(SType::Parentdir, lv))
    }
}

pub(crate) struct StemFn;
impl TagFunction for StemFn {
    fn name(&self) -> &str {
        SType::Stem.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for StemFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

pub(crate) struct IsDirFn;
impl TagFunction for IsDirFn {
    fn name(&self) -> &str {
        SType::IsDir.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for IsDirFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Boolean
    }
}

pub(crate) struct HashFn;
impl TagFunction for HashFn {
    fn name(&self) -> &str {
        SType::Hash.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for HashFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

pub(crate) struct ContentFn;
impl TagFunction for ContentFn {
    fn name(&self) -> &str {
        SType::Content.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for ContentFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

pub(crate) struct FileIdFn;
impl TagFunction for FileIdFn {
    fn name(&self) -> &str {
        SType::FileId.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for FileIdFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

pub(crate) struct NameFn;
impl TagFunction for NameFn {
    fn name(&self) -> &str {
        SType::Name.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for NameFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new(SType::Name, label.clone()))
    }
}

pub(crate) struct SizeFn;
impl TagFunction for SizeFn {
    fn name(&self) -> &str {
        SType::Size.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for SizeFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn normalize_label(&self, label: &Label) -> Label {
        if let Some(bytes) = crate::util::parse_size(&label.as_str()) {
            Label::Size(bytes)
        } else {
            label.clone()
        }
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        let label = self.normalize_label(label);
        QueryNode::TypedTag(TypedTag::new(TagType::from(SType::Size), label))
    }
}

pub(crate) struct MtimeFn;
impl TagFunction for MtimeFn {
    fn name(&self) -> &str {
        SType::Mtime.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for MtimeFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        if let Some(range) = crate::util::parse_datetime(&label.as_str()) {
            if range.start == range.end {
                QueryNode::TypedTag(TypedTag::new(SType::Mtime, range.start))
            } else {
                QueryNode::And(vec![
                    QueryNode::Comparison(ComparisonNode {
                        first: Operand::TypeRef(SType::Mtime.into()),
                        rest: vec![(
                            ComparisonOp::Label(BasicOp::Ge),
                            Operand::Literal(Label::Mtime(range.start)),
                        )],
                    }),
                    QueryNode::Comparison(ComparisonNode {
                        first: Operand::TypeRef(SType::Mtime.into()),
                        rest: vec![(
                            ComparisonOp::Label(BasicOp::Le),
                            Operand::Literal(Label::Mtime(range.end)),
                        )],
                    }),
                ])
            }
        } else {
            QueryNode::TypedTag(TypedTag::new(SType::Mtime, label.clone()))
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}

pub(crate) struct ItemKindFn;
impl TagFunction for ItemKindFn {
    fn name(&self) -> &str {
        SType::ItemKind.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for ItemKindFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::ItemKind,
            label: label.clone(),
        }
    }
}

pub(crate) struct RankFn;
impl TagFunction for RankFn {
    fn name(&self) -> &str {
        SType::Rank.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for RankFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new(SType::Rank, label.clone()))
    }
}

pub(crate) struct OriginFn;
impl TagFunction for OriginFn {
    fn name(&self) -> &str {
        SType::Origin.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for OriginFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Origin,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}

pub(crate) struct TypeFn;
impl TagFunction for TypeFn {
    fn name(&self) -> &str {
        SType::Type.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for TypeFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Type,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}

pub(crate) struct LabelFn;
impl TagFunction for LabelFn {
    fn name(&self) -> &str {
        SType::Label.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for LabelFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Label,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}

pub(crate) struct TypedTagFn;
impl TagFunction for TypedTagFn {
    fn name(&self) -> &str {
        SType::TypedTag.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for TypedTagFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::TypedTag,
            label: label.clone(),
        }
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::functions::ScanRole;
    use std::path::Path;

    struct SimpleTag;
    impl TagFunction for SimpleTag {
        fn name(&self) -> &str {
            "simple"
        }
    }

    struct IndexedTag;
    impl TagFunction for IndexedTag {
        fn name(&self) -> &str {
            "indexed"
        }
        fn index(&self) -> Option<&dyn Index> {
            Some(self)
        }
    }
    impl Index for IndexedTag {
        fn extract(&self, _path: &Path) -> Result<Option<LabelValue>> {
            Ok(Some(LabelValue::String("value".to_string())))
        }
        fn role(&self) -> ScanRole {
            ScanRole::Location
        }
        fn default_rank(&self) -> Rank {
            42
        }
    }

    struct QueryTag;
    impl TagFunction for QueryTag {
        fn name(&self) -> &str {
            "qtest"
        }
        fn query(&self) -> Option<&dyn Query> {
            Some(self)
        }
    }
    impl Query for QueryTag {
        fn normalize_label(&self, label: &Label) -> Label {
            Label::Other(
                TagType::from("qtest"),
                LabelValue::String(label.as_str().to_uppercase()),
            )
        }
    }

    // --- TagRegistry ---

    #[test]
    fn test_registry_register_and_get() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        assert!(reg.get("simple").is_some());
        assert!(reg.get("unknown").is_none());
    }

    #[test]
    fn test_registry_overwrite() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        reg.register(SimpleTag);
        assert!(reg.get("simple").is_some());
    }

    #[test]
    fn test_registry_get_arc() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        let arc = reg.get_arc("simple");
        assert!(arc.is_some());
    }

    #[test]
    fn test_registry_iter_arcs() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        let count = reg.iter_arcs().count();
        assert_eq!(count, 1);
    }

    // --- TagFunction defaults ---

    #[test]
    fn test_tag_function_defaults() {
        assert!(SimpleTag.index().is_none());
        assert!(SimpleTag.query().is_none());
        assert!(SimpleTag.display().is_none());
    }

    // --- Index ---

    #[test]
    fn test_index_role_and_rank() {
        let f = IndexedTag;
        let idx = f.index().unwrap();
        assert_eq!(idx.role(), ScanRole::Location);
        assert_eq!(idx.default_rank(), 42);
    }

    #[test]
    fn test_index_extract() {
        let result = IndexedTag.extract(Path::new("/dummy")).unwrap();
        assert_eq!(result, Some(LabelValue::String("value".to_string())));
    }

    #[test]
    fn test_index_default_extract_from_path() {
        let result = IndexedTag.extract_from_path(Path::new("/dummy"));
        assert!(result.is_none());
    }

    // --- Query ---

    #[test]
    fn test_query_normalize_label() {
        let q = QueryTag;
        let qry = q.query().unwrap();
        let label = Label::Other(
            TagType::from("qtest"),
            LabelValue::String("hello".to_string()),
        );
        let normalized = qry.normalize_label(&label);
        assert_eq!(normalized.as_str(), "HELLO");
    }

    // --- DisplayFormat / DisplayFormats ---

    #[test]
    fn test_display_format_default() {
        let fmt = DisplayFormat::default();
        assert_eq!(fmt.id, "raw");
        assert_eq!(fmt.label, "Raw");
    }

    #[test]
    fn test_display_formats_default() {
        let fmts = DisplayFormats::default();
        assert_eq!(fmts.default.id, "raw");
        assert!(fmts.options.is_empty());
    }

    // --- ttql_parse / ttql! ---

    #[test]
    fn test_ttql_parse_basic() {
        let node = ttql_parse("extension:rs");
        match node {
            QueryNode::TypedTag(_) | QueryNode::And(_) => {}
            other => panic!("Unexpected QueryNode: {:?}", other),
        }
    }

    #[test]
    fn test_ttql_macro() {
        let tag = "extension";
        let node = crate::ttql!("{tag}:rs");
        match node {
            QueryNode::TypedTag(_) | QueryNode::And(_) => {}
            other => panic!("Unexpected QueryNode: {:?}", other),
        }
    }
}
