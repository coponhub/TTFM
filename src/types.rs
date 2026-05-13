use duckdb::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use std::sync::atomic::{AtomicU64, Ordering};
use strum::{Display, EnumIter, EnumString, IntoStaticStr};
use uuid::Uuid;

/// メタデータ取得に失敗した際のデフォルト値。
pub const METADATA_ERROR: i64 = -1;

/// アイテムの優先度を表す型。
pub type Rank = i64;

/// アイテムの一意なID。
/// 実際のDBアイテム (Stored) または揮発性アイテム (Volatile) を表現。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub enum ItemId {
    /// データベースに存在する実アイテム（永続化済み）
    Stored(i64),
    /// 集約結果など、DBに存在しない揮発性アイテム
    Volatile(u64),
}

// 揮発性ID生成用のカウンター
static VOLATILE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ItemId {
    /// 新しい揮発性IDを生成します。
    pub fn new_volatile() -> Self {
        Self::Volatile(VOLATILE_ID_COUNTER.fetch_add(1, Ordering::SeqCst))
    }

    /// 整数表現を取得します。
    pub fn as_i64(&self) -> i64 {
        match self {
            Self::Stored(id) => *id,
            Self::Volatile(id) => *id as i64,
        }
    }

    /// Stored かどうか
    pub fn is_stored(&self) -> bool {
        matches!(self, ItemId::Stored(_))
    }

    /// Volatile かどうか
    pub fn is_volatile(&self) -> bool {
        matches!(self, ItemId::Volatile(_))
    }
}

impl From<i64> for ItemId {
    fn from(i: i64) -> Self {
        ItemId::Stored(i)
    }
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemId::Stored(i) => write!(f, "{}", i),
            ItemId::Volatile(v) => write!(f, "{}", v),
        }
    }
}

impl FromSql for ItemId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let i = i64::column_result(value)?;
        Ok(ItemId::from(i))
    }
}

impl ToSql for ItemId {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Owned(duckdb::types::Value::BigInt(
            self.as_i64(),
        )))
    }
}

/// ファイルの実体（Inode/FileID）を一意に表す 128ビット識別子。
pub type FileRef = Uuid;

/// アイテムの種類 (file, note 等) を表す。
#[derive(
    Debug,
    PartialEq,
    Eq,
    Clone,
    Copy,
    Hash,
    PartialOrd,
    Ord,
    Display,
    EnumString,
    IntoStaticStr,
    EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum ItemKind {
    // --- Stored (DB) ---
    File,
    Note,
    Type,
    Tag,
    // --- Volatile (Result) ---
    Volatile,
}

impl ItemKind {
    pub fn is_stored(&self) -> bool {
        matches!(self, Self::File | Self::Note | Self::Type | Self::Tag)
    }

    pub fn is_volatile(&self) -> bool {
        matches!(self, Self::Volatile)
    }

    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

// strum(EnumString) が TryFrom<&str> を自動実装するため、手動実装は削除。

impl FromSql for ItemKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        use std::str::FromStr;
        // DB上の古い値（label等）も Volatile に丸めるためのフォールバック
        Ok(Self::from_str(s).unwrap_or(Self::Volatile))
    }
}

impl ToSql for ItemKind {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl From<ItemKind> for Label {
    fn from(kind: ItemKind) -> Self {
        Label::resolve(
            TagType::from(crate::types::SType::ItemKind),
            LabelValue::String(kind.to_string()),
        )
    }
}

/// アイテムの表示名を表す型エイリアス。
pub type ItemName = String;

/// アイテム内におけるタグの順序（インデックス）を表す型エイリアス。
pub type TagNumber = usize;

/// データの由来を表す Enum。
#[derive(
    Debug,
    PartialEq,
    Eq,
    Hash,
    Clone,
    Copy,
    Display,
    EnumString,
    IntoStaticStr,
    EnumIter,
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
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
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
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
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
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
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

impl std::fmt::Display for TagType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl From<SType> for TagType {
    fn from(s: SType) -> Self {
        TagType::Base(s)
    }
}

impl From<String> for TagType {
    fn from(s: String) -> Self {
        // Clean up the string (trim whitespace) just in case
        let s = s.trim().to_string();
        // Fully qualified syntax to ensure we use the Trait implementation which returns Result
        match <SType as std::str::FromStr>::from_str(&s) {
            Ok(st) => TagType::Base(st),
            Err(_) => TagType::Custom(s),
        }
    }
}

impl From<&str> for TagType {
    fn from(s: &str) -> Self {
        s.to_string().into()
    }
}

/// タグの「値」部分（例: "rs", "1024"）。
/// 物理的な型だけでなく、ドメイン上の意味（SType）を宿しています。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum Label {
    // --- ドメイン特化型 (Standard Types) ---
    Name(String),
    Rank(i64),
    Size(i64),
    Mtime(i64),
    Hash(String),
    ItemKind(String),
    Extension(String),
    Path(String),
    ItemId(i64),
    FileId(Uuid),
    IsDir(bool),

    // --- 汎用・未解決型 ---
    /// 標準外のタグ、または明示的にドメインを特定しない汎用値。
    /// タグの型（TagType）を自律的に保持します。
    Other(TagType, LabelValue),
}

/// Label が保持する生の値の種類。
#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Clone,
    strum::Display,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum LabelValue {
    String(String),
    Integer(i64),  // -> "integer"
    Boolean(bool), // -> "boolean"
    Double(u64),   // -> "double" (f64::to_bits() で保持)
    Null,          // -> "null"
    Literal(String),
}

impl LabelValue {
    /// 検索結果の名前 (SearchResult.name) などに使用する、人間が読みやすい文字列表現。
    pub fn as_display_name(&self) -> String {
        match self {
            LabelValue::String(s) | LabelValue::Literal(s) => s.clone(),
            LabelValue::Integer(i) => i.to_string(),
            LabelValue::Boolean(true) => "TRUE".to_string(),
            LabelValue::Boolean(false) => "FALSE".to_string(),
            LabelValue::Double(bits) => f64::from_bits(*bits).to_string(),
            LabelValue::Null => "NULL".to_string(),
        }
    }
}

impl From<duckdb::types::Value> for LabelValue {
    fn from(v: duckdb::types::Value) -> Self {
        use duckdb::types::Value;
        match v {
            Value::Union(inner) => LabelValue::from(*inner),
            Value::Boolean(b) => LabelValue::Boolean(b),
            Value::Int(i) => LabelValue::Integer(i as i64),
            Value::BigInt(i) => LabelValue::Integer(i),
            Value::HugeInt(i) => LabelValue::Integer(i as i64),
            Value::Float(f) => LabelValue::Double((f as f64).to_bits()),
            Value::Double(d) => LabelValue::Double(d.to_bits()),
            Value::Text(s) => LabelValue::String(s),
            Value::Null => LabelValue::Null,
            Value::List(l) => {
                // LabelValue はスカラーなので、リストが来たら最初の要素を取り出す（保険的措置）
                // 本来的には Fetcher 側で分解されるべき。
                if let Some(first) = l.into_iter().next() {
                    LabelValue::from(first)
                } else {
                    LabelValue::Null
                }
            }
            _ => LabelValue::String(format!("{:?}", v)),
        }
    }
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Label {
    /// 文字列としての表現を取得します。
    pub fn as_str(&self) -> String {
        match self {
            Label::Name(s)
            | Label::Hash(s)
            | Label::ItemKind(s)
            | Label::Extension(s)
            | Label::Path(s) => s.clone(),
            Label::Rank(i)
            | Label::Size(i)
            | Label::Mtime(i)
            | Label::ItemId(i) => i.to_string(),
            Label::FileId(u) => u.to_string(),
            Label::IsDir(b) => LabelValue::Boolean(*b).as_display_name(),
            Label::Other(_, val) => val.as_display_name(),
        }
    }

    /// 数値としての値を取得します（数値でない場合は 0）。
    pub fn as_i64(&self) -> i64 {
        match self {
            Label::Rank(i)
            | Label::Size(i)
            | Label::Mtime(i)
            | Label::ItemId(i) => *i,
            Label::Other(_, LabelValue::Integer(i)) => *i,
            Label::Other(_, LabelValue::Double(bits)) => {
                f64::from_bits(*bits) as i64
            }
            Label::Other(_, LabelValue::Null) => 0,
            _ => self.as_str().parse::<i64>().unwrap_or_default(),
        }
    }

    /// この Label が代表するタグの型（TagType）を返します。
    pub fn tag_type(&self) -> TagType {
        match self {
            Label::Name(_) => TagType::Base(SType::Name),
            Label::Rank(_) => TagType::Base(SType::Rank),
            Label::Size(_) => TagType::Base(SType::Size),
            Label::Mtime(_) => TagType::Base(SType::Mtime),
            Label::Hash(_) => TagType::Base(SType::Hash),
            Label::ItemKind(_) => TagType::Base(SType::ItemKind),
            Label::Extension(_) => TagType::Base(SType::Extension),
            Label::Path(_) => TagType::Base(SType::Path),
            Label::ItemId(_) => TagType::Base(SType::ItemId),
            Label::FileId(_) => TagType::Base(SType::FileId),
            Label::IsDir(_) => TagType::Base(SType::IsDir),
            Label::Other(tt, _) => tt.clone(),
        }
    }

    /// データベースの行から指定されたオフセットのカラム（LabelStr, LabelInt 等）を Label として物理デコードし、
    /// その後ドメインへの格上げ（Promotion）を試みます。
    pub fn from_raw_row(tag: TagType, r: &duckdb::Row, offset: usize) -> Self {
        let label_val = if let Ok(i) = r.get::<_, i64>(offset + 1) {
            LabelValue::Integer(i)
        } else if let Ok(s) = r.get::<_, String>(offset) {
            LabelValue::String(s)
        } else if let Ok(b) = r.get::<_, bool>(offset + 3) {
            LabelValue::Boolean(b)
        } else if let Ok(d) = r.get::<_, f64>(offset + 2) {
            LabelValue::Double(d.to_bits())
        } else {
            LabelValue::String(String::new())
        };
        Self::resolve(tag, label_val)
    }

    /// Label が保持している物理的な値（LabelValue）を返します。
    pub fn value(&self) -> LabelValue {
        match self {
            Label::Name(s)
            | Label::Hash(s)
            | Label::ItemKind(s)
            | Label::Extension(s)
            | Label::Path(s) => LabelValue::String(s.clone()),
            Label::Rank(i)
            | Label::Size(i)
            | Label::Mtime(i)
            | Label::ItemId(i) => LabelValue::Integer(*i),
            Label::FileId(u) => LabelValue::String(u.to_string()),
            Label::IsDir(b) => LabelValue::Boolean(*b),
            Label::Other(_, val) => val.clone(),
        }
    }

    /// 物理的な型とタグの種類から、適切なドメイン指向 Label を構築（Promote）します。
    pub fn resolve(tag: TagType, value: LabelValue) -> Self {
        let TagType::Base(stype) = &tag else {
            return Label::Other(tag, value);
        };

        match (stype, &value) {
            (SType::Name, LabelValue::String(s)) => Label::Name(s.clone()),
            (SType::Rank, LabelValue::Integer(i)) => Label::Rank(*i),
            (SType::Size, LabelValue::Integer(i)) => Label::Size(*i),
            (SType::Mtime, LabelValue::Integer(i)) => Label::Mtime(*i),
            (SType::Hash, LabelValue::String(s)) => Label::Hash(s.clone()),
            (SType::ItemKind, LabelValue::String(s)) => {
                Label::ItemKind(s.clone())
            }
            (SType::Extension, LabelValue::String(s)) => {
                Label::Extension(s.clone())
            }
            (SType::Path, LabelValue::String(s)) => Label::Path(s.clone()),
            (SType::ItemId, LabelValue::Integer(i)) => Label::ItemId(*i),
            (SType::IsDir, LabelValue::Boolean(b)) => Label::IsDir(*b),
            _ => Label::Other(tag, value),
        }
    }
}

impl From<String> for Label {
    fn from(s: String) -> Self {
        Label::Other(TagType::Custom(String::new()), LabelValue::String(s))
    }
}

impl From<&str> for Label {
    fn from(s: &str) -> Self {
        Label::Other(
            TagType::Custom(String::new()),
            LabelValue::String(s.to_string()),
        )
    }
}

impl From<bool> for Label {
    fn from(b: bool) -> Self {
        Label::Other(TagType::Custom(String::new()), LabelValue::Boolean(b))
    }
}

impl From<i64> for Label {
    fn from(i: i64) -> Self {
        Label::Other(TagType::Custom(String::new()), LabelValue::Integer(i))
    }
}

/// 「キー:値」のペアを表す構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct TypedTag {
    /// タグの値（型情報を内包）
    pub label: Label,
}

impl std::fmt::Display for TypedTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}",
            self.label.tag_type().as_str(),
            self.label.as_str()
        )
    }
}

impl TypedTag {
    /// 新しい `TypedTag` を作成します。
    pub fn new(
        tagtype: impl Into<TagType>,
        label_val: impl Into<LabelValue>,
    ) -> Self {
        Self {
            label: Label::resolve(tagtype.into(), label_val.into()),
        }
    }
}

impl From<String> for LabelValue {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}
impl From<&str> for LabelValue {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}
impl From<i64> for LabelValue {
    fn from(i: i64) -> Self {
        Self::Integer(i)
    }
}
impl From<bool> for LabelValue {
    fn from(b: bool) -> Self {
        Self::Boolean(b)
    }
}

impl From<Label> for LabelValue {
    fn from(l: Label) -> Self {
        l.value()
    }
}

impl From<&Label> for LabelValue {
    fn from(l: &Label) -> Self {
        l.value()
    }
}

impl duckdb::ToSql for LabelValue {
    fn to_sql(&self) -> duckdb::Result<duckdb::types::ToSqlOutput<'_>> {
        use duckdb::types::Value;
        let val = match self {
            LabelValue::Integer(i) => Value::BigInt(*i),
            LabelValue::Boolean(b) => Value::Boolean(*b),
            LabelValue::Double(bits) => Value::Double(f64::from_bits(*bits)),
            LabelValue::Null => Value::Null,
            LabelValue::String(s) | LabelValue::Literal(s) => {
                Value::Text(s.clone())
            }
        };
        Ok(duckdb::types::ToSqlOutput::Owned(val))
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
#[derive(Debug, PartialEq, Clone, Default)]
pub struct Tags {
    pub entries: Vec<TagEntry>,
}

#[derive(Debug, PartialEq, Clone)]
pub struct TagEntry {
    pub label: Label,
    pub origin: Origin,
}

impl Tags {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 新しいタグを追加します。
    pub fn push(&mut self, label: Label, origin: Origin) {
        self.entries.push(TagEntry { label, origin });
    }

    /// 「型:値」のペアを生成するイテレータを返します。
    pub fn iter_typed_tags(&self) -> impl Iterator<Item = TypedTag> + '_ {
        self.entries.iter().map(|e| TypedTag {
            label: e.label.clone(),
        })
    }

    /// 指定された型のタグ値をリストとして取得します（リニアスキャン）。
    pub fn get_values(&self, key: &TagType) -> Vec<TagValue> {
        self.entries
            .iter()
            .filter(|e| e.label.tag_type() == *key)
            .map(|e| TagValue {
                label: e.label.clone(),
                origin: e.origin,
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// 既存コードとの互換性のためのイテレーション対応（所有権を消費）
impl IntoIterator for Tags {
    type Item = (TagType, Vec<TagValue>);
    type IntoIter =
        std::collections::hash_map::IntoIter<TagType, Vec<TagValue>>;

    fn into_iter(self) -> Self::IntoIter {
        // 必要に応じて HashMap に詰め直して返す（Lazy 化の恩恵は受けられないが、互換性は保つ）
        let mut map: std::collections::HashMap<TagType, Vec<TagValue>> =
            std::collections::HashMap::new();
        for entry in self.entries {
            map.entry(entry.label.tag_type())
                .or_default()
                .push(TagValue {
                    label: entry.label,
                    origin: entry.origin,
                });
        }
        map.into_iter()
    }
}

// 共有参照によるイテレーション（HashMap への詰め直しを避けるため、(TagType, Vec<TagValue>) 形式は限定的に）
impl<'a> IntoIterator for &'a Tags {
    type Item = (TagType, Vec<TagValue>);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        let mut map: std::collections::HashMap<TagType, Vec<TagValue>> =
            std::collections::HashMap::new();
        for entry in &self.entries {
            map.entry(entry.label.tag_type())
                .or_default()
                .push(TagValue {
                    label: entry.label.clone(),
                    origin: entry.origin,
                });
        }
        map.into_iter().collect::<Vec<_>>().into_iter()
    }
}

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

/// キャッシュ生成等の進捗状況を表す構造体。
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub struct Progress {
    /// 現在の完了件数
    pub current: usize,
    /// 全体の予定件数（不明な場合は None）
    pub total: Option<usize>,
    /// 完了フラグ（明示的な完了状態）
    pub is_done: bool,
}

impl Progress {
    /// 完了率（0.0 〜 1.0）を取得します。
    pub fn ratio(&self) -> f32 {
        match self.total {
            Some(t) if t > 0 => self.current as f32 / t as f32,
            _ => 0.0,
        }
    }

    /// 全て完了しているかどうかを判定します。
    pub fn is_finished(&self) -> bool {
        self.is_done
    }
}

/// ライフタイムに制約のないタグ名（参照）。
pub type Name<'a> = &'a str;

/// プログラム終了まで有効なタグ名（静的文字列）。
pub type StaticName = &'static str;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    Value,
    // Type-specific tags
    Integer,
    Boolean,
    Double,
    Null,
}

impl SType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ItemId => "item_id",
            Self::FileId => "file_id",
            Self::Path => "path",
            Self::Parentdir => "parentdir",
            Self::Filename => "filename",
            Self::Stem => "stem",
            Self::Extension => "extension",
            Self::IsDir => "is_dir",
            Self::Size => "size",
            Self::Mtime => "mtime",
            Self::TypeFromExt => "type_from_ext",
            Self::SizeStr => "size_str",
            Self::ModifiedStr => "modified_str",
            Self::Hash => "hash",
            Self::Type => "type",
            Self::Label => "label",
            Self::ItemKind => "item_kind",
            Self::Content => "content",
            Self::Rank => "rank",
            Self::Origin => "origin",
            Self::Name => "name",
            Self::Types => "types",
            Self::Labels => "labels",
            Self::ScanHash => "scan_hash",
            Self::TypedTag => "tag",
            Self::Directory => "directory",
            Self::LabelStr => "label_str",
            Self::LabelInt => "label_int",
            Self::LabelDouble => "label_double",
            Self::LabelBool => "label_bool",
            Self::DataType => "data_type",
            Self::Value => "value",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Double => "double",
            Self::Null => "null",
        }
    }

    pub fn name(&self) -> String {
        self.as_str().to_string()
    }
}

impl std::fmt::Display for SType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for SType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "item_id" => Ok(Self::ItemId),
            "file_id" => Ok(Self::FileId),
            "path" => Ok(Self::Path),
            "parentdir" => Ok(Self::Parentdir),
            "filename" => Ok(Self::Filename),
            "stem" => Ok(Self::Stem),
            "extension" => Ok(Self::Extension),
            "is_dir" => Ok(Self::IsDir),
            "size" => Ok(Self::Size),
            "mtime" => Ok(Self::Mtime),
            "type_from_ext" => Ok(Self::TypeFromExt),
            "size_str" => Ok(Self::SizeStr),
            "modified_str" => Ok(Self::ModifiedStr),
            "hash" => Ok(Self::Hash),
            "type" => Ok(Self::Type),
            "label" => Ok(Self::Label),
            "item_kind" => Ok(Self::ItemKind),
            "content" => Ok(Self::Content),
            "rank" => Ok(Self::Rank),
            "origin" => Ok(Self::Origin),
            "name" => Ok(Self::Name),
            "types" => Ok(Self::Types),
            "labels" => Ok(Self::Labels),
            "scan_hash" => Ok(Self::ScanHash),
            "tag" => Ok(Self::TypedTag),
            "directory" => Ok(Self::Directory),
            "label_str" => Ok(Self::LabelStr),
            "label_int" => Ok(Self::LabelInt),
            "label_double" => Ok(Self::LabelDouble),
            "label_bool" => Ok(Self::LabelBool),
            "data_type" => Ok(Self::DataType),
            "value" => Ok(Self::Value),
            "integer" => Ok(Self::Integer),
            "boolean" => Ok(Self::Boolean),
            "double" => Ok(Self::Double),
            "null" => Ok(Self::Null),
            _ => Err(format!("Unknown SType: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests_types {
    use super::*;

    #[test]
    fn test_typed_tag_display() {
        let tt = TypedTag::new("extension", "rs");
        assert_eq!(tt.to_string(), "extension:rs");

        let tt_int = TypedTag::new("size", 1024i64);
        assert_eq!(tt_int.to_string(), "size:1024");
    }

    #[test]
    fn test_tags_iter_typed_tags() {
        let mut tags = Tags::new();
        tags.push(
            Label::resolve(TagType::from("project"), "A".into()),
            Origin::User,
        );
        tags.push(
            Label::resolve(TagType::from("project"), "B".into()),
            Origin::User,
        );
        tags.push(
            Label::resolve(TagType::from("extension"), "rs".into()),
            Origin::User,
        );

        let mut results: Vec<String> =
            tags.iter_typed_tags().map(|tt| tt.to_string()).collect();
        results.sort();

        assert_eq!(results.len(), 3);
        assert!(results.contains(&"project:A".to_string()));
        assert!(results.contains(&"project:B".to_string()));
        assert!(results.contains(&"extension:rs".to_string()));
    }

    #[test]
    fn test_item_id_volatile_serial() {
        // Volatile ID は連番になるはず (0から開始)
        let id1 = ItemId::new_volatile();
        let id2 = ItemId::new_volatile();

        if let (ItemId::Volatile(v1), ItemId::Volatile(v2)) = (id1, id2) {
            assert_eq!(v2, v1 + 1);
        } else {
            panic!("Should be Volatile");
        }
    }

    #[test]
    fn test_item_id_as_i64() {
        let sid = ItemId::Stored(123);
        assert_eq!(sid.as_i64(), 123);

        let vid = ItemId::Volatile(456);
        assert_eq!(vid.as_i64(), 456);
    }

    #[test]
    fn test_item_id_display() {
        let sid = ItemId::Stored(123);
        assert_eq!(sid.to_string(), "123");

        let vid = ItemId::Volatile(456);
        assert_eq!(vid.to_string(), "456");
    }

    #[test]
    fn test_item_id_from_i64() {
        let id1 = ItemId::from(10);
        assert_eq!(id1, ItemId::Stored(10));

        let id2 = ItemId::from(-10);
        assert_eq!(id2, ItemId::Stored(-10));
    }

    #[test]
    fn test_item_kind_properties() {
        let k = ItemKind::File;
        assert!(k.is_stored());
        assert!(!k.is_volatile());
        assert_eq!(k.to_string(), "file");

        let kv = ItemKind::Volatile;
        assert!(!kv.is_stored());
        assert!(kv.is_volatile());
        assert_eq!(kv.to_string(), "volatile");
    }
}

impl From<SType> for &'static str {
    fn from(stype: SType) -> Self {
        stype.as_str()
    }
}
