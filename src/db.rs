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
    /// 基本タグテーブル（自動抽出タグ）
    #[iden = "base_tags"]
    BaseTags,
    /// アイテムエンティティテーブル
    #[iden = "item_entities"]
    ItemEntities,
    /// システム定義アイテム用タグテーブル
    #[iden = "system_tags"]
    SystemTags,
    /// ユーザー定義タグテーブル
    #[iden = "user_tags"]
    UserTags,
    
    // --- インデックス処理用テンポラリテーブル ---
    TempScan,
    TempFileEntities,
    TempLocations,
    TempBaseTags,
    TempItemEntities,
    TempSystemTags,
    TempUserTags,

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
    #[iden = "u"]
    UserTagAlias,
    #[iden = "s"]
    SysTagAlias,
}

/// 共通で使用されるカラム名を表す識別子。
#[derive(Iden, Clone, Copy)]
pub enum Col {
    Id,
    ItemId,
    FileId,
    DeviceId,
    Path,
    ParentDir,
    Filename,
    Extension,
    TagType,
    TagValue,
    Size,
    Mtime,
    Hash,
    Type,
    Value,
    Kind,
    Content,
    Rank,
    Origin,
    Name,
    ItemKind,
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
    Kind = 5,
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
            "kind" => SystemRank::Kind as i64,
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