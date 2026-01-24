use crate::query::ast::{Operand, QueryNode};
use crate::types::{Label, TagType, TypedTag};
use std::collections::HashMap;

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
        use crate::query_functions::*;
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

pub fn expand_query_node(node: QueryNode, registry: &QueryFunctionRegistry) -> QueryNode {
    match node {
        QueryNode::And(nodes) => QueryNode::And(
            nodes.into_iter().map(|n| expand_query_node(n, registry)).collect(),
        ),
        QueryNode::Or(nodes) => QueryNode::Or(
            nodes.into_iter().map(|n| expand_query_node(n, registry)).collect(),
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
        QueryNode::TypedTag(tt) => {
            registry.process_tag(tt.tagtype, tt.label)
        }
        QueryNode::Projection(tt) => registry.expand_projection(tt),
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
            rest: vec![(ComparisonOp::Gt, Operand::Literal(Label::Integer(1)))]
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
            fn name(&self) -> &str { "size" } // Must be a valid SType (Base) to trigger expansion
            fn expand(&self, _: &Label) -> QueryNode { QueryNode::And(vec![]) }
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
             fn name(&self) -> &str { "size" } // Use valid SType "size"
             fn expand(&self, _label: &Label) -> QueryNode {
                 QueryNode::Comparison(crate::query::ast::ComparisonNode {
                     first: Operand::TypeRef(TagType::from("size")),
                     rest: vec![(ComparisonOp::Gt, Operand::Literal(Label::Integer(0)))]
                 })
             }
        }
        reg.register(Box::new(MockTagQuery));

        // Registered ("size")
        let node = reg.process_tag(TagType::from("size"), Label::from("foo"));
        match node {
            QueryNode::Comparison(_) => {},
            _ => panic!("Expected Comparison from expansion, got {:?}", node),
        }

        // Unregistered
        let node2 = reg.process_tag(TagType::from("unknown"), Label::from("foo"));
        match node2 {
            QueryNode::TypedTag(tt) => assert_eq!(tt.tagtype.as_str(), "unknown"),
            _ => panic!("Expected TypedTag for unknown, got {:?}", node2),
        }
    }

    #[test]
    fn test_expand_query_node_recursive() {
        let mut reg = QueryFunctionRegistry::new();
        struct MockRecursive;
        impl QueryFunction for MockRecursive {
            fn name(&self) -> &str { "name" } // Use valid SType "name"
            fn expand(&self, _: &Label) -> QueryNode {
                 QueryNode::TypedTag(crate::types::TypedTag::new("expanded", "rec"))
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
                     _ => panic!("Expected expanded TypedTag in first node, got {:?}", nodes[0]),
                }
                // Second should be same
                match &nodes[1] {
                     QueryNode::TypedTag(tt) => assert_eq!(tt.tagtype.as_str(), "other"),
                     _ => panic!("Expected original TypedTag in second node, got {:?}", nodes[1]),
                }
            }
            _ => panic!("Expected And node, got {:?}", expanded),
        }
    }
}

