use crate::query::ast::QueryNode;
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
            return QueryNode::And(vec![QueryNode::TypedTag(TypedTag::new(
                tagtype, label,
            ))]);
        }

        // Baseタグ（SType）であれば、レジストリから展開関数を探す
        if let TagType::Base(stag) = tagtype {
            let key_str: &'static str = stag.into();
            if let Some(f) = self.functions.get(key_str) {
                return f.expand(&label);
            }
        }

        // それ以外（カスタムタグまたは未登録の標準タグ）はそのまま TypedTag として保持
        QueryNode::TypedTag(TypedTag::new(tagtype, label))
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
    node: crate::query::ast::ComparisonNode,
    registry: &QueryFunctionRegistry,
) -> QueryNode {
    use crate::query::ast::{ComparisonOp, Operand};

    // 最初の演算子でモードを判定。Label モードの場合のみ拡張ロジックを適用
    match node.rest.first().map(|(op, _)| op) {
        Some(ComparisonOp::Label(_)) => {
            let rep_func = find_representative_function(&node, registry);
            let Some(f) = rep_func else {
                return QueryNode::Comparison(node);
            };

            if f.name() == SType::Mtime.name() {
                return expand_mtime_comparison(node);
            }

            // 標準的な正規化（SizeQuery 等）
            let mut node = node;
            if let Operand::Literal(label) = &mut node.first {
                *label = f.normalize_label(label);
            }
            for (op, rhs) in &mut node.rest {
                if let ComparisonOp::Label(_) = op {
                    if let Operand::Literal(label) = rhs {
                        *label = f.normalize_label(label);
                    }
                }
            }
            QueryNode::Comparison(node)
        }
        _ => QueryNode::Comparison(node),
    }
}

fn find_representative_function<'a>(
    node: &crate::query::ast::ComparisonNode,
    registry: &'a QueryFunctionRegistry,
) -> Option<&'a dyn QueryFunction> {
    use crate::query::ast::Operand;
    if let Operand::TypeRef(tt) = &node.first {
        if let Some(f) = registry.get_function(tt) {
            return Some(f);
        }
    }
    for (_, op) in &node.rest {
        if let Operand::TypeRef(tt) = op {
            if let Some(f) = registry.get_function(tt) {
                return Some(f);
            }
        }
    }
    None
}

fn expand_mtime_comparison(
    node: crate::query::ast::ComparisonNode,
) -> QueryNode {
    use crate::query::ast::Operand;

    let mut first = node.first.clone();
    let rest = node.rest;

    // 左辺の正規化
    if let Operand::Literal(label) = &mut first {
        if let Some(range) = crate::util::parse_datetime(&label.as_str()) {
            *label = Label::Mtime(range.start);
        }
    }

    let mut conditions = Vec::new();
    for (op, rhs) in rest {
        if let Operand::Literal(label) = &rhs {
            if let Some(range) = crate::util::parse_datetime(&label.as_str()) {
                conditions.extend(expand_mtime_range_op(
                    &first,
                    op,
                    range,
                    rhs.clone(),
                ));
                continue;
            }
        }
        conditions.push(QueryNode::Comparison(
            crate::query::ast::ComparisonNode {
                first: first.clone(),
                rest: vec![(op, rhs)],
            },
        ));
    }

    if conditions.len() == 1 {
        conditions.remove(0)
    } else {
        QueryNode::And(conditions)
    }
}

fn expand_mtime_range_op(
    first: &crate::query::ast::Operand,
    op: crate::query::ast::ComparisonOp,
    range: crate::util::DatetimeRange,
    _original_rhs: crate::query::ast::Operand,
) -> Vec<QueryNode> {
    use crate::query::ast::{BasicOp, ComparisonNode, ComparisonOp, Operand};

    // expand_mtime_comparison から呼ばれる際、op は必ず ComparisonOp::Label(BasicOp) であることを想定
    let ComparisonOp::Label(basic_op) = op else {
        return vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(op, _original_rhs)],
        })];
    };

    match basic_op {
        BasicOp::Eq => vec![
            QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(
                    ComparisonOp::Label(BasicOp::Ge),
                    Operand::Literal(Label::Mtime(range.start)),
                )],
            }),
            QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(
                    ComparisonOp::Label(BasicOp::Le),
                    Operand::Literal(Label::Mtime(range.end)),
                )],
            }),
        ],
        BasicOp::Ne => {
            vec![QueryNode::Complement(Box::new(QueryNode::And(vec![
                QueryNode::Comparison(ComparisonNode {
                    first: first.clone(),
                    rest: vec![(
                        ComparisonOp::Label(BasicOp::Ge),
                        Operand::Literal(Label::Mtime(range.start)),
                    )],
                }),
                QueryNode::Comparison(ComparisonNode {
                    first: first.clone(),
                    rest: vec![(
                        ComparisonOp::Label(BasicOp::Le),
                        Operand::Literal(Label::Mtime(range.end)),
                    )],
                }),
            ])))]
        }
        BasicOp::Gt => vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::Literal(Label::Mtime(range.end)),
            )],
        })],
        BasicOp::Ge => vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Ge),
                Operand::Literal(Label::Mtime(range.start)),
            )],
        })],
        BasicOp::Lt => vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Lt),
                Operand::Literal(Label::Mtime(range.start)),
            )],
        })],
        BasicOp::Le => vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Le),
                Operand::Literal(Label::Mtime(range.end)),
            )],
        })],
    }
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
        QueryNode::Comparison(cmp) => expand_comparison_node(cmp, registry),
        QueryNode::ColumnMatch { tag, label } => {
            QueryNode::ColumnMatch { tag, label }
        }
        QueryNode::TypedTag(tt) => {
            registry.process_tag(tt.label.tag_type(), tt.label)
        }
        QueryNode::Projection(tt) => registry.expand_projection(tt),
        QueryNode::Aggregation(agg) => match agg {
            crate::query::ast::AggregationNode::Count(node) => {
                QueryNode::Aggregation(
                    crate::query::ast::AggregationNode::Count(Box::new(
                        expand_query_node(*node, registry),
                    )),
                )
            }
            crate::query::ast::AggregationNode::Arithmetic { op, inner } => {
                QueryNode::Aggregation(
                    crate::query::ast::AggregationNode::Arithmetic {
                        op,
                        inner: Box::new(expand_query_node(*inner, registry)),
                    },
                )
            }
        },
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
            QueryNode::TypedTag(TypedTag::new(SType::Filename, label.clone())),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, true)),
        ])
    }
    fn expand_projection(&self, _tagtype: TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, true)),
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
            QueryNode::TypedTag(TypedTag::new(SType::Filename, label.clone())),
            // ディレクトリを除外
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
        ])
    }
    fn expand_projection(&self, _tagtype: TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
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
            QueryNode::TypedTag(TypedTag::new(SType::Extension, normalized)),
            // ディレクトリを除外 (拡張子はファイルのみ)
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
        ])
    }
    fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
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
        let label_val = match label.value() {
            crate::types::LabelValue::Literal(_) => {
                crate::types::LabelValue::Literal(normalized)
            }
            _ => crate::types::LabelValue::String(normalized),
        };
        QueryNode::TypedTag(TypedTag::new(SType::Path, label_val))
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
        let label_val = match label.value() {
            crate::types::LabelValue::Literal(_) => {
                crate::types::LabelValue::Literal(normalized)
            }
            _ => crate::types::LabelValue::String(normalized),
        };
        QueryNode::TypedTag(TypedTag::new(SType::Parentdir, label_val))
    }
}

/// "name:label" -> ColumnMatch(SType::Name, label)
pub struct NameQuery;
impl QueryFunction for NameQuery {
    fn name(&self) -> &str {
        SType::Name.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new(SType::Name, label.clone()))
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
        QueryNode::TypedTag(TypedTag::new(SType::Rank, label.clone()))
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
        if let Some(bytes) = crate::util::parse_size(&label.as_str()) {
            Label::Size(bytes)
        } else {
            label.clone()
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
        if let Some(range) = crate::util::parse_datetime(&label.as_str()) {
            if range.start == range.end {
                // 秒まで指定されている場合は単一の TypedTag
                QueryNode::TypedTag(TypedTag::new(SType::Mtime, range.start))
            } else {
                // 日付指定（範囲）の場合は期間検索へ展開
                QueryNode::And(vec![
                    QueryNode::Comparison(crate::query::ast::ComparisonNode {
                        first: crate::query::ast::Operand::TypeRef(
                            SType::Mtime.into(),
                        ),
                        rest: vec![(
                            crate::query::ast::ComparisonOp::Label(
                                crate::query::ast::BasicOp::Ge,
                            ),
                            crate::query::ast::Operand::Literal(Label::Mtime(
                                range.start,
                            )),
                        )],
                    }),
                    QueryNode::Comparison(crate::query::ast::ComparisonNode {
                        first: crate::query::ast::Operand::TypeRef(
                            SType::Mtime.into(),
                        ),
                        rest: vec![(
                            crate::query::ast::ComparisonOp::Label(
                                crate::query::ast::BasicOp::Le,
                            ),
                            crate::query::ast::Operand::Literal(Label::Mtime(
                                range.end,
                            )),
                        )],
                    }),
                ])
            }
        } else {
            QueryNode::TypedTag(TypedTag::new(SType::Mtime, label.clone()))
        }
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

/// "tag:label" -> item_kind:tag & name:label (タグ自体の検索)
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
            Label::Size(label.as_i64() * 1024)
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
            rest: vec![(
                ComparisonOp::Label(crate::query::ast::BasicOp::Gt),
                Operand::Literal(Label::from(1)),
            )],
        };

        let expanded = expand_comparison_node(node, &reg);

        let QueryNode::Comparison(comp) = expanded else {
            panic!("Expected Comparison, got {:?}", expanded);
        };

        match &comp.rest[0].1 {
            Operand::Literal(l) => assert_eq!(l.as_i64(), 1024),
            _ => panic!("Expected Literal with value 1024"),
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
                        ComparisonOp::Label(crate::query::ast::BasicOp::Gt),
                        Operand::Literal(Label::from(0)),
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
                assert_eq!(tt.label.tag_type().as_str(), "unknown")
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
                        assert_eq!(tt.label.tag_type().as_str(), "expanded");
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
                        assert_eq!(tt.label.tag_type().as_str(), "other")
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
    fn test_tag_query_expansion() {
        let q = TypedTagQuery;

        // 1. expand (通常の検索)
        let label = Label::from("extension:rs");
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

    #[test]
    fn test_expand_mtime_comparison() {
        let reg = QueryFunctionRegistry::with_standard();

        // 1. 一致検索 (日付指定) -> 範囲検索 (Ge & Le) への展開
        let node = crate::query::ast::ComparisonNode {
            first: Operand::TypeRef(TagType::from("mtime")),
            rest: vec![(
                ComparisonOp::Label(crate::query::ast::BasicOp::Eq),
                Operand::Literal(Label::from("2024/01/01")),
            )],
        };
        let expanded = expand_comparison_node(node, &reg);
        match expanded {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                // 順序は実装依存だが、Ge と Le が含まれているはず
            }
            _ => panic!(
                "Expected And node for mtime date equality, got {:?}",
                expanded
            ),
        }

        // 2. 大小比較 (Gt) -> 境界値の調整 (その日の終わり)
        let node_gt = crate::query::ast::ComparisonNode {
            first: Operand::TypeRef(TagType::from("mtime")),
            rest: vec![(
                ComparisonOp::Label(crate::query::ast::BasicOp::Gt),
                Operand::Literal(Label::from("2024/01/01")),
            )],
        };
        let expanded_gt = expand_comparison_node(node_gt, &reg);
        if let QueryNode::Comparison(comp) = expanded_gt {
            if let Operand::Literal(Label::Mtime(ts)) = &comp.rest[0].1 {
                // 2024/01/01 23:59:59 のタイムスタンプ（Local）
                assert!(*ts > 0);
            } else {
                panic!("Expected Mtime literal, got {:?}", comp.rest[0].1);
            }
        } else {
            panic!("Expected Comparison node, got {:?}", expanded_gt);
        }
    }
}
