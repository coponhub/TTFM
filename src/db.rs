use sea_query::{Iden, TableCreateStatement, Table, ColumnDef as SeaColumnDef, IntoIden};
use crate::taggers::{ColumnDef};
use strum::{EnumIter, Display};
use std::path::{PathBuf};

/// カラムが所属すべきテーブル。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, Display, Iden)]
#[strum(serialize_all = "snake_case")]
pub enum TargetTable {
    FileEntities,
    Locations,
    BaseTags,
    ItemEntities,
    SystemTags,
    UserTags,
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
}

/// データベースのテーブル名を表す識別子。
#[derive(Iden, Clone, Copy)]
pub enum Tbl {
    FileEntities,
    Locations,
    BaseTags,
    ItemEntities,
    SystemTags,
    UserTags,
    #[iden = "oneview"]
    OneView,
    
    // --- Diff Tables ---
    FileEntitiesDiff,
    LocationsDiff,
    BaseTagsDiff,
    ItemEntitiesDiff,
    SystemTagsDiff,
    UserTagsDiff,

    // --- Work Tables / Aliases ---
    Scan,
    Item,
    IdItem,
    Target,
    Diff,
    Master,

    // --- Set Operation Aliases ---
    LeftSide,
    RightSide,
    NotSide,
}

/// SQL型名（CAST用）。
#[allow(non_camel_case_types)]
#[derive(Iden, Clone, Copy)]
pub enum SqlType {
    BIGINT,
    VARCHAR,
    BOOLEAN,
}

/// 共通で使用されるカラム名を表す識別子。
#[derive(Iden, Clone, Copy)]
pub enum Col {
    ItemId,
    FileId,
    Path,
    Parentdir,
    Filename,
    Extension,
    Size,
    Mtime,
    Hash,
    Type,
    Label,
    ItemKind,
    Content,
    Rank,
    Origin,
    Name,
    Types,
    Labels,
}

impl Col {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "item_id" => Some(Col::ItemId),
            "file_id" => Some(Col::FileId),
            "path" => Some(Col::Path),
            "parentdir" => Some(Col::Parentdir),
            "filename" => Some(Col::Filename),
            "extension" => Some(Col::Extension),
            "size" => Some(Col::Size),
            "mtime" => Some(Col::Mtime),
            "hash" => Some(Col::Hash),
            "type" => Some(Col::Type),
            "label" => Some(Col::Label),
            "item_kind" => Some(Col::ItemKind),
            "content" => Some(Col::Content),
            "rank" => Some(Col::Rank),
            "origin" => Some(Col::Origin),
            "name" => Some(Col::Name),
            "types" => Some(Col::Types),
            "labels" => Some(Col::Labels),
            _ => None,
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
            TargetTable::FileEntities => {
                create.col(SeaColumnDef::new(Col::ItemId).big_integer());
                create.col(SeaColumnDef::new(Col::Rank).big_integer());
                for c in columns
                    .iter()
                    .filter(|c| c.target_table == TargetTable::FileEntities)
                {
                    let iden = Col::from_str(&c.name)
                        .map(|c| c.into_iden())
                        .unwrap_or_else(|| crate::util::alias_from(&c.name));
                    let mut def = SeaColumnDef::new(iden);
                    match c.sql_type {
                        "BIGINT" => def.big_integer(),
                        "BOOLEAN" => def.boolean(),
                        _ => def.string(),
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
                    match c.sql_type {
                        "BIGINT" => def.big_integer(),
                        "BOOLEAN" => def.boolean(),
                        _ => def.string(),
                    };
                    create.col(&mut def);
                }
            }
            TargetTable::BaseTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Type).string())
                    .col(SeaColumnDef::new(Col::Label).string());
            }
            TargetTable::ItemEntities => {
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
                    .col(SeaColumnDef::new(Col::Label).string());
            }
        }
        create
    }
}
