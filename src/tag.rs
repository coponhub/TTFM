use crate::indexing::functions::ScanRole;
use crate::query::ast::{Operand, QueryNode};
use crate::types::{Label, LabelValue, Rank, TagType, TypedTag};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

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
    functions: HashMap<String, Box<dyn TagFunction>>,
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
        }
    }

    pub fn register(&mut self, func: impl TagFunction + 'static) {
        self.functions
            .insert(func.name().to_string(), Box::new(func));
    }

    pub fn get(&self, name: &str) -> Option<&dyn TagFunction> {
        self.functions.get(name).map(|f| f.as_ref())
    }

    /// 標準タグを全登録したレジストリを返す（Phase 2 で実装）。
    pub fn with_standard() -> Self {
        Self::new()
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
