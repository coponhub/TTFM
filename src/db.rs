use sea_query::Iden;

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

/// システムタグの表示優先度（RANK）を定義する列挙型。
#[derive(Debug, Clone, Copy)]
pub enum SystemRank {
    /// 解決済みの名称（最優先）
    Name = 10,
    /// 拡張子からの種類
    TypeFromExt = 9,
    /// サイズ（読みやすい形式）
    SizeStr = 8,
    /// 更新日時（読みやすい形式）
    ModifiedStr = 7,
    /// 親ディレクトリ
    ParentDir = 6,
    /// アイテムの種類 (file/note等)
    ItemKind = 5,
    /// コンテンツ（本文など）
    Content = 4,
    /// 物理的なファイル名（nameがある場合は優先度を下げる）
    Filename = 1,
    /// その他
    Other = 0,
    /// フルパス（長いため優先度を極めて低く設定）
    Path = -1,
}

impl SystemRank {
    pub fn get_default_rank(name: &str) -> i64 {
        match name {
            "name" => SystemRank::Name as i64,
            "type_from_ext" => SystemRank::TypeFromExt as i64,
            "size_str" => SystemRank::SizeStr as i64,
            "modified_str" => SystemRank::ModifiedStr as i64,
            "parentdir" => SystemRank::ParentDir as i64,
            "item_kind" => SystemRank::ItemKind as i64,
            "content" => SystemRank::Content as i64,
            "filename" => SystemRank::Filename as i64,
            "path" => SystemRank::Path as i64,
            _ => SystemRank::Other as i64,
        }
    }
}

impl From<SystemRank> for i64 {
    fn from(r: SystemRank) -> Self {
        r as i64
    }
}

impl From<SystemRank> for sea_query::Value {
    fn from(r: SystemRank) -> Self {
        sea_query::Value::BigInt(Some(r as i64))
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
