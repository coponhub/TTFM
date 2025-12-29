/// タグの「キー」部分（例: "extension", "parentdir"）
/// 検索でもインデックスでも参照される正式な識別子を持つ
///
/// # Examples
///
/// ```
/// use ttfm::TagType;
/// let t = TagType("extension".to_string());
/// ```
#[derive(Debug, PartialEq, Clone)]
pub struct TagType(pub String);

/// タグの「値」部分（例: "rs", "src"）
///
/// # Examples
///
/// ```
/// use ttfm::types::Tag;
/// let t = Tag("rs".to_string());
/// ```
#[derive(Debug, PartialEq, Clone)]
pub struct Tag(pub String);

/// 「キー:値」のペア
/// 機能拡張におけるデータの基本単位
///
/// # Examples
///
/// ```
/// use ttfm::TypedTag;
/// let tt = TypedTag::new("extension".to_string(), "rs".to_string());
/// assert_eq!(tt.tagtype.0, "extension");
/// assert_eq!(tt.tag.0, "rs");
/// ```
#[derive(Debug, PartialEq, Clone)]
pub struct TypedTag {
    /// タグの型（キー）
    pub tagtype: TagType,
    /// タグの値
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