use duckdb::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use uuid::Uuid;

/// メタデータ取得に失敗した際のデフォルト値。
pub const METADATA_ERROR: i64 = -1;

/// アイテムの優先度を表す型。
pub type Rank = i64;

/// アイテムの一意なID。
pub type ItemId = i64;

/// ファイルの実体（Inode/FileID）を一意に表す 128ビット識別子。
pub type FileRef = Uuid;

/// アイテムの種類 (file, note 等) を表す型エイリアス。
pub type ItemKind = String;

/// アイテムの表示名を表す型エイリアス。
pub type ItemName = String;

/// アイテム内におけるタグの順序（インデックス）を表す型エイリアス。
pub type TagNumber = usize;

/// データの由来を表す Enum。
#[derive(
    Debug, PartialEq, Eq, Hash, Clone, Copy, strum::Display, strum::EnumString,
)]
#[strum(serialize_all = "snake_case")]
pub enum Origin {
    /// システムによる自動抽出
    System,
    /// ユーザーによる手動付与
    User,
}

/// データベース上の型名を取得するためのトレイト。
pub trait DBType {
    /// 対応する SQL の型を返します。
    fn db_type() -> crate::db::SqlType;
}

impl DBType for String {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::VARCHAR
    }
}
impl DBType for i64 {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::BIGINT
    }
}
impl DBType for Uuid {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::UUID
    }
}
impl DBType for bool {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::BOOLEAN
    }
}

/// ファイルサイズ（バイト単位）を表す型。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub struct FileSize(pub i64);
impl DBType for FileSize {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::BIGINT
    }
}

impl FromSql for FileSize {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(FileSize)
    }
}

impl ToSql for FileSize {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

/// UNIXタイムスタンプ（秒単位）を表す型。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub struct FileTimestamp(pub i64);
impl DBType for FileTimestamp {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::BIGINT
    }
}

impl FromSql for FileTimestamp {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(FileTimestamp)
    }
}

impl ToSql for FileTimestamp {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

/// タグの「キー（型）」部分を表す SuperType。
/// システム定義の標準タグ（SType）と、自由なカスタムタグの両方を扱えます。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum TagType {
    Base(SType),
    Custom(String),
    /// 引用符で囲まれたリテラル。Glob無効・自動展開を行わない。
    LiteralCustom(String),
}

impl TagType {
    /// 文字列としての表現を取得します。
    pub fn as_str(&self) -> &str {
        match self {
            TagType::Base(s) => (*s).into(),
            TagType::Custom(s) => s.as_str(),
            TagType::LiteralCustom(s) => s.as_str(),
        }
    }
}

impl From<SType> for TagType {
    fn from(s: SType) -> Self {
        TagType::Base(s)
    }
}

impl From<String> for TagType {
    fn from(s: String) -> Self {
        SType::from_str(&s)
            .map(TagType::Base)
            .unwrap_or(TagType::Custom(s))
    }
}

impl From<&str> for TagType {
    fn from(s: &str) -> Self {
        s.to_string().into()
    }
}

/// タグの「値」部分（例: "rs", "1024"）。
/// 文字列と数値のどちらかを取り得ます。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum Label {
    String(String),
    Integer(i64),
    /// 引用符で囲まれたリテラル文字列。Globを無効化する。
    Literal(String),
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Label {
    /// 文字列としての値を取得します。
    pub fn as_str(&self) -> String {
        match self {
            Label::String(s) => s.clone(),
            Label::Integer(i) => i.to_string(),
            Label::Literal(s) => s.clone(),
        }
    }

    /// 数値としての値を取得します（数値でない場合は 0）。
    pub fn as_i64(&self) -> i64 {
        match self {
            Label::Integer(i) => *i,
            Label::String(s) | Label::Literal(s) => {
                s.parse::<i64>().unwrap_or_default()
            }
        }
    }
}

impl From<String> for Label {
    fn from(s: String) -> Self {
        Label::String(s)
    }
}

impl From<&str> for Label {
    fn from(s: &str) -> Self {
        Label::String(s.to_string())
    }
}

impl From<i64> for Label {
    fn from(i: i64) -> Self {
        Label::Integer(i)
    }
}

/// 「キー:値」のペアを表す構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct TypedTag {
    /// タグの型（キー）。例: "extension"
    pub tagtype: TagType,
    /// タグの値。例: "rs"
    pub label: Label,
}

impl std::fmt::Display for TypedTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.tagtype.as_str(), self.label.as_str())
    }
}

impl TypedTag {
    /// 新しい `TypedTag` を作成します。
    pub fn new(tagtype: impl Into<TagType>, label: impl Into<Label>) -> Self {
        Self {
            tagtype: tagtype.into(),
            label: label.into(),
        }
    }
}

/// 値と由来をセットで保持する構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct TagValue {
    /// タグの値
    pub label: Label,
    /// 由来
    pub origin: Origin,
}

/// タグの集合。
/// 並列リスト（Stream）ベースの Lazy な構造で、情報の解決をアクセス時まで遅延させます。
#[derive(Debug, PartialEq, Clone, Default)]
pub struct Tags {
    pub types: Vec<String>,
    pub labels: Vec<Label>,
    pub origins: Vec<Origin>,
}

impl Tags {
    pub fn new() -> Self {
        Self {
            types: Vec::new(),
            labels: Vec::new(),
            origins: Vec::new(),
        }
    }

    /// 新しいタグを追加します。
    pub fn push(&mut self, tagtype: TagType, label: Label, origin: Origin) {
        self.types.push(tagtype.as_str().to_string());
        self.labels.push(label);
        self.origins.push(origin);
    }

    /// 「型:値」のペアを生成するイテレータを返します。
    pub fn iter_typed_tags(&self) -> impl Iterator<Item = TypedTag> + '_ {
        self.types
            .iter()
            .zip(self.labels.iter())
            .map(|(t, l)| TypedTag {
                tagtype: TagType::from(t.as_str()),
                label: l.clone(),
            })
    }

    /// 指定された型のタグ値をリストとして取得します（リニアスキャン）。
    pub fn get_values(&self, key: &TagType) -> Vec<TagValue> {
        let key_str = key.as_str();
        self.types
            .iter()
            .enumerate()
            .filter(|(_, t)| *t == key_str)
            .map(|(i, _)| TagValue {
                label: self.labels[i].clone(),
                origin: self.origins[i],
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }
}

// 既存コードとの互換性のためのイテレーション対応（所有権を消費）
impl IntoIterator for Tags {
    type Item = (TagType, Vec<TagValue>);
    type IntoIter = std::collections::hash_map::IntoIter<TagType, Vec<TagValue>>;

    fn into_iter(self) -> Self::IntoIter {
        // 必要に応じて HashMap に詰め直して返す（Lazy 化の恩恵は受けられないが、互換性は保つ）
        let mut map: std::collections::HashMap<TagType, Vec<TagValue>> =
            std::collections::HashMap::new();
        for i in 0..self.types.len() {
            map.entry(TagType::from(self.types[i].as_str()))
                .or_default()
                .push(TagValue {
                    label: self.labels[i].clone(),
                    origin: self.origins[i],
                });
        }
        map.into_iter()
    }
}

// 共有参照によるイテレーション（HashMap への詰め直しを避けるため、(TagType, Vec<TagValue>) 形式は限定的に）
impl<'a> IntoIterator for &'a Tags {
    type Item = (TagType, Vec<TagValue>);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        let mut map: std::collections::HashMap<TagType, Vec<TagValue>> =
            std::collections::HashMap::new();
        for i in 0..self.types.len() {
            map.entry(TagType::from(self.types[i].as_str()))
                .or_default()
                .push(TagValue {
                    label: self.labels[i].clone(),
                    origin: self.origins[i],
                });
        }
        map.into_iter().collect::<Vec<_>>().into_iter()
    }
}

/// アイテム固有の不動の情報をまとめた構造体。
#[derive(Debug, PartialEq, Clone, Default)]
pub struct Intrinsic {
    /// ファイルサイズ
    pub size: Option<FileSize>,
    /// 更新日時
    pub mtime: Option<FileTimestamp>,
    /// コンテンツのハッシュ
    pub hash: Option<String>,
}


/// ライフタイムに制約のないタグ名（参照）。
pub type Name<'a> = &'a str;

/// プログラム終了まで有効なタグ名（静的文字列）。
pub type StaticName = &'static str;

/// システムで使用される標準的なタグ名のシンボル定義。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::Display,
)]
#[strum(serialize_all = "snake_case")]
pub enum SType {
    ItemId,
    FileId,
    Path,
    Parentdir,
    Filename,
    Stem,
    Extension,
    IsDir,
    Size,
    Mtime,
    TypeFromExt,
    SizeStr,
    ModifiedStr,
    Hash,
    Type,
    Label,
    ItemKind,
    Content,
    Rank,
    Origin,
    Name,
    // 内部カラム用
    Types,
    Labels,
    ScanHash,
    #[strum(serialize = "typedtag")]
    TypedTag,
    // 検索専用仮想タグ
    Directory,
    // Typed Label Columns
    LabelStr,
    LabelInt,
    LabelDouble,
    LabelBool,
    // Schema Table Columns
    DataType,
}

#[cfg(test)]
mod tests_types {
    use super::*;

    #[test]
    fn test_typed_tag_display() {
        let tt = TypedTag::new("extension", "rs");
        assert_eq!(tt.to_string(), "extension:rs");

        let tt_int = TypedTag::new("size", 1024);
        assert_eq!(tt_int.to_string(), "size:1024");
    }

    #[test]
    fn test_tags_iter_typed_tags() {
        let mut tags = Tags::new();
        tags.push(
            TagType::from("project"),
            Label::from("A"),
            Origin::User,
        );
        tags.push(
            TagType::from("project"),
            Label::from("B"),
            Origin::User,
        );
        tags.push(
            TagType::from("extension"),
            Label::from("rs"),
            Origin::User,
        );

        let mut results: Vec<String> = tags
            .iter_typed_tags()
            .map(|tt| tt.to_string())
            .collect();
        results.sort();

        assert_eq!(results.len(), 3);
        assert!(results.contains(&"project:A".to_string()));
        assert!(results.contains(&"project:B".to_string()));
        assert!(results.contains(&"extension:rs".to_string()));
    }
}
