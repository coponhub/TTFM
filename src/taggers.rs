use anyhow::Result;
use std::path::Path;

/// データベースのカラム定義
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub sql_type: &'static str,
}

/// Taggerが返す値の型
#[derive(Debug)]
pub enum TagValue {
    Text(String),
    BigInt(i64),
    Boolean(bool),
    Null,
    #[allow(dead_code)]
    Map(Vec<(String, String)>), // for tags map
}

impl TagValue {
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

/// ファイルから情報を抽出し、カラムデータを提供するトレイト
pub trait Tagger: Send + Sync {
    /// このTaggerが提供するカラム（タグ）のリストを返す
    fn get_columns(&self) -> Vec<ColumnDef>;

    /// ファイルを解析し、カラム順に対応するデータを返す
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>>;
}
