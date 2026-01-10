use duckdb::types::{FromSql, FromSqlResult, ValueRef, ToSql, ToSqlOutput};

/// アイテムの優先度を表す型。
pub type Rank = i64;

/// データベース上の型名を取得するためのトレイト。
pub trait DBType {
    /// 対応する SQL の型名（例: "VARCHAR", "BIGINT"）を返します。
    fn db_type() -> &'static str;
}

impl DBType for String { fn db_type() -> &'static str { "VARCHAR" } }
impl DBType for i64 { fn db_type() -> &'static str { "BIGINT" } }
impl DBType for bool { fn db_type() -> &'static str { "BOOLEAN" } }

/// ファイルサイズ（バイト単位）を表す型。
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct FileSize(pub i64);
impl DBType for FileSize { fn db_type() -> &'static str { "BIGINT" } }

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
impl DBType for FileTimestamp { fn db_type() -> &'static str { "BIGINT" } }

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

/// タグの「キー」部分（例: "extension", "parentdir"）。
#[derive(Debug, PartialEq, Clone)]
pub struct TagType(pub String);

/// タグの「値」部分（例: "rs", "src"）。
#[derive(Debug, PartialEq, Clone)]
pub struct Label(pub String);

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
    pub fn new(key: String, value: String) -> Self {
        Self {
            tagtype: TagType(key),
            label: Label(value),
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
