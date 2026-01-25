use crate::query::ast::{Operand, QueryNode};
use crate::types::{Label, SType, TagType, TypedTag};
use path_slash::PathExt;
use std::collections::HashMap;
use std::path::Path;

/// 検索クエリの展開を行う抽象化単位。
pub trait QueryFunction: Send + Sync {
    /// この関数の名前（例: "directory", "filename"）
    fn name(&self) -> &str;
    /// タグを別のクエリ構造（QueryNode）へ展開します。
    fn expand(&self, label: &Label) -> QueryNode;

    /// ラベルの値を正規化します（例: "1MB" -> 1048576）。
    /// デフォルトでは元のラベルをそのまま返します。
    fn normalize_label(&self, label: &Label) -> Label {
        label.clone()
    }

    /// ラベル取得を基本構造へ展開します。
    fn expand_projection(&self, _tagtype: TagType) -> QueryNode {
        QueryNode::Projection(_tagtype)
    }
}

/// QueryFunction を管理するレジストリ。
pub struct QueryFunctionRegistry {
    functions: HashMap<String, Box<dyn QueryFunction>>,
}

impl Default for QueryFunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryFunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
        }
    }

    /// 標準的なクエリ展開関数を登録済みのレジストリを返します。
    pub fn with_standard() -> Self {
        use self::*;
        let mut reg = Self::new();
        reg.register(Box::new(DirectoryQuery));
        reg.register(Box::new(FilenameQuery));
        reg.register(Box::new(ExtensionQuery));
        reg.register(Box::new(PathQuery));
        reg.register(Box::new(ParentDirQuery));
        reg.register(Box::new(NameQuery));
        reg.register(Box::new(ItemKindQuery));
        reg.register(Box::new(RankQuery));
        reg.register(Box::new(SizeQuery));
        reg.register(Box::new(MtimeQuery));
        reg.register(Box::new(OriginQuery));
        reg.register(Box::new(TypeQuery));
        reg.register(Box::new(LabelQuery));
        reg.register(Box::new(TypedTagQuery));
        reg
    }

    pub fn register(&mut self, func: Box<dyn QueryFunction>) {
        self.functions.insert(func.name().to_string(), func);
    }

    /// タグを検索し、登録された関数があれば適用します。
    pub fn process_tag(&self, tagtype: TagType, label: Label) -> QueryNode {
        // LiteralCustom の場合は魔法（展開関数）をスキップする
        if let TagType::LiteralCustom(_) = tagtype {
            return QueryNode::And(vec![QueryNode::TypedTag(TypedTag {
                tagtype,
                label,
            })]);
        }

        // Baseタグ（SType）であれば、レジストリから展開関数を探す
        if let TagType::Base(stag) = tagtype {
            let key_str: &'static str = stag.into();
            if let Some(f) = self.functions.get(key_str) {
                return f.expand(&label);
            }
        }

        // それ以外（カスタムタグまたは未登録の標準タグ）はそのまま TypedTag として保持
        QueryNode::TypedTag(TypedTag { tagtype, label })
    }

    /// 指定された TagType に対応する QueryFunction を返します。
    pub fn get_function(
        &self,
        tagtype: &TagType,
    ) -> Option<&dyn QueryFunction> {
        if let TagType::Base(stag) = tagtype {
            let key_str: &'static str = (*stag).into();
            return self.functions.get(key_str).map(|f| f.as_ref());
        }
        None
    }

    pub fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        // LiteralCustom の場合は魔法（展開関数）をスキップする
        if let TagType::LiteralCustom(_) = tagtype {
            return QueryNode::Projection(tagtype);
        }

        // Baseタグ（SType）であれば、レジストリから展開関数を探す
        if let TagType::Base(stag) = tagtype {
            let key_str: &'static str = stag.into();
            if let Some(f) = self.functions.get(key_str) {
                return f.expand_projection(tagtype);
            }
        }

        // それ以外（カスタムタグまたは未登録の標準タグ）はそのまま Projection として保持
        QueryNode::Projection(tagtype)
    }
}

// Logic migrated from ComparisonNode impl in query.rs
pub fn expand_comparison_node(
    mut node: crate::query::ast::ComparisonNode,
    registry: &QueryFunctionRegistry,
) -> crate::query::ast::ComparisonNode {
    let mut rep_func = None;

    if let Operand::TypeRef(tt) = &node.first {
        rep_func = registry.get_function(tt);
    }

    if rep_func.is_none() {
        for (_, op) in &node.rest {
            if let Operand::TypeRef(tt) = op {
                if let Some(f) = registry.get_function(tt) {
                    rep_func = Some(f);
                    break;
                }
            }
        }
    }

    if let Some(f) = rep_func {
        if let Operand::Literal(label) = &mut node.first {
            *label = f.normalize_label(label);
        }
        for (_, op) in &mut node.rest {
            if let Operand::Literal(label) = op {
                *label = f.normalize_label(label);
            }
        }
    }

    node
}

pub fn expand_query_node(
    node: QueryNode,
    registry: &QueryFunctionRegistry,
) -> QueryNode {
    match node {
        QueryNode::And(nodes) => QueryNode::And(
            nodes
                .into_iter()
                .map(|n| expand_query_node(n, registry))
                .collect(),
        ),
        QueryNode::Or(nodes) => QueryNode::Or(
            nodes
                .into_iter()
                .map(|n| expand_query_node(n, registry))
                .collect(),
        ),
        QueryNode::Difference(l, r) => QueryNode::Difference(
            Box::new(expand_query_node(*l, registry)),
            Box::new(expand_query_node(*r, registry)),
        ),
        QueryNode::Complement(c) => {
            QueryNode::Complement(Box::new(expand_query_node(*c, registry)))
        }
        QueryNode::Comparison(cmp) => {
            QueryNode::Comparison(expand_comparison_node(cmp, registry))
        }
        QueryNode::ColumnMatch { tag, label } => {
            QueryNode::ColumnMatch { tag, label }
        }
        QueryNode::TypedTag(tt) => registry.process_tag(tt.tagtype, tt.label),
        QueryNode::Projection(tt) => registry.expand_projection(tt),
    }
}

/// "directory:name" -> "name:name & is_dir:true" への展開
pub struct DirectoryQuery;
impl QueryFunction for DirectoryQuery {
    fn name(&self) -> &str {
        SType::Directory.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::Filename).to_string(),
                label.clone(),
            )),
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::IsDir).to_string(),
                Label::String("true".to_string()),
            )),
        ])
    }
    fn expand_projection(&self, _tagtype: TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::IsDir).to_string(),
                Label::String("true".to_string()),
            )),
            QueryNode::Projection(SType::Filename.into()),
        ])
    }
}

/// "filename:name" -> "name:name & is_dir:false" への展開
pub struct FilenameQuery;
impl QueryFunction for FilenameQuery {
    fn name(&self) -> &str {
        SType::Filename.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::Filename).to_string(),
                label.clone(),
            )),
            // ディレクトリを除外
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::IsDir).to_string(),
                Label::String("false".to_string()),
            )),
        ])
    }
    fn expand_projection(&self, _tagtype: TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::IsDir).to_string(),
                Label::String("false".to_string()),
            )),
            QueryNode::Projection(SType::Filename.into()),
        ])
    }
}

/// "extension:RS" -> "extension:rs" (小文字化とドットの削除)
pub struct ExtensionQuery;
impl QueryFunction for ExtensionQuery {
    fn name(&self) -> &str {
        SType::Extension.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        let normalized = label
            .as_str()
            .to_lowercase()
            .trim_start_matches('.')
            .to_string();
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::Extension).to_string(),
                Label::String(normalized),
            )),
            // ディレクトリを除外 (拡張子はファイルのみ)
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::IsDir).to_string(),
                Label::String("false".to_string()),
            )),
        ])
    }
    fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::IsDir).to_string(),
                Label::String("false".to_string()),
            )),
            QueryNode::Projection(tagtype),
        ])
    }
}

/// パスの正規化を行う内部ヘルパー
fn normalize_path(path_str: &str) -> String {
    Path::new(path_str).to_slash_lossy().to_string()
}

/// "path:C:\foo" -> "path:C:/foo"
pub struct PathQuery;
impl QueryFunction for PathQuery {
    fn name(&self) -> &str {
        SType::Path.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        let normalized = normalize_path(&label.as_str());
        let new_label = match label {
            Label::Literal(_) => Label::Literal(normalized),
            _ => Label::String(normalized),
        };
        QueryNode::TypedTag(TypedTag::new(
            <&str>::from(SType::Path).to_string(),
            new_label,
        ))
    }
}

/// "parentdir:C:\foo" -> "parentdir:C:/foo"
pub struct ParentDirQuery;
impl QueryFunction for ParentDirQuery {
    fn name(&self) -> &str {
        SType::Parentdir.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        let normalized = normalize_path(&label.as_str());
        let new_label = match label {
            Label::Literal(_) => Label::Literal(normalized),
            _ => Label::String(normalized),
        };
        QueryNode::TypedTag(TypedTag::new(
            <&str>::from(SType::Parentdir).to_string(),
            new_label,
        ))
    }
}

/// "name:label" -> ColumnMatch(SType::Name, label)
pub struct NameQuery;
impl QueryFunction for NameQuery {
    fn name(&self) -> &str {
        SType::Name.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new("name", label.clone()))
    }
}

/// "item_kind:label" -> ColumnMatch(SType::ItemKind, label)
pub struct ItemKindQuery;
impl QueryFunction for ItemKindQuery {
    fn name(&self) -> &str {
        SType::ItemKind.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::ItemKind,
            label: label.clone(),
        }
    }
}

/// "rank:label" -> ColumnMatch(SType::Rank, label)
pub struct RankQuery;
impl QueryFunction for RankQuery {
    fn name(&self) -> &str {
        SType::Rank.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new("rank", label.clone()))
    }
}

/// "size:label" -> ColumnMatch(SType::Size, label)
pub struct SizeQuery;
impl QueryFunction for SizeQuery {
    fn name(&self) -> &str {
        SType::Size.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        let label = self.normalize_label(label);
        QueryNode::TypedTag(TypedTag::new(TagType::from(SType::Size), label))
    }
    fn normalize_label(&self, label: &Label) -> Label {
        match label {
            Label::String(s) | Label::Literal(s) => {
                if let Some(bytes) = crate::util::parse_size(s) {
                    Label::Integer(bytes)
                } else {
                    label.clone()
                }
            }
            _ => label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        QueryNode::Projection(tagtype)
    }
}

/// "mtime:label" -> ColumnMatch(SType::Mtime, label)
pub struct MtimeQuery;
impl QueryFunction for MtimeQuery {
    fn name(&self) -> &str {
        SType::Mtime.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new("mtime", label.clone()))
    }
    fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        QueryNode::Projection(tagtype)
    }
}

/// "origin:system/user" -> ColumnMatch(SType::Origin, label)
pub struct OriginQuery;
impl QueryFunction for OriginQuery {
    fn name(&self) -> &str {
        SType::Origin.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Origin,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        QueryNode::Projection(tagtype)
    }
}

/// "type:label" -> ColumnMatch(SType::Type, label)
pub struct TypeQuery;
impl QueryFunction for TypeQuery {
    fn name(&self) -> &str {
        SType::Type.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Type,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        QueryNode::Projection(tagtype)
    }
}

/// "label:value" -> ColumnMatch(SType::Label, label)
pub struct LabelQuery;
impl QueryFunction for LabelQuery {
    fn name(&self) -> &str {
        SType::Label.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Label,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        QueryNode::Projection(tagtype)
    }
}

/// "typedtag:label" -> item_kind:typedtag & name:label (タグ自体の検索)
pub struct TypedTagQuery;
impl QueryFunction for TypedTagQuery {
    fn name(&self) -> &str {
        SType::TypedTag.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::TypedTag,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, _tagtype: TagType) -> QueryNode {
        QueryNode::Projection(SType::TypedTag.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{ComparisonOp, Operand};
    use crate::types::{Label, TagType};

    struct MockSizeQuery;
    impl QueryFunction for MockSizeQuery {
        fn name(&self) -> &str {
            "size"
        }
        fn expand(&self, _label: &Label) -> QueryNode {
            QueryNode::And(vec![]) // Dummy
        }
        fn normalize_label(&self, label: &Label) -> Label {
            // Mock: multiply by 1024
            match label {
                Label::Integer(i) => Label::Integer(i * 1024),
                _ => label.clone(),
            }
        }
    }

    #[test]
    fn test_registry_registration() {
        let mut reg = QueryFunctionRegistry::new();
        reg.register(Box::new(MockSizeQuery));
        assert!(reg.get_function(&TagType::from("size")).is_some());
        assert!(reg.get_function(&TagType::from("unknown")).is_none());
    }

    #[test]
    fn test_expand_comparison_node_normalization() {
        let mut reg = QueryFunctionRegistry::new();
        reg.register(Box::new(MockSizeQuery));

        // size > 1
        let node = crate::query::ast::ComparisonNode {
            first: Operand::TypeRef(TagType::from("size")),
            rest: vec![(ComparisonOp::Gt, Operand::Literal(Label::Integer(1)))],
        };

        let expanded = expand_comparison_node(node, &reg);

        match &expanded.rest[0].1 {
            Operand::Literal(Label::Integer(val)) => assert_eq!(*val, 1024),
            _ => panic!("Expected Literal Integer"),
        }
    }

    #[test]
    fn test_expand_projection_mock() {
        struct MockProjQuery;
        impl QueryFunction for MockProjQuery {
            fn name(&self) -> &str {
                "size"
            } // Must be a valid SType (Base) to trigger expansion
            fn expand(&self, _: &Label) -> QueryNode {
                QueryNode::And(vec![])
            }
            // Mock: projection size -> projection rank
            fn expand_projection(&self, _: TagType) -> QueryNode {
                QueryNode::Projection(TagType::from("rank"))
            }
        }

        let mut reg = QueryFunctionRegistry::new();
        reg.register(Box::new(MockProjQuery));

        let tag = TagType::from("size");
        let expanded = reg.expand_projection(tag);

        if let QueryNode::Projection(t) = expanded {
            assert_eq!(t.as_str(), "rank");
        } else {
            panic!("Expected Projection rank, got {:?}", expanded);
        }
    }

    #[test]
    fn test_process_tag() {
        let mut reg = QueryFunctionRegistry::new();
        struct MockTagQuery;
        impl QueryFunction for MockTagQuery {
            fn name(&self) -> &str {
                "size"
            } // Use valid SType "size"
            fn expand(&self, _label: &Label) -> QueryNode {
                QueryNode::Comparison(crate::query::ast::ComparisonNode {
                    first: Operand::TypeRef(TagType::from("size")),
                    rest: vec![(
                        ComparisonOp::Gt,
                        Operand::Literal(Label::Integer(0)),
                    )],
                })
            }
        }
        reg.register(Box::new(MockTagQuery));

        // Registered ("size")
        let node = reg.process_tag(TagType::from("size"), Label::from("foo"));
        match node {
            QueryNode::Comparison(_) => {}
            _ => panic!("Expected Comparison from expansion, got {:?}", node),
        }

        // Unregistered
        let node2 =
            reg.process_tag(TagType::from("unknown"), Label::from("foo"));
        match node2 {
            QueryNode::TypedTag(tt) => {
                assert_eq!(tt.tagtype.as_str(), "unknown")
            }
            _ => panic!("Expected TypedTag for unknown, got {:?}", node2),
        }
    }

    #[test]
    fn test_expand_query_node_recursive() {
        let mut reg = QueryFunctionRegistry::new();
        struct MockRecursive;
        impl QueryFunction for MockRecursive {
            fn name(&self) -> &str {
                "name"
            } // Use valid SType "name"
            fn expand(&self, _: &Label) -> QueryNode {
                QueryNode::TypedTag(crate::types::TypedTag::new(
                    "expanded", "rec",
                ))
            }
        }
        reg.register(Box::new(MockRecursive));

        // And(TypedTag(name:1), TypedTag(other:1))
        let node = QueryNode::And(vec![
            QueryNode::TypedTag(crate::types::TypedTag::new("name", "1")),
            QueryNode::TypedTag(crate::types::TypedTag::new("other", "1")),
        ]);

        let expanded = expand_query_node(node, &reg);
        match expanded {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                // First should be expanded
                match &nodes[0] {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.tagtype.as_str(), "expanded");
                        assert_eq!(tt.label.as_str(), "rec");
                    }
                    _ => panic!(
                        "Expected expanded TypedTag in first node, got {:?}",
                        nodes[0]
                    ),
                }
                // Second should be same
                match &nodes[1] {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.tagtype.as_str(), "other")
                    }
                    _ => panic!(
                        "Expected original TypedTag in second node, got {:?}",
                        nodes[1]
                    ),
                }
            }
            _ => panic!("Expected And node, got {:?}", expanded),
        }
    }

    #[test]
    fn test_typedtag_query_expansion() {
        let q = TypedTagQuery;

        // 1. expand (通常の検索)
        let label = Label::String("extension:rs".to_string());
        let expanded = q.expand(&label);

        if let QueryNode::ColumnMatch { tag, label: l } = expanded {
            assert_eq!(tag, SType::TypedTag);
            assert_eq!(l, label);
        } else {
            panic!("Expected ColumnMatch, got {:?}", expanded);
        }

        // 2. expand_projection (プロジェクション)
        let expanded_proj = q.expand_projection(SType::TypedTag.into());

        if let QueryNode::Projection(tt) = expanded_proj {
            assert_eq!(tt, SType::TypedTag.into());
        } else {
            panic!("Expected Projection, got {:?}", expanded_proj);
        }
    }

    #[test]
    fn test_type_query_expansion() {
        let q = TypeQuery;

        // expand_projection
        let expanded_proj = q.expand_projection(SType::Type.into());
        // Projection(Type)
        if let QueryNode::Projection(tt) = expanded_proj {
            assert_eq!(tt, TagType::Base(SType::Type));
        } else {
            panic!("Expected Projection node, got {:?}", expanded_proj);
        }
    }
}
