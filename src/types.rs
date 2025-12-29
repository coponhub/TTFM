/// タグの「キー」部分（例: "extension", "parentdir"）
/// 検索でもインデックスでも参照される正式な識別子を持つ
#[derive(Debug, PartialEq, Clone)]
pub struct TagType(pub String);

/// タグの「値」部分（例: "rs", "src"）
#[derive(Debug, PartialEq, Clone)]
pub struct Tag(pub String);

/// 「キー:値」のペア
/// 機能拡張におけるデータの基本単位
#[derive(Debug, PartialEq, Clone)]
pub struct TypedTag {
    pub tagtype: TagType,
    pub tag: Tag,
}

impl TypedTag {
    pub fn new(key: String, value: String) -> Self {
        Self {
            tagtype: TagType(key),
            tag: Tag(value),
        }
    }
}