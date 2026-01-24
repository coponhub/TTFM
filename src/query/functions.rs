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
