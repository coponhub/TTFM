use sea_query::Iden;

/// データベースのテーブル名を表す識別子。
#[derive(Iden, Clone, Copy)]
pub enum Tbl {
    /// ファイルエンティティ（実体）テーブル
    #[iden = "file_entities"]
    FileEntities,
    /// ファイルパス（場所）テーブル
    #[iden = "locations"]
    Locations,
    /// ファイルタグテーブル
    #[iden = "file_tags"]
    FileTags,
    /// アイテムエンティティテーブル
    #[iden = "item_entities"]
    ItemEntities,
    /// アイテムタグテーブル
    #[iden = "item_tags"]
    ItemTags,
    
    // --- インデックス処理用テンポラリテーブル ---
    TempScan,
    TempFileEntities,
    TempLocations,
    TempFileTags,
    TempItemEntities,
    TempItemTags,

    // --- エイリアス用 ---
    #[iden = "scan"]
    ScanAlias, 
    #[iden = "e"]
    EntAlias,
    #[iden = "l"]
    LocAlias,
    #[iden = "old"]
    OldAlias,
    #[iden = "origin"]
    OriginAlias,
    #[iden = "t"]
    TagAlias,
}

/// 共通で使用されるカラム名を表す識別子。
#[derive(Iden, Clone, Copy)]
pub enum Col {
    Id,
    Path,
    ParentDir,
    Filename,
    Extension,
    EntityId,
    TagType,
    TagValue,
    Inode,
    Size,
    Mtime,
    TargetId,
    TargetKind,
    Type,
    Value,
    Kind,
    Content,
    ItemId,
    Rank,
}

/// システムタグの表示優先度（RANK）を定義する列挙型。
#[derive(Debug, Clone, Copy)]
pub enum SystemRank {
    Filename = 7,
    TypeFromExt = 6,
    SizeStr = 5,
    ModifiedStr = 4,
    ParentDir = 3,
    Content = 2,
    Other = 1,
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