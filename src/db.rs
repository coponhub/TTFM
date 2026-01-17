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
                    match &c.sql_type {
                        SqlType::BIGINT => def.big_integer(),
                        SqlType::BOOLEAN => def.boolean(),
                        SqlType::VARCHAR => def.string(),
                        SqlType::DOUBLE => def.double(),
                        SqlType::UUID => def.custom(SqlType::UUID),
                    };
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
                    match &c.sql_type {
                        SqlType::BIGINT => def.big_integer(),
                        SqlType::BOOLEAN => def.boolean(),
                        SqlType::VARCHAR => def.string(),
                        SqlType::DOUBLE => def.double(),
                        SqlType::UUID => def.custom(SqlType::UUID),
                    };
                    create.col(&mut def);
                }
                create.col(SeaColumnDef::new(Col::ScanHash).big_integer());
            }
            TargetTable::BaseTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Type).string())
                    .col(SeaColumnDef::new(Col::LabelStr).string())
                    .col(SeaColumnDef::new(Col::LabelInt).big_integer())
                    .col(SeaColumnDef::new(Col::LabelDouble).double())
                    .col(SeaColumnDef::new(Col::LabelBool).boolean());
            }
            TargetTable::ItemReferences => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Rank).big_integer())
                    .col(SeaColumnDef::new(Col::ItemKind).string())
                    .col(SeaColumnDef::new(Col::Content).string());
            }
            TargetTable::SystemTags | TargetTable::UserTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Type).string())
                    .col(SeaColumnDef::new(Col::LabelStr).string())
                    .col(SeaColumnDef::new(Col::LabelInt).big_integer())
                    .col(SeaColumnDef::new(Col::LabelDouble).double())
                    .col(SeaColumnDef::new(Col::LabelBool).boolean());
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
