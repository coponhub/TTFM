/// タグの「キー」部分（例: "extension", "parentdir"）。
#[derive(Debug, PartialEq, Clone)]
pub struct TagType(pub String);

/// タグの「値」部分（例: "rs", "src"）。
#[derive(Debug, PartialEq, Clone)]
pub struct Tag(pub String);

/// 「キー:値」のペアを表す構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct TypedTag {
    /// タグの型（キー）。例: "extension"
    pub tagtype: TagType,
    /// タグの値。例: "rs"
    pub tag: Tag,
}

impl TypedTag {
    /// 新しい `TypedTag` を作成します。
    pub fn new(key: String, value: String) -> Self {
        Self {
            tagtype: TagType(key),
            tag: Tag(value),
        }
    }
}
