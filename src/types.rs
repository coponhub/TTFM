use duckdb::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use uuid::Uuid;

/// メタデータ取得に失敗した際のデフォルト値。
pub const METADATA_ERROR: i64 = -1;

/// アイテムの優先度を表す型。
pub type Rank = i64;

/// アイテムの一意なID。
pub type ItemId = i64;

/// ファイルの実体（Inode/FileID）を一意に表す 128ビット識別子。
pub type FileRef = Uuid;

/// アイテムの種類 (file, note 等) を表す型エイリアス。
pub type ItemKind = String;

/// アイテムの表示名を表す型エイリアス。
pub type ItemName = String;

/// アイテム内におけるタグの順序（インデックス）を表す型エイリアス。
pub type TagNumber = usize;

/// データの由来を表す Enum。
#[derive(
    Debug, PartialEq, Eq, Hash, Clone, Copy, strum::Display, strum::EnumString,
)]
#[strum(serialize_all = "snake_case")]
pub enum Origin {
    /// システムによる自動抽出
    System,
    /// ユーザーによる手動付与
    User,
}

/// データベース上の型名を取得するためのトレイト。
pub trait DBType {
    /// 対応する SQL の型を返します。
    fn db_type() -> crate::db::SqlType;
}

impl DBType for String {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::VARCHAR
    }
}
impl DBType for i64 {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::BIGINT
    }
}
impl DBType for Uuid {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::UUID
    }
}
impl DBType for bool {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::BOOLEAN
    }
}

/// ファイルサイズ（バイト単位）を表す型。
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct FileSize(pub i64);
impl DBType for FileSize {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::BIGINT
    }
}

impl FromSql for FileSize {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(FileSize)
    }
}

impl ToSql for FileSize {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

/// UNIXタイムスタンプ（秒単位）を表す型。
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct FileTimestamp(pub i64);
impl DBType for FileTimestamp {
    fn db_type() -> crate::db::SqlType {
        crate::db::SqlType::BIGINT
    }
}

impl FromSql for FileTimestamp {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(FileTimestamp)
    }
}

impl ToSql for FileTimestamp {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

/// タグの「キー（型）」部分を表す SuperType。
/// システム定義の標準タグ（SType）と、自由なカスタムタグの両方を扱えます。
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum TagType {
    Base(SType),
    Custom(String),
    /// 引用符で囲まれたリテラル。Glob無効・自動展開を行わない。
    LiteralCustom(String),
}

impl TagType {
    /// 文字列としての表現を取得します。
    pub fn as_str(&self) -> &str {
        match self {
            TagType::Base(s) => (*s).into(),
            TagType::Custom(s) => s.as_str(),
            TagType::LiteralCustom(s) => s.as_str(),
        }
    }
}

impl From<SType> for TagType {
    fn from(s: SType) -> Self {
        TagType::Base(s)
    }
}

impl From<String> for TagType {
    fn from(s: String) -> Self {
        SType::from_str(&s)
            .map(TagType::Base)
            .unwrap_or(TagType::Custom(s))
    }
}

impl From<&str> for TagType {
    fn from(s: &str) -> Self {
        s.to_string().into()
    }
}

/// タグの「値」部分（例: "rs", "1024"）。
/// 文字列と数値のどちらかを取り得ます。
#[derive(Debug, PartialEq, Clone)]
pub enum Label {
    String(String),
    Integer(i64),
    /// 引用符で囲まれたリテラル文字列。Globを無効化する。
    Literal(String),
}

impl Label {
    /// 文字列としての値を取得します。
    pub fn as_str(&self) -> String {
        match self {
            Label::String(s) => s.clone(),
            Label::Integer(i) => i.to_string(),
            Label::Literal(s) => s.clone(),
        }
    }

    /// 数値としての値を取得します（数値でない場合は 0）。
    pub fn as_i64(&self) -> i64 {
        match self {
            Label::Integer(i) => *i,
            Label::String(s) | Label::Literal(s) => {
                s.parse::<i64>().unwrap_or_default()
            }
        }
    }
}

impl From<String> for Label {
    fn from(s: String) -> Self {
        Label::String(s)
    }
}

impl From<&str> for Label {
    fn from(s: &str) -> Self {
        Label::String(s.to_string())
    }
}

impl From<i64> for Label {
    fn from(i: i64) -> Self {
        Label::Integer(i)
    }
}

/// 「キー:値」のペアを表す構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct TypedTag {
    /// タグの型（キー）。例: "extension"
    pub tagtype: TagType,
    /// タグの値。例: "rs"
    pub label: Label,
}

impl TypedTag {
    /// 新しい `TypedTag` を作成します。
    pub fn new(tagtype: impl Into<TagType>, label: impl Into<Label>) -> Self {
        Self {
            tagtype: tagtype.into(),
            label: label.into(),
        }
    }
}

/// 値と由来をセットで保持する構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct TagValue {
    /// タグの値
    pub label: Label,
    /// 由来
    pub origin: Origin,
}

/// タグの集合。
pub type Tags = std::collections::HashMap<TagType, Vec<TagValue>>;

/// アイテム固有の不動の情報をまとめた構造体。
#[derive(Debug, PartialEq, Clone, Default)]
pub struct Intrinsic {
    /// ファイルサイズ
    pub size: Option<FileSize>,
    /// 更新日時
    pub mtime: Option<FileTimestamp>,
    /// コンテンツのハッシュ
    pub hash: Option<String>,
}

/// 検索結果を表す構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct SearchResult {
    /// アイテムの一意なID
    pub id: ItemId,
    /// アイテムの種類
    pub item_kind: ItemKind,
    /// 解決済みの名称
    pub name: ItemName,
    /// アイテムの優先度
    pub rank: Rank,
    /// 固定の固有情報
    pub intrinsic: Intrinsic,
    /// アイテムに紐づく動的なタグの集合
    pub tags: Tags,
}

/// 検索クエリの結果全体を表す構造体。
#[derive(Debug, PartialEq, Clone, Default)]
pub struct SearchResponse {
    /// ヒットしたアイテムのリスト
    pub results: Vec<SearchResult>,
    /// クエリで明示的に投影（Projection）されたタグ型の一覧
    pub projections: Vec<String>,
}

impl SearchResult {
    /// 代表的な値（パスやコンテンツ）を取得するヘルパー。
    /// ファイルならパス、Noteならコンテンツなどを返します。
    pub fn primary_value(&self) -> Option<String> {
        // 抽象化された名前があればそれを最優先
        if !self.name.is_empty() {
            return Some(self.name.clone());
        }
        // フォールバックとしてタグの中を探す
        self.get_tag_value("path")
            .or_else(|| self.get_tag_value("content"))
            .or_else(|| self.get_tag_value("value"))
            .or_else(|| self.get_tag_value("filename"))
    }

    /// アイテム全体の集約された由来を取得します。
    /// 一つでもユーザー付与のタグがあれば Origin::User を返します。
    pub fn origin(&self) -> Origin {
        self.tags
            .values()
            .flatten()
            .any(|tv| tv.origin == Origin::User)
            .then_some(Origin::User)
            .unwrap_or(Origin::System)
    }

    /// 指定されたキーのタグ値を文字列として取得します。
    /// 固定メタデータ (size 等) も透過的にアクセス可能です。
    pub fn get_tag_value(&self, key: &str) -> Option<String> {
        let tag_type = TagType::from(key);

        // 1. 固有情報の早期リターン
        let fixed = match &tag_type {
            TagType::Base(SType::Size) => {
                self.intrinsic.size.as_ref().map(|s| s.0.to_string())
            }
            TagType::Base(SType::Mtime) => {
                self.intrinsic.mtime.as_ref().map(|t| t.0.to_string())
            }
            TagType::Base(SType::Hash) => self.intrinsic.hash.clone(),
            TagType::Base(SType::Rank) => Some(self.rank.to_string()),
            TagType::Base(SType::ItemKind) => Some(self.item_kind.clone()),
            TagType::Base(SType::Name) => Some(self.name.clone()),
            TagType::Base(SType::Origin) => Some(self.origin().to_string()),
            _ => None,
        };

        if fixed.is_some() {
            return fixed;
        }

        // 2. HashMap からのフォールバック
        self.tags.get(&tag_type)?.get(0).map(|tv| tv.label.as_str())
    }

    /// 指定されたキーの全てのタグ値を取得します。
    pub fn get_tag_values(&self, key: &str) -> Option<&[TagValue]> {
        self.tags.get(&TagType::from(key)).map(|v| v.as_slice())
    }
}

/// ライフタイムに制約のないタグ名（参照）。
pub type Name<'a> = &'a str;

/// プログラム終了まで有効なタグ名（静的文字列）。
pub type StaticName = &'static str;

/// システムで使用される標準的なタグ名のシンボル定義。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::Display,
)]
#[strum(serialize_all = "snake_case")]
pub enum SType {
    ItemId,
    FileId,
    Path,
    Parentdir,
    Filename,
    Stem,
    Extension,
    IsDir,
    Size,
    Mtime,
    TypeFromExt,
    SizeStr,
    ModifiedStr,
    Hash,
    Type,
    Label,
    ItemKind,
    Content,
    Rank,
    Origin,
    Name,
    // 内部カラム用
    Types,
    Labels,
    ScanHash,
    #[strum(serialize = "typedtag")]
    TypedTag,
    // 検索専用仮想タグ
    Directory,
    // Typed Label Columns
    LabelStr,
    LabelInt,
    LabelDouble,
    LabelBool,
    // Schema Table Columns
    DataType,
}
