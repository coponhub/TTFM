use anyhow::Result;
use std::path::Path;

/// データベースのカラム定義。
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// カラム名（例: "filename"）
    pub name: String,
    /// SQLのデータ型（例: "TEXT", "BIGINT"）
    pub sql_type: &'static str,
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

impl TagValue {
    /// DuckDBの `ToSql` パラメータに変換します。
    pub fn to_sql_param(&self) -> Box<dyn duckdb::ToSql> {
        match self {
            TagValue::Text(s) => Box::new(s.clone()),
            TagValue::BigInt(i) => Box::new(*i),
            TagValue::Boolean(b) => Box::new(*b),
            TagValue::Null => Box::new(Option::<String>::None),
            TagValue::Map(_) => Box::new(Option::<String>::None), 
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