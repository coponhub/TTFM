use anyhow::Result;
use duckdb::types::{Null, ToSql, ToSqlOutput};
use std::path::Path;

use crate::db::{SqlType, TargetTable};

/// データベースのカラム定義。
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// カラム名（例: "filename"）
    pub name: String,
    /// SQLのデータ型
    pub sql_type: SqlType,
    /// 所属テーブル
    pub target_table: TargetTable,
}

/// Taggerが抽出して返す値の型。
#[derive(Debug, Clone, PartialEq)]
pub enum TagValue {
    Text(String),
    BigInt(i64),
    Uuid(uuid::Uuid),
    Boolean(bool),
    Null,
    #[allow(dead_code)]
    Map(Vec<(String, String)>),
}

impl TagValue {
    /// 値を文字列として取得します。所有権を移動するため、クローンを避けられます。
    pub fn into_string(self) -> Option<String> {
        match self {
            TagValue::Text(s) => Some(s),
            TagValue::BigInt(i) => Some(i.to_string()),
            TagValue::Uuid(u) => Some(u.to_string()),
            TagValue::Boolean(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// 値を文字列として取得します（クローンが発生します）。
    pub fn to_string_lossy(&self) -> Option<String> {
        match self {
            TagValue::Text(s) => Some(s.clone()),
            TagValue::BigInt(i) => Some(i.to_string()),
            TagValue::Uuid(u) => Some(u.to_string()),
            TagValue::Boolean(b) => Some(b.to_string()),
            _ => None,
        }
    }
}

impl ToSql for TagValue {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        match self {
            TagValue::Text(s) => Ok(ToSqlOutput::from(s.as_str())),
            TagValue::BigInt(i) => Ok(ToSqlOutput::from(*i)),
            TagValue::Uuid(u) => Ok(ToSqlOutput::from(*u)),
            TagValue::Boolean(b) => Ok(ToSqlOutput::from(*b)),
            TagValue::Null => Ok(ToSqlOutput::from(Null)),
            TagValue::Map(_) => Ok(ToSqlOutput::from(Null)),
        }
    }
}

/// ファイルからメタデータを抽出するトレイト。
pub trait Tagger: Send + Sync {
    /// 提供するカラム定義を返します。
    fn get_columns(&self) -> Vec<ColumnDef>;

    /// 指定されたファイルから値を抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>>;
}
