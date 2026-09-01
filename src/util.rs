// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use anyhow::Result;
use duckdb::Connection;
use sea_query::{
    ColumnDef, DeleteStatement, Expr, Iden, InsertStatement, IntoIden,
    PostgresQueryBuilder, Query, SelectStatement, SimpleExpr,
    TableCreateStatement, TableDropStatement, UpdateStatement,
};
use std::collections::HashMap;
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
    metadata: Option<&HashMap<String, String>>,
) -> Result<()> {
    let sql = query.to_string(PostgresQueryBuilder);
    let path_str = path.to_string_lossy();
    let tmp_path = format!("{}.tmp", path_str);

    let mut kv_part = String::new();
    if let Some(meta) = metadata {
        if !meta.is_empty() {
            let pairs: Vec<String> = meta
                .iter()
                .map(|(k, v)| {
                    format!(
                        "'{}': '{}'",
                        k.replace("'", "''"),
                        v.replace("'", "''")
                    )
                })
                .collect();
            kv_part = format!(", KV_METADATA {{{}}}", pairs.join(", "));
        }
    }

    let copy_sql = format!(
        "COPY ({}) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd'{})",
        sql, tmp_path, kv_part
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
    save_parquet(conn, &query, path, None)
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
        save_parquet(conn, self, path, None)
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
            "CREATE OR REPLACE TABLE {} AS {}",
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
    use crate::db::{DuckDbFunc, Pronoun::*};
    use sea_query::Func;
    Query::select()
        .column(sea_query::Asterisk)
        .from_function(
            Func::cust(DuckDbFunc::ReadParquet).arg(Expr::val(path)),
            Diff,
        )
        .to_owned()
}

/// サイズ単位（B, KB, MB, GB, TB, PB とその別名。1024累乗）の1バイトあたり倍率。
/// 空文字列・`B` は等倍（生バイト）。未知の単位は None。
pub fn size_unit_multiplier(unit: &str) -> Option<i64> {
    match unit {
        "" | "B" => Some(1),
        "KB" | "KIB" | "K" => Some(1024),
        "MB" | "MIB" | "M" => Some(1024 * 1024),
        "GB" | "GIB" | "G" => Some(1024 * 1024 * 1024),
        "TB" | "TIB" | "T" => Some(1024 * 1024 * 1024 * 1024),
        "PB" | "PIB" | "P" => Some(1024 * 1024 * 1024 * 1024 * 1024),
        _ => None,
    }
}

use crate::types::DateTimeRange;

/// 様々な形式の日時文字列をパースし、対応する時間範囲を返します。
/// パース自体は `DateTime::from_str`（types.rs）に一本化されており、
/// ここではその結果の Eq 相当区間（floor..ceiling）を求めるだけ。
pub fn parse_datetime(s: &str) -> Option<DateTimeRange> {
    s.parse::<crate::types::DateTime>().ok()?.to_interval()
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

/// パターンが `*` のみで構成される全一致 glob（`**` も含む）かどうかを判定します。
/// この場合、逆像はどの値域でも全域になります。
pub fn is_full_match_glob(pattern: &str) -> bool {
    !pattern.is_empty() && pattern.chars().all(|c| c == '*')
}

/// パターンが glob メタ文字（`*`, `?`, `[`）を1つ以上含むかどうかを判定します。
pub fn is_glob_pattern(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

/// 数値部の1フィールド（整数部 or 小数部）。フィールド単位でしか自由にできないため
/// `*`（自由）か数字リテラルのみを表し、フィールド内の部分 glob は無い。
pub enum NumericField<'a> {
    Free,
    Literal(&'a str),
}

/// 数値部の1フィールドをパースする。`*` は自由、全桁が数字ならリテラル、
/// フィールド内の部分 glob 等それ以外は None。
pub fn parse_numeric_field(s: &str) -> Option<NumericField<'_>> {
    if s == "*" {
        Some(NumericField::Free)
    } else if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        Some(NumericField::Literal(s))
    } else {
        None
    }
}

/// GLOB パターンマッチングを行います（`*`, `?`, `[...]`, `[!...]` に対応、
/// `glob` crate の `Pattern` に委譲）。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    match glob::Pattern::new(pattern) {
        Ok(p) => p.matches(text),
        Err(_) => pattern == text,
    }
}

// ========================================================
// Tests
// ========================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_full_match_glob() {
        assert!(is_full_match_glob("*"));
        assert!(is_full_match_glob("**"));
        assert!(!is_full_match_glob("*.rs"));
        assert!(!is_full_match_glob("o*"));
        assert!(!is_full_match_glob("?"));
        assert!(!is_full_match_glob("[abc]"));
        assert!(!is_full_match_glob(""));
        assert!(!is_full_match_glob("foo"));
    }

    #[test]
    fn test_is_glob_pattern() {
        assert!(is_glob_pattern("*"));
        assert!(is_glob_pattern("o*"));
        assert!(is_glob_pattern("?"));
        assert!(is_glob_pattern("[abc]"));
        assert!(!is_glob_pattern("foo"));
        assert!(!is_glob_pattern(""));
    }

    #[test]
    fn test_parse_numeric_field() {
        assert!(matches!(parse_numeric_field("*"), Some(NumericField::Free)));
        assert!(matches!(
            parse_numeric_field("123"),
            Some(NumericField::Literal("123"))
        ));
        assert!(parse_numeric_field("").is_none());
        assert!(parse_numeric_field("1*").is_none());
        assert!(parse_numeric_field("abc").is_none());
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("builtin", "builtin"));
        assert!(glob_match("b*", "builtin"));
        assert!(glob_match("*in", "builtin"));
        assert!(glob_match("b*n", "builtin"));
        assert!(glob_match("b?iltin", "builtin"));
        assert!(!glob_match("user", "builtin"));
        assert!(!glob_match("b*", "user"));
        assert!(glob_match("b[uv]iltin", "builtin"));
        assert!(!glob_match("b[uv]iltin", "biltin"));
        assert!(glob_match("b[!v]iltin", "builtin"));
        assert!(!glob_match("b[!u]iltin", "builtin"));
    }

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
    fn test_size_unit_multiplier() {
        assert_eq!(size_unit_multiplier(""), Some(1));
        assert_eq!(size_unit_multiplier("B"), Some(1));
        assert_eq!(size_unit_multiplier("KB"), Some(1024));
        assert_eq!(size_unit_multiplier("KIB"), Some(1024));
        assert_eq!(size_unit_multiplier("K"), Some(1024));
        assert_eq!(size_unit_multiplier("MB"), Some(1024 * 1024));
        assert_eq!(size_unit_multiplier("MIB"), Some(1024 * 1024));
        assert_eq!(size_unit_multiplier("M"), Some(1024 * 1024));
        assert_eq!(size_unit_multiplier("GB"), Some(1024 * 1024 * 1024));
        assert_eq!(size_unit_multiplier("TB"), Some(1024i64.pow(4)));
        assert_eq!(size_unit_multiplier("TIB"), Some(1024i64.pow(4)));
        assert_eq!(size_unit_multiplier("PB"), Some(1024i64.pow(5)));
        assert_eq!(size_unit_multiplier("PIB"), Some(1024i64.pow(5)));
        assert_eq!(size_unit_multiplier("XYZ"), None);
    }

    #[test]
    fn test_parse_datetime() {
        // 相対指定
        assert!(parse_datetime("today").is_some());
        assert!(parse_datetime("yesterday").is_some());
        assert!(parse_datetime("1d ago").is_some());
        assert!(parse_datetime("2m ago").is_some());
        assert!(parse_datetime("1y ago").is_some());
        assert!(parse_datetime("12H ago").is_some());
        assert!(parse_datetime("30min ago").is_some());

        // 部分的指定 (M/D, HH:MM)
        let (md_start, md_end) =
            parse_datetime("1/10").unwrap().as_interval().unwrap();
        assert_ne!(md_start, md_end);
        let (hm_start, hm_end) =
            parse_datetime("12:30").unwrap().as_interval().unwrap();
        assert_ne!(hm_start, hm_end);

        // 絶対指定 (YYYY/M/D)
        let (ymd_start, ymd_end) =
            parse_datetime("2024/01/01").unwrap().as_interval().unwrap();
        assert!(ymd_start < ymd_end);

        // 秒まで指定 (開始 == 終了)
        let (hms_start, hms_end) =
            parse_datetime("12:30:05").unwrap().as_interval().unwrap();
        assert_eq!(hms_start, hms_end);

        let (ymd_hms_start, ymd_hms_end) =
            parse_datetime("2024/01/01 12:30:05")
                .unwrap()
                .as_interval()
                .unwrap();
        assert_eq!(ymd_hms_start, ymd_hms_end);

        // 各種区切り文字
        assert!(parse_datetime("2024-01-01").is_some());
        assert!(parse_datetime("2024.01.01").is_some()); // dateparserによりサポート済み

        assert!(parse_datetime("2024").is_none());

        let (ym_start, ym_end) =
            parse_datetime("2013/1").unwrap().as_interval().unwrap();
        assert!(ym_start < ym_end);
        // 2013/1/1 00:00:00 〜 2013/1/31 23:59:59 のはず
    }
}
