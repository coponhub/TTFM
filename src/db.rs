use sea_query::{Iden, TableCreateStatement, Table, ColumnDef as SeaColumnDef, IntoIden};
use crate::taggers::{TargetTable, ColumnDef};

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
    FileEntitiesDiff, // TempFileEntities
    LocationsDiff,    // TempLocations
    BaseTagsDiff,     // TempBaseTags
    ItemEntitiesDiff, // TempItemEntities
    SystemTagsDiff,   // TempSystemTags
    UserTagsDiff,     // TempUserTags

    // --- Work Tables / Aliases ---
    Scan,   // TempScan / scan
    Item,   // NewItemsRaw / Candidate (c) / Items (i) / Items2 (it2) / TempAddItem
    IdItem, // NewItemsWithId
    Target, // TempBatchRank
    Diff,   // SourceTable (st)
    Master, // m

    // --- Set Operation Aliases ---
    LeftSide,  // left_side
    RightSide, // right_side
    NotSide,   // not_side
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
    Parentdir, // parentdir (note: lowercase 'd' for schema matching)
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
    Types,  // types
    Labels, // labels
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
                    .col(SeaColumnDef::new(Col::Label).string())
                    // Rankカラムが必要になる可能性があるが、BaseTagsには通常含まれない
                    ;
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