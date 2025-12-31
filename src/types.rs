use duckdb::types::{FromSql, FromSqlResult, ValueRef, ToSql, ToSqlOutput};

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
