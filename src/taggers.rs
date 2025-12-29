use anyhow::Result;
use std::path::Path;

/// データベースのカラム定義
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// カラム名
    pub name: String,
    /// SQLのデータ型（例: "TEXT", "BIGINT"）
    pub sql_type: &'static str,
}

/// Taggerが抽出して返す値の型。
/// DuckDBのデータ型にマッピングされます。
#[derive(Debug)]
pub enum TagValue {
    /// 文字列データ
    Text(String),
    /// 整数データ（64ビット）
    BigInt(i64),
    /// 真偽値データ
    Boolean(bool),
    /// 空データ
    Null,
    /// キーバリューのマップデータ（ユーザータグ用）
    #[allow(dead_code)]
    Map(Vec<(String, String)>), // for tags map
}

impl TagValue {
    /// この値をDuckDBの `ToSql` パラメータに変換します。
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

/// ファイルから特定のメタデータを抽出し、データベースのカラムデータを提供するトレイト。
///
/// 各 `TagFunction` はこのトレイトを実装した構造体を内部に持ちます。
pub trait Tagger: Send + Sync {
    /// このTaggerが提供するカラム（タグ）の定義リストを返します。
    fn get_columns(&self) -> Vec<ColumnDef>;

    /// 指定されたファイルを解析し、`get_columns` で定義した順序に対応する値を抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>>;
}
