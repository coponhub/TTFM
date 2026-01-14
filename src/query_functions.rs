use crate::query::{QueryFunction, QueryNode};
use crate::types::{STag, Label, TypedTag};
use crate::db::Col;
use std::path::Path;
use path_slash::PathExt;

/// "directory:name" -> "name:name & is_dir:true" への展開
pub struct DirectoryQuery;
impl QueryFunction for DirectoryQuery {
    fn name(&self) -> &str {
        STag::Directory.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::And(
            Box::new(QueryNode::ColumnMatch {
                col: Col::Name,
                label: label.clone(),
            }),
            Box::new(QueryNode::TypedTag(TypedTag::new(
                <&str>::from(STag::IsDir).to_string(),
                Label::String("true".to_string()),
            ))),
        )
    }
}

/// "filename:name" -> "name:name & is_dir:false" への展開
pub struct FilenameQuery;
impl QueryFunction for FilenameQuery {
    fn name(&self) -> &str {
        STag::Filename.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::And(
            Box::new(QueryNode::ColumnMatch {
                col: Col::Name,
                label: label.clone(),
            }),
            Box::new(QueryNode::TypedTag(TypedTag::new(
                <&str>::from(STag::IsDir).to_string(),
                Label::String("false".to_string()),
            ))),
        )
    }
}

/// "extension:RS" -> "extension:rs" (小文字化とドットの削除)
pub struct ExtensionQuery;
impl QueryFunction for ExtensionQuery {
    fn name(&self) -> &str {
        STag::Extension.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        let normalized = label
            .as_str()
            .to_lowercase()
            .trim_start_matches('.')
            .to_string();
        QueryNode::TypedTag(TypedTag::new(
            <&str>::from(STag::Extension).to_string(),
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
        STag::Path.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new(
            <&str>::from(STag::Path).to_string(),
            Label::String(normalize_path(&label.as_str())),
        ))
    }
}

/// "parentdir:C:\foo" -> "parentdir:C:/foo"
pub struct ParentDirQuery;
impl QueryFunction for ParentDirQuery {
    fn name(&self) -> &str {
        STag::Parentdir.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new(
            <&str>::from(STag::Parentdir).to_string(),
            Label::String(normalize_path(&label.as_str())),
        ))
    }
}

/// "name:label" -> ColumnMatch(Col::Name, label)
pub struct NameQuery;
impl QueryFunction for NameQuery {
    fn name(&self) -> &str {
        STag::Name.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            col: Col::Name,
            label: label.clone(),
        }
    }
}

/// "item_kind:label" -> ColumnMatch(Col::ItemKind, label)
pub struct ItemKindQuery;
impl QueryFunction for ItemKindQuery {
    fn name(&self) -> &str {
        STag::ItemKind.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            col: Col::ItemKind,
            label: label.clone(),
        }
    }
}

/// "rank:label" -> ColumnMatch(Col::Rank, label)
pub struct RankQuery;
impl QueryFunction for RankQuery {
    fn name(&self) -> &str {
        STag::Rank.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            col: Col::Rank,
            label: label.clone(),
        }
    }
}

/// "size:label" -> ColumnMatch(Col::Size, label)
pub struct SizeQuery;
impl QueryFunction for SizeQuery {
    fn name(&self) -> &str {
        STag::Size.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            col: Col::Size,
            label: label.clone(),
        }
    }
}

/// "mtime:label" -> ColumnMatch(Col::Mtime, label)
pub struct MtimeQuery;
impl QueryFunction for MtimeQuery {
    fn name(&self) -> &str {
        STag::Mtime.into()
    }
    fn expand(&self, label: &Label) -> QueryNode {
        QueryNode::ColumnMatch {
            col: Col::Mtime,
            label: label.clone(),
        }
    }
}