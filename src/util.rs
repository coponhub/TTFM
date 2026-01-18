use anyhow::Result;
use duckdb::Connection;
use sea_query::{
    ColumnDef, DeleteStatement, Expr, Iden, InsertStatement, IntoIden,
    PostgresQueryBuilder, Query, SelectStatement, SimpleExpr,
    TableCreateStatement, TableDropStatement, UpdateStatement,
};
use std::path::Path;

// --- 1. 通常の関数 (ロジックの実体) ---

/// sea-query のステートメントをビルドして実行します。
pub fn execute<S: SqlStatement + ?Sized>(
    conn: &Connection,
    stmt: &S,
) -> Result<()> {
    conn.execute(&stmt.build(), [])?;
    Ok(())
}

/// SelectStatement の結果をアトミックに Parquet 保存します。
pub fn save_parquet(
    conn: &Connection,
    query: &SelectStatement,
    path: &Path,
) -> Result<()> {
    let sql = query.to_string(PostgresQueryBuilder);
    let path_str = path.to_string_lossy();
    let tmp_path = format!("{}.tmp", path_str);

    let copy_sql = format!(
        "COPY ({}) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')",
        sql, tmp_path
    );
    conn.execute(&copy_sql, [])?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// 指定したテーブル（Iden）の全内容を Parquet 保存します。
pub fn write_parquet<I: Iden + Clone + 'static>(
    conn: &Connection,
    table: I,
    path: &Path,
) -> Result<()> {
    let query = Query::select()
        .column(sea_query::Asterisk)
        .from(table.clone())
        .to_owned();
    save_parquet(conn, &query, path)
}

/// CAST(NULL AS type) を生成します。
pub fn null_as<I: Iden + Clone + 'static>(iden: I) -> SimpleExpr {
    Expr::val(None::<i8>).cast_as(iden)
}

// --- 2. トレイト (メソッドチェーン用) ---

/// ステートメントを文字列化するための内部トレイト
pub trait SqlStatement {
    fn build(&self) -> String;
}

macro_rules! impl_sql_statement {
    ($($t:ty),*) => {
        $(
            impl SqlStatement for $t {
                fn build(&self) -> String { self.to_string(PostgresQueryBuilder) }
            }
        )*
    };
}

impl_sql_statement!(
    SelectStatement,
    TableCreateStatement,
    TableDropStatement,
    DeleteStatement,
    UpdateStatement,
    InsertStatement
);

/// .execute(conn) を提供するトレイト
pub trait ExecuteSql: SqlStatement {
    fn execute(&self, conn: &Connection) -> Result<()> {
        execute(conn, self)
    }
}

impl<T: SqlStatement> ExecuteSql for T {}

/// .save_parquet(conn, path) を提供するトレイト
pub trait ParquetExt {
    fn save_parquet(&self, conn: &Connection, path: &Path) -> Result<()>;
}

impl ParquetExt for SelectStatement {
    fn save_parquet(&self, conn: &Connection, path: &Path) -> Result<()> {
        save_parquet(conn, self, path)
    }
}

pub trait SelectExt {
    fn create_table_as<I: Iden + Clone + 'static>(
        &self,
        conn: &Connection,
        name: I,
    ) -> Result<I>;
    fn create_temp_table_as<I: Iden + Clone + 'static>(
        &self,
        conn: &Connection,
        name: I,
    ) -> Result<I>;
}

impl SelectExt for SelectStatement {
    fn create_table_as<I: Iden + Clone + 'static>(
        &self,
        conn: &Connection,
        name: I,
    ) -> Result<I> {
        let sql = format!(
            "CREATE TABLE {} AS {}",
            iden_to_sql(name.clone()),
            self.to_string(PostgresQueryBuilder)
        );
        conn.execute(&sql, [])?;
        Ok(name)
    }

    fn create_temp_table_as<I: Iden + Clone + 'static>(
        &self,
        conn: &Connection,
        name: I,
    ) -> Result<I> {
        let sql = format!(
            "CREATE TEMP TABLE {} AS {}",
            iden_to_sql(name.clone()),
            self.to_string(PostgresQueryBuilder)
        );
        conn.execute(&sql, [])?;
        Ok(name)
    }
}

/// .write_parquet(conn, path) を提供するトレイト
pub trait IdenExt: Iden {
    fn write_parquet(&self, conn: &Connection, path: &Path) -> Result<()>;
    fn drop_table(&self, conn: &Connection) -> Result<()>;
}

impl<T: Iden + Clone + 'static> IdenExt for T {
    fn write_parquet(&self, conn: &Connection, path: &Path) -> Result<()> {
        write_parquet(conn, self.clone(), path)
    }

    fn drop_table(&self, conn: &Connection) -> Result<()> {
        use sea_query::Table;
        Table::drop().table(self.clone()).execute(conn)
    }
}

/// テーブル作成の拡張トレイト
pub trait TableCreateExt {
    /// (名前, 型) のペアのイテレータを受け取り、カラムを追加します。
    fn add_columns<I, N, T>(&mut self, iter: I) -> &mut Self
    where
        I: IntoIterator<Item = (N, T)>,
        N: IntoIden,
        T: IntoIden;
}

impl TableCreateExt for TableCreateStatement {
    fn add_columns<I, N, T>(&mut self, iter: I) -> &mut Self
    where
        I: IntoIterator<Item = (N, T)>,
        N: IntoIden,
        T: IntoIden,
    {
        for (name, col_type) in iter {
            self.col(ColumnDef::new(name).custom(col_type));
        }
        self
    }
}

// --- 3. その他の補助関数 ---

/// CREATE OR REPLACE VIEW を実行します。
pub fn create_or_replace_view(
    conn: &Connection,
    name: impl Iden + 'static,
    query: SelectStatement,
) -> Result<()> {
    let quoted_name = iden_to_sql(name);
    let sql = format!(
        "CREATE OR REPLACE VIEW {} AS {}",
        quoted_name,
        query.to_string(PostgresQueryBuilder)
    );
    conn.execute(&sql, [])?;
    Ok(())
}

pub fn iden_to_sql(iden: impl Iden + 'static) -> String {
    let sql = Query::select()
        .column(iden)
        .from(crate::db::Tbl::Master)
        .to_string(PostgresQueryBuilder);
    sql.split_whitespace().nth(1).unwrap_or("").to_string()
}

/// 文字列から sea_query のエイリアス識別子を作成します。
pub fn alias_from(s: &str) -> sea_query::DynIden {
    use sea_query::Alias;
    Alias::new(s).into_iden()
}

/// 文字列から Col または Alias への識別子変換を行います。
pub fn col_to_iden(name: &str) -> sea_query::DynIden {
    use crate::db::Col;
    use sea_query::{Alias, IntoIden};

    Col::from_str(name)
        .map(|c| c.into_iden())
        .unwrap_or_else(|| Alias::new(name).into_iden())
}

/// 成功値を Result::Ok に包むための拡張トレイト。
/// パイプライン風の記述を可能にし、ネストを減らすために使用します。
pub trait DotOk: Sized {
    /// 自身を Ok で包んで返します。
    fn to_ok<E>(self) -> Result<Self, E> {
        Ok(self)
    }
}

// 全ての型に対して DotOk を実装
impl<T: Sized> DotOk for T {}

pub fn parquet_query(path: &str) -> SelectStatement {
    use crate::db::{DuckDbFunc, Tbl};
    use sea_query::Func;
    Query::select()
        .expr(Expr::cust("*"))
        .from_function(
            Func::cust(DuckDbFunc::ReadParquet).arg(Expr::val(path)),
            Tbl::Diff,
        )
        .to_owned()
}

/// 単位付きのサイズ文字列（例: "1.5MB", "100KiB", "2PB"）をバイト数に変換します。
/// 単位は B, KB, MB, GB, TB, PB (1024累乗) をサポートします。
pub fn parse_size(s: &str) -> Option<i64> {
    let s = s.trim().to_uppercase();
    if s.is_empty() {
        return None;
    }

    // 数値部分と単位部分を分離
    let unit_start = s.find(|c: char| !c.is_numeric() && c != '.');

    let (num_part, unit_part) = match unit_start {
        Some(idx) => s.split_at(idx),
        None => (s.as_str(), ""),
    };

    let val: f64 = num_part.trim().parse().ok()?;
    let unit = unit_part.trim();

    let multiplier: i64 = match unit {
        "" | "B" => 1,
        "KB" | "KIB" | "K" => 1024,
        "MB" | "MIB" | "M" => 1024 * 1024,
        "GB" | "GIB" | "G" => 1024 * 1024 * 1024,
        "TB" | "TIB" | "T" => 1024 * 1024 * 1024 * 1024,
        "PB" | "PIB" | "P" => 1024 * 1024 * 1024 * 1024 * 1024,
        _ => return None,
    };

    Some((val * multiplier as f64) as i64)
}

/// メタデータ取得エラー時にエラー値を返すためのラッパー。
pub struct SafeMetadata {
    len: i64,
    modified: i64,
    is_dir: bool,
}

impl SafeMetadata {
    /// 本物のメタデータから値を抽出して作成します。
    pub fn new(m: &std::fs::Metadata) -> Self {
        use std::time::UNIX_EPOCH;
        let secs = m
            .modified()
            .and_then(|t| {
                t.duration_since(UNIX_EPOCH)
                    .map_err(|_| std::io::ErrorKind::Other.into())
            })
            .map(|d| d.as_secs() as i64)
            .unwrap_or(crate::types::METADATA_ERROR);

        Self {
            len: m.len() as i64,
            modified: secs,
            is_dir: m.is_dir(),
        }
    }

    /// メタデータ取得に失敗した場合のエラー値（-1等）で作成します。
    pub fn recovered() -> Self {
        Self {
            len: crate::types::METADATA_ERROR,
            modified: crate::types::METADATA_ERROR,
            is_dir: false,
        }
    }

    /// ファイルサイズを取得します。
    pub fn len(&self) -> i64 {
        self.len
    }

    /// 更新日時（UNIXタイムスタンプ）を取得します。
    pub fn modified(&self) -> i64 {
        self.modified
    }

    /// ディレクトリかどうかを判定します。
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }
}

/// ignore::Error から「ファイルが見つからない」エラーかどうかを判定します。
pub fn is_not_found_err(err: &ignore::Error) -> bool {
    err.io_error()
        .map_or(false, |io_e| io_e.kind() == std::io::ErrorKind::NotFound)
}

pub struct CustomExpr;

impl CustomExpr {
    /// DuckDB の `DISTINCT ON (col) *` 構文を構築します。
    pub fn distinct_on_all<I>(col: I) -> sea_query::SimpleExpr
    where
        I: IntoIden + 'static,
    {
        sea_query::Expr::cust_with_exprs(
            "DISTINCT ON ($1) *",
            [sea_query::Expr::col(col).into()],
        )
    }
}

// ========================================================
// Tests
// ========================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_metadata_real() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let m = std::fs::metadata(&path).unwrap();
        let safe_m = SafeMetadata::new(&m);

        assert_eq!(safe_m.len(), 5);
        assert!(!safe_m.is_dir());
        assert!(safe_m.modified() > 0);
    }

    #[test]
    fn test_safe_metadata_recovered() {
        let safe_m = SafeMetadata::recovered();
        assert_eq!(safe_m.len(), crate::types::METADATA_ERROR);
        assert_eq!(safe_m.modified(), crate::types::METADATA_ERROR);
        assert!(!safe_m.is_dir());
    }

    #[test]
    fn test_parse_size() {
        // 標準単位 (大文字小文字・スペース混在)
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1KB"), Some(1024));
        assert_eq!(parse_size("1 kb"), Some(1024));
        assert_eq!(parse_size("1.5MB"), Some(1572864));
        assert_eq!(parse_size("1GB"), Some(1073741824));

        // 巨大サイズ (TB, PB)
        assert_eq!(parse_size("1TB"), Some(1099511627776));
        assert_eq!(parse_size("1PB"), Some(1125899906842624));

        // バイナリ接頭辞 (KiB, MiB, ...)
        assert_eq!(parse_size("1KiB"), Some(1024));
        assert_eq!(parse_size("1.5MiB"), Some(1572864));
        assert_eq!(parse_size("1GiB"), Some(1073741824));
        assert_eq!(parse_size("1 TiB"), Some(1099511627776));
        assert_eq!(parse_size("1 PiB"), Some(1125899906842624));

        // ショートハンド (K, M, G, ...)
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1M"), Some(1048576));

        // 異常系
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("100XYZ"), None);
    }
}
