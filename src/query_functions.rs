use crate::query::{QueryFunction, QueryNode};
use crate::types::{SType, Label, TypedTag};
use std::path::Path;
use path_slash::PathExt;

/// "directory:name" -> "name:name & is_dir:true" への展開
pub struct DirectoryQuery;
impl QueryFunction for DirectoryQuery {
    fn name(&self) -> &str {
        SType::Directory.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::And(
            Box::new(QueryNode::ColumnMatch {
                tag: SType::Name,
                label: label.clone(),
            }),
            Box::new(QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::IsDir).to_string(),
                Label::String("true".to_string()),
            ))),
        )
    }
}

/// "filename:name" -> "name:name & is_dir:false" への展開
pub struct FilenameQuery;
impl QueryFunction for FilenameQuery {
    fn name(&self) -> &str {
        SType::Filename.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::And(
            Box::new(QueryNode::ColumnMatch {
                tag: SType::Name,
                label: label.clone(),
            }),
            Box::new(QueryNode::TypedTag(TypedTag::new(
                <&str>::from(SType::IsDir).to_string(),
                Label::String("false".to_string()),
            ))),
        )
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
        QueryNode::ColumnMatch {
            tag: SType::Name,
            label: label.clone(),
        }
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
        QueryNode::ColumnMatch {
            tag: SType::Rank,
            label: label.clone(),
        }
    }
}

/// "size:label" -> ColumnMatch(SType::Size, label)
pub struct SizeQuery;
impl QueryFunction for SizeQuery {
    fn name(&self) -> &str {
        SType::Size.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Size,
            label: label.clone(),
        }
    }
}

/// "mtime:label" -> ColumnMatch(SType::Mtime, label)
pub struct MtimeQuery;
impl QueryFunction for MtimeQuery {
    fn name(&self) -> &str {
        SType::Mtime.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Mtime,
            label: label.clone(),
        }
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
}