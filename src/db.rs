use crate::taggers::ColumnDef;
use sea_query::{
    ColumnDef as SeaColumnDef, Iden, IntoIden, Table, TableCreateStatement,
};
use std::path::PathBuf;
use strum::{Display, EnumIter};

/// カラムが所属すべきテーブル。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, Display, Iden)]
#[strum(serialize_all = "snake_case")]
pub enum TargetTable {
    FileReferences,
    Locations,
    BaseTags,
    ItemReferences,
    SystemTags,
    UserTags,
    DataTypes,
}

/// データベースの物理ストレージ（Parquetファイル）へのパスを管理する構造体。
pub struct Store {
    pub db_dir: PathBuf,
}

impl Store {
    pub fn new(db_dir: PathBuf) -> Self {
        Self { db_dir }
    }

    /// ターゲットテーブルに対応するパスを生成します。
    pub fn path_for_target(&self, target: TargetTable) -> PathBuf {
        self.db_dir.join(format!("{}.parquet", target))
    }

    /// 一時的なスキャン結果の保存先パスを返します。
    pub fn temp_scan_path(&self) -> PathBuf {
        self.db_dir.join("current_scan.parquet")
    }

    /// 一時的な生存 ID リストの保存先パスを返します。
    pub fn temp_live_path(&self) -> PathBuf {
        self.db_dir.join("live_ids.parquet")
    }
}

/// データベースのテーブル名を表す識別子。
#[derive(Iden, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tbl {
    FileReferences,
    Locations,
    BaseTags,
    ItemReferences,
    SystemTags,
    UserTags,
    DataTypes,
    #[iden = "oneview"]
    OneView,

    // --- Diff Tables ---
    FileReferencesDiff,
    LocationsDiff,
    BaseTagsDiff,
    ItemReferencesDiff,
    SystemTagsDiff,
    UserTagsDiff,
    DataTypesDiff,

    // --- Work Tables ---
    Scan,
    Live,
    Item,
    IdItem,
    Target,
    Master,
}

/// SQL 内部で使われる中間的な識別子（サブクエリエイリアス・中間カラム名）。
/// `use crate::db::Pronoun::*;` でインポートして使用する。
#[derive(Iden, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pronoun {
    // --- 元 Tbl から移行 ---
    Sub,
    Diff,
    AllHits,
    TopItems,
    PickedIds,
    GroupTotal,
    Rn,
    // --- サブクエリエイリアス ---
    View,
    Proj,
    Pivot,
    NvFilter,
    Pk,
    // --- JOIN エイリアス ---
    #[iden = "L"]
    L,
    #[iden = "R"]
    R,
    // --- 中間カラム ---
    Nvalue,
    Group,
    Cast,
    Label,
    Scalar,
    // --- 追加分 ---
    Agg,
    Ctx,
    Filter,
    Tags,
    Deduped,
    Val,
    Kind,
    Key,
}

/// Volatile Column
/// クエリの結果にのみあるような永続化されていないカラム名
#[derive(Iden, Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum VCol {
    Total,
}

/// SQL型名（CAST用）。データベース上のデータ型ID（`data_types` テーブルと連携）。
#[allow(non_camel_case_types)]
#[derive(Clone, Debug, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum SqlType {
    VARCHAR = 1,
    BIGINT = 2,
    DOUBLE = 3,
    BOOLEAN = 4,
    UUID = 5,
}

impl Iden for SqlType {
    fn unquoted(&self, s: &mut dyn std::fmt::Write) {
        match self {
            SqlType::VARCHAR => write!(s, "VARCHAR").unwrap(),
            SqlType::BIGINT => write!(s, "BIGINT").unwrap(),
            SqlType::DOUBLE => write!(s, "DOUBLE").unwrap(),
            SqlType::BOOLEAN => write!(s, "BOOLEAN").unwrap(),
            SqlType::UUID => write!(s, "UUID").unwrap(),
        }
    }
}

impl SqlType {
    pub fn prepare_column<'a>(
        &self,
        def: &'a mut SeaColumnDef,
    ) -> &'a mut SeaColumnDef {
        match self {
            SqlType::VARCHAR => def.string(),
            SqlType::BIGINT => def.big_integer(),
            SqlType::DOUBLE => def.double(),
            SqlType::BOOLEAN => def.boolean(),
            SqlType::UUID => def.custom(SqlType::UUID),
        }
    }
}

/// 値として使用される定数文字列の識別子。
/// マジックストリングを排除するために使用します。
#[derive(
    Clone, Copy, Debug, strum::Display, strum::EnumString, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum Val {
    System,
    User,
    File,
    Note,
    ItemKind,
    Rank,
    Filename,
    Name,
    Unknown,
    Key,
    Value,
}

impl sea_query::Iden for Val {
    fn unquoted(&self, s: &mut dyn std::fmt::Write) {
        let val: &'static str = (*self).into();
        write!(s, "{}", val).unwrap();
    }
}

/// クエリ結果の動的カラム名を表す識別子。
/// データベーステーブルのカラムではなく、
/// SQL結果を整形するために使用される一時的な名前。
#[derive(
    Clone, Copy, Debug, strum::Display, strum::EnumString, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum QueryResultCol {
    Tags,
}

impl sea_query::Iden for QueryResultCol {
    fn unquoted(&self, s: &mut dyn std::fmt::Write) {
        let val: &'static str = (*self).into();
        write!(s, "{}", val).unwrap();
    }
}

/// 共通で使用されるカラム名を表す識別子。
pub use crate::types::SType as Col;

impl sea_query::Iden for Col {
    fn unquoted(&self, s: &mut dyn std::fmt::Write) {
        let val: &'static str = (*self).into();
        write!(s, "{}", val).unwrap();
    }
}

impl Col {
    pub fn from_str(s: &str) -> Option<Self> {
        <Self as std::str::FromStr>::from_str(s).ok()
    }

    pub fn item_references_columns() -> [Self; 5] {
        [
            Self::ItemId,
            Self::Rank,
            Self::Name,
            Self::ItemKind,
            Self::Content,
        ]
    }

    pub fn typed_label_columns() -> [Self; 4] {
        [
            Self::LabelStr,
            Self::LabelInt,
            Self::LabelDouble,
            Self::LabelBool,
        ]
    }

    pub fn tag_value_columns() -> Vec<Self> {
        std::iter::once(Self::Types)
            .chain(Self::typed_label_columns())
            .chain(std::iter::once(Self::Origin))
            .chain(std::iter::once(Self::TypedTag))
            .collect()
    }

    pub fn raw_tag_row_columns() -> [Self; 8] {
        [
            Self::ItemId,
            Self::ItemKind,
            Self::Type,
            Self::LabelStr,
            Self::LabelInt,
            Self::LabelDouble,
            Self::LabelBool,
            Self::Origin,
        ]
    }

    pub fn from_sql_type(st: SqlType) -> Self {
        match st {
            SqlType::VARCHAR | SqlType::UUID => Self::LabelStr,
            SqlType::BIGINT => Self::LabelInt,
            SqlType::DOUBLE => Self::LabelDouble,
            SqlType::BOOLEAN => Self::LabelBool,
        }
    }

    pub fn sql_type(&self) -> SqlType {
        match self {
            Self::LabelStr => SqlType::VARCHAR,
            Self::LabelInt | Self::ItemId | Self::Rank | Self::ScanHash => {
                SqlType::BIGINT
            }
            Self::LabelDouble => SqlType::DOUBLE,
            Self::LabelBool => SqlType::BOOLEAN,
            _ => SqlType::VARCHAR,
        }
    }
}

/// DuckDB 固有の関数名を表す識別子。
#[derive(Iden, Clone, Copy)]
pub enum DuckDbFunc {
    ReadParquet,
    Coalesce,
    List,
    Concat,
    ParquetKvMetadata,
    StructPack,
    ListSlice,
    RowNumber,
    Count,
    AnyValue,
    ListValue,
    #[iden = "typeof"]
    TypeOf,
    StringAgg,
}

#[derive(Iden, Clone, Copy)]
pub enum DuckDbKeyword {
    #[iden = "DISTINCT ON"]
    DistinctOn,
}

/// DuckDB 固有の複雑な構文を型安全に構築するためのヘルパー。
pub struct CustomFunc;

impl CustomFunc {
    /// TRY_CAST(expr AS BIGINT) を生成します。
    pub fn try_cast_bigint<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "TRY_CAST($1 AS BIGINT)",
            [expr.into()],
        )
    }

    /// list(expr) を生成します。
    pub fn list<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::List)
            .arg(expr.into())
            .into()
    }

    /// list(expr ORDER BY ...) を生成します。
    pub fn list_with_order<E, O>(
        expr: E,
        order_bys: Vec<(O, sea_query::Order)>,
    ) -> sea_query::SimpleExpr
    where
        E: Into<sea_query::SimpleExpr>,
        O: sea_query::IntoIden,
    {
        let mut sql = "list($1".to_string();
        if !order_bys.is_empty() {
            sql.push_str(" ORDER BY ");
            let orders = order_bys
                .into_iter()
                .map(|(col, ord)| {
                    let mut s = String::new();
                    col.into_iden().unquoted(&mut s);
                    format!(
                        "\"{}\" {}",
                        s,
                        if matches!(ord, sea_query::Order::Asc) {
                            "ASC"
                        } else {
                            "DESC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&orders);
        }
        sql.push(')');
        sea_query::Expr::cust_with_exprs(sql, [expr.into()])
    }

    /// struct_pack(col1 := col1, ...) を生成します。
    pub fn struct_pack<I>(columns: &[I]) -> sea_query::SimpleExpr
    where
        I: sea_query::IntoIden + Clone,
    {
        let fields = columns
            .iter()
            .map(|c| {
                let mut s = String::new();
                c.clone().into_iden().unquoted(&mut s);
                format!("\"{}\" := \"{}\"", s, s)
            })
            .collect::<Vec<_>>()
            .join(", ");
        sea_query::Expr::cust(format!("struct_pack({})", fields))
    }

    /// list_slice(list, start, end) を生成します。
    pub fn list_slice<E: Into<sea_query::SimpleExpr>>(
        expr: E,
        start: usize,
        end: usize,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::ListSlice)
            .args([
                expr.into(),
                sea_query::Expr::val(start as i64).into(),
                sea_query::Expr::val(end as i64).into(),
            ])
            .into()
    }

    /// MAX(expr) FILTER (WHERE cond) を生成します。
    pub fn max_filter<E, F>(expr: E, filter_expr: F) -> sea_query::SimpleExpr
    where
        E: Into<sea_query::SimpleExpr>,
        F: Into<sea_query::SimpleExpr>,
    {
        sea_query::Expr::cust_with_exprs(
            "MAX($1) FILTER (WHERE $2)",
            [expr.into(), filter_expr.into()],
        )
    }

    /// ANY_VALUE(expr) FILTER (WHERE cond) を生成します。
    pub fn any_value_filter<E, F>(
        expr: E,
        filter_expr: F,
    ) -> sea_query::SimpleExpr
    where
        E: Into<sea_query::SimpleExpr>,
        F: Into<sea_query::SimpleExpr>,
    {
        sea_query::Expr::cust_with_exprs(
            "ANY_VALUE($1) FILTER (WHERE $2)",
            [expr.into(), filter_expr.into()],
        )
    }

    /// any_value(expr) を生成します。
    pub fn any_value<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::AnyValue)
            .arg(expr.into())
            .into()
    }

    /// ID割り当て用のウィンドウ関数式を生成します。
    pub fn assign_id_window(start_id: i64) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "$1 - (row_number() OVER (ORDER BY rank DESC, content ASC) - 1)",
            [sea_query::Expr::val(start_id).into()],
        )
    }

    /// row_number() OVER (PARTITION BY ... ORDER BY ...) を生成します。
    pub fn row_number_over<P, O>(
        partition_by: P,
        order_bys: Vec<(O, sea_query::Order)>,
    ) -> sea_query::SimpleExpr
    where
        P: sea_query::IntoIden,
        O: sea_query::IntoIden,
    {
        let mut sql = "row_number() OVER (PARTITION BY ".to_string();
        let mut p_name = String::new();
        partition_by.into_iden().unquoted(&mut p_name);
        sql.push_str(&format!("\"{}\"", p_name));

        if !order_bys.is_empty() {
            sql.push_str(" ORDER BY ");
            let orders = order_bys
                .into_iter()
                .map(|(col, ord)| {
                    let mut s = String::new();
                    col.into_iden().unquoted(&mut s);
                    format!(
                        "\"{}\" {}",
                        s,
                        if matches!(ord, sea_query::Order::Asc) {
                            "ASC"
                        } else {
                            "DESC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&orders);
        }
        sql.push(')');
        sea_query::Expr::cust(sql)
    }

    /// union_value(arm := expr)::UNION(...) — SqlType に対応する UNION アームを生成し、
    /// 完全な UNION 型にキャストします。CASE 式の型統一に必要です。
    pub fn union_value<E: Into<sea_query::SimpleExpr>>(
        sql_type: SqlType,
        expr: E,
    ) -> sea_query::SimpleExpr {
        let arm = match sql_type {
            SqlType::BIGINT => "i",
            SqlType::DOUBLE => "d",
            SqlType::BOOLEAN => "b",
            SqlType::VARCHAR | SqlType::UUID => "s",
        };
        sea_query::Expr::cust_with_exprs(
            &format!(
                "union_value({arm} := $1)\
                 ::UNION(i BIGINT, d DOUBLE, b BOOLEAN, s VARCHAR)"
            ),
            [expr.into()],
        )
    }

    /// struct_pack("tag_type" := t, "value" := v, "origin" := o) を生成します。
    pub fn struct_pack_tag(
        tag_type: sea_query::SimpleExpr,
        value: sea_query::SimpleExpr,
        origin: sea_query::SimpleExpr,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "struct_pack(\"tag_type\" := $1, \"value\" := $2, \"origin\" := $3)",
            [tag_type, value, origin],
        )
    }

    /// list_value(v1, v2, ...) を生成します。
    pub fn list_value(
        exprs: impl IntoIterator<Item = sea_query::SimpleExpr>,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::ListValue)
            .args(exprs)
            .into()
    }

    /// typeof(expr) を生成します。
    pub fn type_of<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::TypeOf)
            .arg(expr.into())
            .into()
    }

    /// EAV列の (Col, SqlType) ペア配列から CASE WHEN IS NOT NULL THEN union_value(...) 式を生成します。
    pub fn eav_union_value(arms: &[(Col, SqlType)]) -> sea_query::SimpleExpr {
        let Some(((first_col, first_type), rest)) = arms.split_first() else {
            return sea_query::Expr::val(Option::<String>::None).into();
        };
        let init = sea_query::Expr::case(
            sea_query::Expr::col(*first_col).is_not_null(),
            Self::union_value(*first_type, sea_query::Expr::col(*first_col)),
        );
        rest.iter()
            .fold(init, |cs, (col, sql_type)| {
                cs.case(
                    sea_query::Expr::col(*col).is_not_null(),
                    Self::union_value(*sql_type, sea_query::Expr::col(*col)),
                )
            })
            .finally(sea_query::Expr::val(Option::<String>::None))
            .into()
    }

    /// TRY_CAST(expr AS DOUBLE) を生成します。
    pub fn try_cast_double<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "TRY_CAST($1 AS DOUBLE)",
            [expr.into()],
        )
    }

    /// COUNT(*) を生成します。
    pub fn count_star() -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::Count)
            .arg(sea_query::Expr::cust("*"))
            .into()
    }

    /// string_agg(expr, separator) を生成します。
    pub fn string_agg<E, S>(expr: E, separator: S) -> sea_query::SimpleExpr
    where
        E: Into<sea_query::SimpleExpr>,
        S: Into<sea_query::SimpleExpr>,
    {
        sea_query::Func::cust(DuckDbFunc::StringAgg)
            .args([expr.into(), separator.into()])
            .into()
    }

    /// count(*) OVER (PARTITION BY col) を生成します。
    pub fn count_over<P>(partition_by: P) -> sea_query::SimpleExpr
    where
        P: sea_query::IntoIden,
    {
        Self::count_over_multi(&[partition_by.into_iden()])
    }

    /// count(*) OVER (PARTITION BY col1, col2, ...) を生成します。
    pub fn count_over_multi(
        partition_cols: &[sea_query::DynIden],
    ) -> sea_query::SimpleExpr {
        let cols = partition_cols
            .iter()
            .map(|c| {
                let mut s = String::new();
                c.unquoted(&mut s);
                format!("\"{}\"", s)
            })
            .collect::<Vec<_>>()
            .join(", ");
        sea_query::Expr::cust(format!("count(*) OVER (PARTITION BY {})", cols))
    }

    /// row_number() OVER (PARTITION BY col1, col2, ... ORDER BY ...) を生成します。
    pub fn row_number_over_multi<O>(
        partition_cols: &[sea_query::DynIden],
        order_bys: Vec<(O, sea_query::Order)>,
    ) -> sea_query::SimpleExpr
    where
        O: sea_query::IntoIden,
    {
        let cols = partition_cols
            .iter()
            .map(|c| {
                let mut s = String::new();
                c.unquoted(&mut s);
                format!("\"{}\"", s)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!("row_number() OVER (PARTITION BY {}", cols);
        if !order_bys.is_empty() {
            sql.push_str(" ORDER BY ");
            let orders = order_bys
                .into_iter()
                .map(|(col, ord)| {
                    let mut s = String::new();
                    col.into_iden().unquoted(&mut s);
                    format!(
                        "\"{}\" {}",
                        s,
                        if matches!(ord, sea_query::Order::Asc) {
                            "ASC"
                        } else {
                            "DESC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&orders);
        }
        sql.push(')');
        sea_query::Expr::cust(sql)
    }
}

/// データベーススキーマ定義（テーブル作成SQL）を提供する構造体。
pub struct Schema;

impl Schema {
    pub fn build_table(
        target: TargetTable,
        name: impl Iden + 'static,
        columns: &[ColumnDef],
    ) -> TableCreateStatement {
        let mut create = Table::create().table(name).to_owned();
        match target {
            TargetTable::FileReferences => {
                create.col(SeaColumnDef::new(Col::ItemId).big_integer());
                create.col(SeaColumnDef::new(Col::Rank).big_integer());
                for c in columns
                    .iter()
                    .filter(|c| c.target_table == TargetTable::FileReferences)
                {
                    let iden = Col::from_str(&c.name)
                        .map(|c| c.into_iden())
                        .unwrap_or_else(|| crate::util::alias_from(&c.name));
                    let mut def = SeaColumnDef::new(iden);
                    c.sql_type.prepare_column(&mut def);
                    create.col(&mut def);
                }
            }
            TargetTable::Locations => {
                create.col(SeaColumnDef::new(Col::ItemId).big_integer());
                for c in columns
                    .iter()
                    .filter(|c| c.target_table == TargetTable::Locations)
                {
                    let iden = Col::from_str(&c.name)
                        .map(|c| c.into_iden())
                        .unwrap_or_else(|| crate::util::alias_from(&c.name));
                    let mut def = SeaColumnDef::new(iden);
                    c.sql_type.prepare_column(&mut def);
                    create.col(&mut def);
                }
                create.col(SeaColumnDef::new(Col::ScanHash).big_integer());
            }
            TargetTable::BaseTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Type).string());
                for l_col in Col::typed_label_columns() {
                    let mut def = SeaColumnDef::new(l_col);
                    l_col.sql_type().prepare_column(&mut def);
                    create.col(&mut def);
                }
            }
            TargetTable::ItemReferences => {
                for col in Col::item_references_columns() {
                    let mut def = SeaColumnDef::new(col);
                    col.sql_type().prepare_column(&mut def);
                    create.col(&mut def);
                }
            }
            TargetTable::SystemTags | TargetTable::UserTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Type).string());
                for l_col in Col::typed_label_columns() {
                    let mut def = SeaColumnDef::new(l_col);
                    l_col.sql_type().prepare_column(&mut def);
                    create.col(&mut def);
                }
            }
            TargetTable::DataTypes => {
                create
                    .col(SeaColumnDef::new(Col::Type).string())
                    .col(SeaColumnDef::new(Col::DataType).integer());
            }
        }
        create
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::{Expr, PostgresQueryBuilder, Query};

    #[test]
    fn test_union_value_varchar() {
        let expr =
            CustomFunc::union_value(SqlType::VARCHAR, Expr::val("hello"));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("union_value(s :="),
            "should contain union_value(s :=: {}",
            sql
        );
        assert!(sql.contains("'hello'"), "should contain value: {}", sql);
    }

    #[test]
    fn test_union_value_boolean() {
        let expr = CustomFunc::union_value(SqlType::BOOLEAN, Expr::val(true));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("union_value(b :="),
            "should contain union_value(b :=: {}",
            sql
        );
    }

    #[test]
    fn test_union_value_bigint() {
        let expr = CustomFunc::union_value(SqlType::BIGINT, Expr::val(42i64));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("union_value(i :="),
            "should contain union_value(i :=: {}",
            sql
        );
        assert!(sql.contains("42"), "should contain 42: {}", sql);
    }

    #[test]
    fn test_union_value_double() {
        let expr = CustomFunc::union_value(SqlType::DOUBLE, Expr::val(3.14f64));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("union_value(d :="),
            "should contain union_value(d :=: {}",
            sql
        );
    }

    #[test]
    fn test_struct_pack_tag_generates_three_fields() {
        let expr = CustomFunc::struct_pack_tag(
            Expr::val("name").into(),
            CustomFunc::union_value(SqlType::VARCHAR, Expr::val("foo")),
            Expr::val("system").into(),
        );
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("struct_pack"),
            "should contain struct_pack: {}",
            sql
        );
        assert!(
            sql.contains("\"tag_type\""),
            "should contain tag_type field: {}",
            sql
        );
        assert!(
            sql.contains("\"value\""),
            "should contain value field: {}",
            sql
        );
        assert!(
            sql.contains("\"origin\""),
            "should contain origin field: {}",
            sql
        );
    }

    #[test]
    fn test_eav_union_value_generates_case_when() {
        let expr = CustomFunc::eav_union_value(&[
            (Col::LabelInt, SqlType::BIGINT),
            (Col::LabelStr, SqlType::VARCHAR),
            (Col::LabelBool, SqlType::BOOLEAN),
            (Col::LabelDouble, SqlType::DOUBLE),
        ]);
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(sql.contains("CASE"), "should have CASE: {}", sql);
        assert!(
            sql.contains("union_value(i :="),
            "should have int arm: {}",
            sql
        );
        assert!(
            sql.contains("union_value(s :="),
            "should have str arm: {}",
            sql
        );
        assert!(
            sql.contains("union_value(b :="),
            "should have bool arm: {}",
            sql
        );
        assert!(
            sql.contains("union_value(d :="),
            "should have double arm: {}",
            sql
        );
    }

    #[test]
    fn test_list_value_generates_function_call() {
        let expr = CustomFunc::list_value([
            Expr::val(1i64).into(),
            Expr::val(2i64).into(),
        ]);
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("list_value"),
            "should contain list_value: {}",
            sql
        );
    }

    #[test]
    fn test_count_star() {
        let sql = Query::select()
            .expr(CustomFunc::count_star())
            .to_string(PostgresQueryBuilder);
        assert!(sql.contains("count(*)"), "should contain count(*): {}", sql);
    }

    #[test]
    fn test_string_agg() {
        let expr =
            CustomFunc::string_agg(Expr::col(Col::LabelStr), Expr::val(","));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("string_agg"),
            "should contain string_agg: {}",
            sql
        );
    }

    #[test]
    fn test_try_cast_double() {
        let expr = CustomFunc::try_cast_double(Expr::col(Col::LabelInt));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("TRY_CAST") && sql.contains("DOUBLE"),
            "should contain TRY_CAST ... DOUBLE: {}",
            sql
        );
    }

    #[test]
    fn test_count_over_multi() {
        use sea_query::{Alias, IntoIden};
        let cols = vec![
            Alias::new("key0").into_iden(),
            Alias::new("key1").into_iden(),
        ];
        let sql = Query::select()
            .expr(CustomFunc::count_over_multi(&cols))
            .to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("count(*)") && sql.contains("PARTITION BY"),
            "should contain count(*) OVER PARTITION BY: {}",
            sql
        );
        assert!(
            sql.contains("key0") && sql.contains("key1"),
            "should contain both keys: {}",
            sql
        );
    }

    #[test]
    fn test_row_number_over_multi() {
        use sea_query::{Alias, IntoIden, Order};
        let cols = vec![Alias::new("label_value").into_iden()];
        let order_bys = vec![(Col::ItemId, Order::Desc)];
        let sql = Query::select()
            .expr(CustomFunc::row_number_over_multi(&cols, order_bys))
            .to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("row_number()") && sql.contains("PARTITION BY"),
            "should contain row_number() OVER PARTITION BY: {}",
            sql
        );
        assert!(
            sql.contains("label_value"),
            "should contain partition key: {}",
            sql
        );
    }
}
