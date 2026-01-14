use crate::db::SqlType;
use duckdb::types::{FromSql, FromSqlResult, ValueRef, ToSql, ToSqlOutput};
use uuid::Uuid;

/// メタデータ取得に失敗した際のデフォルト値。
pub const METADATA_ERROR: i64 = -1;

/// アイテムの優先度を表す型。
pub type Rank = i64;

/// アイテムの一意なID。
pub type ItemId = i64;

/// ファイルの実体（Inode/FileID）を一意に表す 128ビット識別子。
pub type FileRef = Uuid;

/// データベース上の型名を取得するためのトレイト。
pub trait DBType {
    /// 対応する SQL の型を返します。
    fn db_type() -> SqlType;
}

impl DBType for String { fn db_type() -> SqlType { SqlType::VARCHAR } }
impl DBType for i64 { fn db_type() -> SqlType { SqlType::BIGINT } }
impl DBType for Uuid { fn db_type() -> SqlType { SqlType::UUID } }
impl DBType for bool { fn db_type() -> SqlType { SqlType::BOOLEAN } }

/// ファイルサイズ（バイト単位）を表す型。
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct FileSize(pub i64);
impl DBType for FileSize { fn db_type() -> SqlType { SqlType::BIGINT } }

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
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct FileTimestamp(pub i64);
impl DBType for FileTimestamp { fn db_type() -> SqlType { SqlType::BIGINT } }

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
#[derive(Debug, PartialEq, Clone)]
pub enum TagType {
    Base(SType),
    Custom(String),
}

impl TagType {
    /// 文字列としての表現を取得します。
    pub fn as_str(&self) -> &str {
        match self {
            TagType::Base(s) => (*s).into(),
            TagType::Custom(s) => s.as_str(),
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
#[derive(Debug, PartialEq, Clone)]
pub enum Label {
    String(String),
    Integer(i64),
}

impl Label {
    /// 文字列としての値を取得します。
    pub fn as_str(&self) -> String {
        match self {
            Label::String(s) => s.clone(),
            Label::Integer(i) => i.to_string(),
        }
    }

    /// 数値としての値を取得します（数値でない場合は 0）。
    pub fn as_i64(&self) -> i64 {
        match self {
            Label::Integer(i) => *i,
            Label::String(s) => s.parse::<i64>().unwrap_or_default(),
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

impl TypedTag {
    /// 新しい `TypedTag` を作成します。
    pub fn new(tagtype: impl Into<TagType>, label: impl Into<Label>) -> Self {
        Self {
            tagtype: tagtype.into(),
            label: label.into(),
        }
    }
}

/// 検索結果を表す構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct SearchResult {
    /// アイテムの一意なID
    pub id: i64,
    /// アイテムの種類 (file, note, type, label, typedtag)
    pub item_kind: String,
    /// 解決済みの名称（ユーザ定義名を優先）
    pub name: String,
    /// アイテムの優先度
    pub rank: Rank,
    /// アイテムに紐づく全てのタグ (type, value)
    pub tags: Vec<(String, String)>,
}

impl SearchResult {
    /// 代表的な値（パスやコンテンツ）を取得するヘルパー。
    /// ファイルならパス、Noteならコンテンツなどを返します。
    pub fn primary_value(&self) -> Option<&str> {
        // 抽象化された名前があればそれを最優先
        if !self.name.is_empty() {
            return Some(&self.name);
        }
        // フォールバックとしてタグの中を探す
        self.get_tag_value("path")
            .or_else(|| self.get_tag_value("content"))
            .or_else(|| self.get_tag_value("value"))
            .or_else(|| self.get_tag_value("filename"))
    }

    /// 指定されたキーのタグ値を取得します。
    pub fn get_tag_value(&self, key: &str) -> Option<&str> {
        self.tags.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// ライフタイムに制約のないタグ名（参照）。
pub type Name<'a> = &'a str;

/// プログラム終了まで有効なタグ名（静的文字列）。
pub type StaticName = &'static str;

/// システムで使用される標準的なタグ名のシンボル定義。
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::EnumString, strum::Display)]
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
    // 検索専用仮想タグ
    Directory,
}
