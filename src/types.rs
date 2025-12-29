/// タグの「キー」部分（例: "extension", "parentdir"）
/// 検索でもインデックスでも参照される正式な識別子を持つ
#[derive(Debug, PartialEq, Clone)]
pub struct TagType(pub String);

impl TagType {
    // Standard System Tags
    pub const PATH: &'static str = "path";
    pub const PARENT_DIR: &'static str = "parentdir";
    pub const FILENAME: &'static str = "filename";
    pub const STEM: &'static str = "stem";
    pub const EXTENSION: &'static str = "extension";
    pub const DIRECTORY: &'static str = "directory";
    pub const SIZE_BYTES: &'static str = "size_bytes";
    pub const MODIFIED_TS: &'static str = "modified_ts";
    pub const KIND: &'static str = "kind";
    pub const SIZE_STR: &'static str = "size_str";
    pub const MODIFIED_STR: &'static str = "modified_str";
    pub const TAGS: &'static str = "tags";
}

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