use crate::db::Col;
use crate::query::QueryFunction;
use crate::query::functions::*;
use crate::query::ast::ComparisonOp;
use crate::types::{Label, SType, TagType};
use anyhow::Result;
use std::collections::HashMap;

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

/// タグのメタデータ記述
pub struct TagDescriptor {
    pub tag_type: TagType,
    pub storage: StorageMapping,
    pub sql_type: crate::db::SqlType,
    pub logical_function: Option<Box<dyn QueryFunction>>,
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
        let mut base = Self::base_standard();
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
            self.registry.insert(descriptor.tag_type.clone(), descriptor);
        }
    }

    /// 指定されたタグの定義を検索します。
    pub fn look_up(&self, tag: &TagType) -> Option<&TagDescriptor> {
        self.registry.get(tag)
    }

    /// 特定の標準タグ（SType）に対応する物理カラムを解決します。
    pub fn resolve_col(&self, stype: crate::types::SType) -> anyhow::Result<crate::db::Col> {
        let tag = TagType::Base(stype);
        let desc = self.look_up(&tag).ok_or_else(|| anyhow::anyhow!("Tag definition not found: {:?}", tag))?;
        if let StorageMapping::Column(col) = desc.storage {
            Ok(col)
        } else {
            Err(anyhow::anyhow!("Tag {:?} is not mapped to a direct column", tag))
        }
    }

    /// 論理的なクエリタグを、Lens の定義に基づいて展開（Expand）します。
    pub fn expand(&self, node: crate::query::ast::QueryNode) -> anyhow::Result<crate::query::ast::QueryNode> {
        self.expand_recursive(node)
    }

    fn expand_recursive(&self, node: crate::query::ast::QueryNode) -> anyhow::Result<crate::query::ast::QueryNode> {
        use crate::query::ast::QueryNode;
        match node {
            QueryNode::TypedTag(tt) => {
                if let Some(desc) = self.look_up(&tt.tagtype) {
                    if let Some(func) = &desc.logical_function {
                        return Ok(func.expand(&tt.label));
                    }
                }
                Ok(QueryNode::TypedTag(tt))
            }
            QueryNode::Projection(tagtype) => {
                if let Some(desc) = self.look_up(&tagtype) {
                    if let Some(func) = &desc.logical_function {
                        return Ok(func.expand_projection(tagtype.clone()));
                    }
                }
                Ok(QueryNode::Projection(tagtype))
            }
            QueryNode::And(nodes) => {
                let mut expanded = Vec::new();
                for n in nodes {
                    expanded.push(self.expand_recursive(n)?);
                }
                Ok(QueryNode::And(expanded))
            }
            QueryNode::Or(nodes) => {
                let mut expanded = Vec::new();
                for n in nodes {
                    expanded.push(self.expand_recursive(n)?);
                }
                Ok(QueryNode::Or(expanded))
            }
            QueryNode::Difference(l, r) => Ok(QueryNode::Difference(
                Box::new(self.expand_recursive(*l)?),
                Box::new(self.expand_recursive(*r)?),
            )),
            QueryNode::Complement(c) => {
                Ok(QueryNode::Complement(Box::new(self.expand_recursive(*c)?)))
            }
            QueryNode::Comparison(cmp) => {
                Ok(QueryNode::Comparison(self.expand_comparison_node(cmp)?))
            }
            other => Ok(other),
        }
    }

    fn expand_comparison_node(
        &self,
        mut cmp: crate::query::ast::ComparisonNode,
    ) -> anyhow::Result<crate::query::ast::ComparisonNode> {
        use crate::query::ast::Operand;

        if let Some(func) = self.find_logical_function(&cmp) {
            if let Operand::Literal(lab) = &mut cmp.first {
                *lab = func.normalize_label(lab);
            }
            for (_, op) in &mut cmp.rest {
                if let Operand::Literal(lab) = op {
                    *lab = func.normalize_label(lab);
                }
            }
        }
        Ok(cmp)
    }

    fn find_logical_function<'a>(
        &'a self,
        cmp: &crate::query::ast::ComparisonNode,
    ) -> Option<&'a dyn crate::query::QueryFunction> {
        use crate::query::ast::Operand;

        let resolve = |op: &Operand| match op {
            Operand::TypeRef(tt) => {
                self.look_up(tt).and_then(|d| d.logical_function.as_deref())
            }
            _ => None,
        };

        resolve(&cmp.first).or_else(|| {
            cmp.rest.iter().find_map(|(_, op)| resolve(op))
        })
    }

    /// 展開済みノードを、物理的な所在（StorageMapping）を持つ ResolvedNode へ解決します。
    pub fn resolve(&self, node: crate::query::ast::QueryNode) -> anyhow::Result<ResolvedNode> {
        use crate::query::ast::QueryNode;
        use crate::query::ast::ComparisonOp;
        match node {
            QueryNode::TypedTag(tt) => {
                let (storage, sql_type) = match self.look_up(&tt.tagtype) {
                    Some(desc) => (desc.storage.clone(), desc.sql_type),
                    None => (
                        StorageMapping::RowTag {
                            column: crate::db::Col::LabelStr,
                            tag_key: tt.tagtype.as_str().to_string(),
                        },
                        crate::db::SqlType::VARCHAR,
                    ),
                };
                Ok(ResolvedNode::Match {
                    tag_type: tt.tagtype,
                    storage,
                    sql_type,
                    op: ComparisonOp::Eq,
                    label: tt.label,
                })
            }
            QueryNode::ColumnMatch { tag, label } => {
                let tag_type = TagType::Base(tag);
                let desc = self
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
            QueryNode::Comparison(cmp) => {
                self.resolve_comparison(cmp)
            }
            QueryNode::And(nodes) => {
                let mut resolved = Vec::new();
                for n in nodes {
                    resolved.push(self.resolve(n)?);
                }
                Ok(ResolvedNode::And(resolved))
            }
            QueryNode::Or(nodes) => {
                let mut resolved = Vec::new();
                for n in nodes {
                    resolved.push(self.resolve(n)?);
                }
                Ok(ResolvedNode::Or(resolved))
            }
            QueryNode::Difference(l, r) => Ok(ResolvedNode::Difference(
                Box::new(self.resolve(*l)?),
                Box::new(self.resolve(*r)?),
            )),
            QueryNode::Complement(c) => {
                Ok(ResolvedNode::Complement(Box::new(self.resolve(*c)?)))
            }
            QueryNode::Projection(tt) => {
                let storage = match self.look_up(&tt) {
                    Some(desc) => desc.storage.clone(),
                    None => StorageMapping::RowTag {
                        column: crate::db::Col::LabelStr,
                        tag_key: tt.as_str().to_string(),
                    },
                };
                Ok(ResolvedNode::Projection(tt, storage))
            }
            _ => Err(anyhow::anyhow!("Unsupported query node for resolution")),
        }
    }

    fn resolve_comparison(&self, cmp: crate::query::ast::ComparisonNode) -> anyhow::Result<ResolvedNode> {
        let mut nodes = Vec::new();
        let mut current_left = cmp.first;

        for (op, right) in cmp.rest {
            nodes.push(self.resolve_single_match(
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
        &self,
        left: crate::query::ast::Operand,
        op: crate::query::ast::ComparisonOp,
        right: crate::query::ast::Operand,
    ) -> anyhow::Result<ResolvedNode> {
        use crate::query::ast::Operand;
        use crate::query::ast::ComparisonOp;

        match (left, right) {
            (Operand::TypeRef(tt), Operand::Literal(lab)) => {
                let (storage, sql_type) = self.get_storage_and_type(&tt);
                Ok(ResolvedNode::Match {
                    tag_type: tt,
                    storage,
                    sql_type,
                    op,
                    label: lab,
                })
            }
            (Operand::Literal(lab), Operand::TypeRef(tt)) => {
                let (storage, sql_type) = self.get_storage_and_type(&tt);
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
        &self,
        tt: &crate::types::TagType,
    ) -> (StorageMapping, crate::db::SqlType) {
        match self.look_up(tt) {
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
}

fn flip_op(op: crate::query::ast::ComparisonOp) -> crate::query::ast::ComparisonOp {
    use crate::query::ast::ComparisonOp;
    match op {
        ComparisonOp::Gt => ComparisonOp::Lt,
        ComparisonOp::Ge => ComparisonOp::Le,
        ComparisonOp::Lt => ComparisonOp::Gt,
        ComparisonOp::Le => ComparisonOp::Ge,
        other => other,
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
            logical_function: Some(Box::new(FilenameQuery)),
        }))
        .collect()
}

fn virtual_tag_descriptors() -> Vec<TagDescriptor> {
    let v_tags: Vec<(SType, Box<dyn QueryFunction>)> = vec![
        (SType::Directory, Box::new(DirectoryQuery)),
        (SType::Extension, Box::new(ExtensionQuery)),
        (SType::Path, Box::new(PathQuery)),
        (SType::Parentdir, Box::new(ParentDirQuery)),
        (SType::Size, Box::new(SizeQuery)),
        (SType::Mtime, Box::new(MtimeQuery)),
        (SType::ItemKind, Box::new(ItemKindQuery)),
        (SType::Rank, Box::new(RankQuery)),
        (SType::Origin, Box::new(OriginQuery)),
        (SType::Type, Box::new(TypeQuery)),
        (SType::Label, Box::new(LabelQuery)),
        (SType::TypedTag, Box::new(TypedTagQuery)),
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
