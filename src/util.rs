use anyhow::Result;
use duckdb::Connection;
use sea_query::{
    PostgresQueryBuilder, SelectStatement, TableCreateStatement, 
    TableDropStatement, DeleteStatement, UpdateStatement, 
    InsertStatement, Iden, Query, Expr, IntoIden, ColumnDef
};
use std::path::Path;

// --- 1. 通常の関数 (ロジックの実体) ---

/// sea-query のステートメントをビルドして実行します。
pub fn execute<S: SqlStatement + ?Sized>(conn: &Connection, stmt: &S) -> Result<()> {
    conn.execute(&stmt.build(), [])?;
    Ok(())
}

/// SelectStatement の結果をアトミックに Parquet 保存します。
pub fn save_parquet(
    conn: &Connection, 
    query: &SelectStatement, 
    path: &Path
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
    path: &Path
) -> Result<()> {
    let query = Query::select()
        .expr(Expr::cust("*"))
        .from(table)
        .to_owned();
    save_parquet(conn, &query, path)
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
    SelectStatement, TableCreateStatement, TableDropStatement,
    DeleteStatement, UpdateStatement, InsertStatement
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
    fn create_table_as<I: Iden + Clone + 'static>(&self, conn: &Connection, name: I) -> Result<I>;
    fn create_temp_table_as<I: Iden + Clone + 'static>(&self, conn: &Connection, name: I) -> Result<I>;
}

impl SelectExt for SelectStatement {
    fn create_table_as<I: Iden + Clone + 'static>(&self, conn: &Connection, name: I) -> Result<I> {
        let sql = format!(
            "CREATE TABLE {} AS {}",
            iden_to_sql(name.clone()),
            self.to_string(PostgresQueryBuilder)
        );
        conn.execute(&sql, [])?;
        Ok(name)
    }

    fn create_temp_table_as<I: Iden + Clone + 'static>(&self, conn: &Connection, name: I) -> Result<I> {
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
    use crate::db::{Tbl, DuckDbFunc};
    use sea_query::{Func};
    Query::select()
        .expr(Expr::cust("*"))
        .from_function(
            Func::cust(DuckDbFunc::ReadParquet).arg(Expr::val(path)),
            Tbl::Diff,
        )
        .to_owned()
}