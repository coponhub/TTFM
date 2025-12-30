use anyhow::Result;
use std::path::Path;
use duckdb::types::{ToSql, ToSqlOutput, Null};

/// カラムが所属すべきテーブル。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetTable {
    Entities,
    Locations,
    Tags,
}

/// データベースのカラム定義。
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// カラム名（例: "filename"）
    pub name: String,
    /// SQLのデータ型（例: "TEXT", "BIGINT"）
    pub sql_type: &'static str,
    /// 所属テーブル
    pub target_table: TargetTable,
}

/// Taggerが抽出して返す値の型。
#[derive(Debug, Clone, PartialEq)]
pub enum TagValue {
    Text(String),
    BigInt(i64),
    Boolean(bool),
    Null,
    #[allow(dead_code)]
    Map(Vec<(String, String)>), 
}

impl ToSql for TagValue {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        match self {
            TagValue::Text(s) => Ok(ToSqlOutput::from(s.as_str())),
            TagValue::BigInt(i) => Ok(ToSqlOutput::from(*i)),
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
