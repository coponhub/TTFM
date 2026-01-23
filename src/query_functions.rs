use crate::query::{QueryFunction, QueryNode};
use crate::types::{Label, SType, TagType, TypedTag};
use path_slash::PathExt;
use std::path::Path;

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
            QueryNode::ColumnMatch {
                tag: SType::Type,
                label: Label::String(SType::Filename.to_string()),
            },
            QueryNode::ColumnMatch {
                tag: SType::Label,
                label: label.clone(),
            },
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
        QueryNode::TypedTag(TypedTag::new(
            <&str>::from(SType::Extension).to_string(),
            Label::String(normalized),
        ))
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
        QueryNode::TypedTag(TypedTag::new(
            <&str>::from(SType::Path).to_string(),
            Label::String(normalize_path(&label.as_str())),
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
        QueryNode::TypedTag(TypedTag::new(
            <&str>::from(SType::Parentdir).to_string(),
            Label::String(normalize_path(&label.as_str())),
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
    // typedtag:xxx -> ColumnMatch(SType::TypedTag, xxx)
    // oneview の typedtag カラム（"type:label"）に対して検索を行う
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::TypedTag,
            label: label.clone(),
        }
    }
    // typedtag: (投影) -> Projection(SType::TypedTag)
    fn expand_projection(&self, _tagtype: TagType) -> QueryNode {
        QueryNode::Projection(SType::TypedTag.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryNode;
    use crate::types::{Label, SType, TagType};

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
