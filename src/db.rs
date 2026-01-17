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
#[derive(Iden, Clone, Copy)]
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

    // --- Work Tables / Aliases ---
    Scan,
    Live,
    Item,
    IdItem,
    Target,
    Diff,
    Master,
    Sub,

    // --- Set Operation Aliases ---
    LeftSide,
    RightSide,
    NotSide,
    Identities,
    AggTags,
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
    pub fn prepare_column<'a>(&self, def: &'a mut SeaColumnDef) -> &'a mut SeaColumnDef {
        match self {
            SqlType::VARCHAR => def.string(),
            SqlType::BIGINT => def.big_integer(),
            SqlType::DOUBLE => def.double(),
            SqlType::BOOLEAN => def.boolean(),
            SqlType::UUID => def.custom(SqlType::UUID),
        }
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
            .collect()
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
            Self::LabelInt | Self::ItemId | Self::Rank | Self::ScanHash => SqlType::BIGINT,
            Self::LabelDouble => SqlType::DOUBLE,
            Self::LabelBool => SqlType::BOOLEAN,
            _ => SqlType::VARCHAR,
        }
    }
}

/// DuckDB 固有の関数名を表す識別子。
#[derive(Iden, Clone, Copy)]
pub enum DuckDbFunc {
    #[iden = "read_parquet"]
    ReadParquet,
    #[iden = "coalesce"]
    Coalesce,
    #[iden = "list"]
    List,
    #[iden = "concat"]
    Concat,
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
        sea_query::Func::cust(DuckDbFunc::List).arg(expr.into()).into()
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

    /// ID割り当て用のウィンドウ関数式を生成します。
    pub fn assign_id_window(start_id: i64) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "$1 - (row_number() OVER (ORDER BY rank DESC, content ASC) - 1)",
            [sea_query::Expr::val(start_id).into()],
        )
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
