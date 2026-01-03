use sea_query::Iden;

/// データベースのテーブル名を表す識別子。
#[derive(Iden)]
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
#[derive(Iden)]
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
}

/// DuckDB 固有の関数名を表す識別子。
#[derive(Iden)]
pub enum DuckDbFunc {
    #[iden = "read_parquet"]
    ReadParquet,
    #[iden = "coalesce"]
    Coalesce,
    #[iden = "list"]
    List,
}
