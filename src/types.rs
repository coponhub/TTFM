use duckdb::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use uuid::Uuid;

/// メタデータ取得に失敗した際のデフォルト値。
pub const METADATA_ERROR: i64 = -1;

/// アイテムの優先度を表す型。
pub type Rank = i64;

/// アイテムの一意なID。
/// 実際のDBアイテム (Stored) または揮発性アイテム (Volatile) を表現。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum ItemId {
    /// データベースに存在する実アイテム（永続化済み）
    Stored(i64),
    /// 集約結果など、DBに存在しない揮発性アイテム
    Volatile(VolatileItem),
}

/// 揮発性アイテムの種類
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum VolatileItem {
    /// 真偽値 (1=True, 0=False)
    Boolean(u8),
    /// スカラー数値 (f64 bits)
    Scalar(u64),
    /// NULL（判定不能）
    Null,
    /// ラベル値（投影時の転置表現）
    Label(String),
}

impl VolatileItem {
    pub const KIND: &'static str = "volatile";
    pub const LABEL_KIND: &'static str = "label";
}

impl ItemId {
    /// i64 値を取得（Volatile の場合は負の値: True=-1, False=-2）
    pub fn as_i64(&self) -> i64 {
        match self {
            ItemId::Stored(i) => *i,
            ItemId::Volatile(VolatileItem::Boolean(v)) => *v as i64,
            ItemId::Volatile(VolatileItem::Scalar(_)) => 0,
            ItemId::Volatile(VolatileItem::Null) => -1,
            ItemId::Volatile(VolatileItem::Label(_)) => -100,
        }
    }

    /// 新しい揮発性スカラーアイテムを作成する
    pub fn new_volatile_scalar(f: f64) -> Self {
        ItemId::Volatile(VolatileItem::Scalar(f.to_bits()))
    }

    /// Stored かどうか
    pub fn is_stored(&self) -> bool {
        matches!(self, ItemId::Stored(_))
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
            ItemId::Volatile(VolatileItem::Boolean(1)) => write!(f, "1"),
            ItemId::Volatile(VolatileItem::Boolean(0)) => write!(f, "0"),
            ItemId::Volatile(VolatileItem::Boolean(v)) => write!(f, "{}", v),
            ItemId::Volatile(VolatileItem::Scalar(bits)) => {
                write!(f, "{}", f64::from_bits(*bits))
            }
            ItemId::Volatile(VolatileItem::Null) => write!(f, "-1"),
            ItemId::Volatile(VolatileItem::Label(s)) => write!(f, "{}", s),
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
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum LabelValue {
    String(String),
    Integer(i64),
    Boolean(bool),
    /// 引用符で囲まれたリテラル文字列。
    Literal(String),
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
            Label::IsDir(b) => b.to_string(),
            Label::Other(_, val) => match val {
                LabelValue::String(s) | LabelValue::Literal(s) => s.clone(),
                LabelValue::Integer(i) => i.to_string(),
                LabelValue::Boolean(b) => b.to_string(),
            },
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
            LabelValue::String(d.to_string())
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

/// システムで使用される標準的なタグ名のシンボル定義。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
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
    #[strum(serialize = "tag")]
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

impl SType {
    pub fn name(&self) -> String {
        self.to_string()
    }
}

#[cfg(test)]
mod tests_types {
    use super::*;

    #[test]
    fn test_typed_tag_display() {
        let tt = TypedTag::new("extension", "rs");
        assert_eq!(tt.to_string(), "extension:rs");

        let tt_int = TypedTag::new("size", 1024);
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
    fn test_volatile_item_null_as_i64() {
        // VolatileItem::Null は -1 を返すべき
        let id = ItemId::Volatile(VolatileItem::Null);
        assert_eq!(id.as_i64(), -1);
    }

    #[test]
    fn test_volatile_item_null_display() {
        // VolatileItem::Null は "-1" と表示されるべき
        let id = ItemId::Volatile(VolatileItem::Null);
        assert_eq!(id.to_string(), "-1");
    }

    #[test]
    fn test_volatile_item_null_is_not_stored() {
        let id = ItemId::Volatile(VolatileItem::Null);
        assert!(!id.is_stored());
    }

    #[test]
    fn test_volatile_item_label_as_i64() {
        // VolatileItem::Label は -100 を返すべき
        let id = ItemId::Volatile(VolatileItem::Label("rs".to_string()));
        assert_eq!(id.as_i64(), -100);
    }

    #[test]
    fn test_volatile_item_label_display() {
        // VolatileItem::Label は名前そのものを表示すべき
        let id = ItemId::Volatile(VolatileItem::Label("extension".to_string()));
        assert_eq!(id.to_string(), "extension");
    }

    #[test]
    fn test_volatile_item_label_is_not_stored() {
        // VolatileItem::Label は Stored ではない
        let id = ItemId::Volatile(VolatileItem::Label("myapp".to_string()));
        assert!(!id.is_stored());
    }

    #[test]
    fn test_volatile_item_label_clone() {
        // VolatileItem::Label は Clone 可能
        let id1 = ItemId::Volatile(VolatileItem::Label("test".to_string()));
        let id2 = id1.clone();
        assert_eq!(id1, id2);
        assert_eq!(id1.as_i64(), id2.as_i64());
    }
}
