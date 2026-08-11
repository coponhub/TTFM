// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

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
    Stored(i64),
    Volatile(u64),
    Settling(Origin, u64),
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
            Self::Settling(_, id) => *id as i64,
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

    /// Settling かどうか
    pub fn is_settling(&self) -> bool {
        matches!(self, ItemId::Settling(_, _))
    }

    /// Volatile を、区画（Origin）だけ確定した Settling に変換します。
    /// counter は使い回し、新規採番はしません。
    pub fn settle(self, origin: Origin) -> Self {
        match self {
            Self::Volatile(c) => Self::Settling(origin, c),
            other => other,
        }
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
            ItemId::Stored(i) => {
                let o = Origin::within(*i);
                write!(f, "{}({})", o.short(), i - o.block_lo())
            }
            ItemId::Volatile(v) => write!(f, "~({})", v),
            ItemId::Settling(o, v) => write!(f, "~{}({})", o.short(), v),
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
        Label::other(Bitical::String(kind.to_string()))
    }
}

/// 検索結果の並び順のキー1つ分。任意の型（TagType）をキーにでき、
/// 複数キーの組み合わせは `Vec<Order>` で表す（空 = 指定なし）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    /// ソートに使う型（rank / item_id / name / size / ユーザー定義型 など）
    pub key: TagType,
    /// 降順かどうか
    pub desc: bool,
}

impl Order {
    pub fn asc(key: impl Into<TagType>) -> Self {
        Self {
            key: key.into(),
            desc: false,
        }
    }

    pub fn desc(key: impl Into<TagType>) -> Self {
        Self {
            key: key.into(),
            desc: true,
        }
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
    PartialOrd,
    Ord,
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
    Builtin,
    User,
    File,
    Plugin,
}

impl Origin {
    /// 区画幅 B = 2^58（i64 空間を 64 分割した 1 区画のサイズ）。
    pub const BLOCK_SIZE: i64 = 1 << 58;

    /// origin の区画 index。負値は負側区画。Origin 追加時はここだけ変える。
    pub fn block_index(self) -> i64 {
        match self {
            Origin::Builtin => -1,
            Origin::User => 0,
            Origin::File => 8,
            Origin::Plugin => 16,
        }
    }

    /// 区画下端 lo = index * BLOCK_SIZE。
    pub fn block_lo(self) -> i64 {
        self.block_index() * Self::BLOCK_SIZE
    }

    /// 区画上端 hi（排他）。直上 origin の lo、最上位は i64::MAX。
    pub fn block_hi(self) -> i64 {
        use strum::IntoEnumIterator;
        let lo = self.block_lo();
        Origin::iter()
            .map(|o| o.block_lo())
            .filter(|&l| l > lo)
            .min()
            .unwrap_or(i64::MAX)
    }

    /// 区画内オフセットから item_id を求める。区画外に出るオフセットは `None`。
    pub fn id_at_offset(self, offset: i64) -> Option<i64> {
        let id = self.block_lo().checked_add(offset)?;
        (self.block_lo()..self.block_hi()).contains(&id).then_some(id)
    }

    /// origin の短縮ラベル。Builtin→"Sys"、User→"User"、File→"File"。
    pub fn short(self) -> &'static str {
        match self {
            Origin::Builtin => "Sys",
            Origin::User => "User",
            Origin::File => "File",
            Origin::Plugin => "Plg",
        }
    }

    /// origin の snake_case 文字列（"builtin" / "user" / "file" / "plugin"）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 大分類 (User/System) への収束。Builtin/File/Plugin は全て LargeOrigin::System。
    pub fn large(self) -> LargeOrigin {
        match self {
            Origin::User => LargeOrigin::User,
            Origin::Builtin | Origin::File | Origin::Plugin => {
                LargeOrigin::System
            }
        }
    }

    /// `large() == LargeOrigin::User` の糖衣。
    pub fn is_user(self) -> bool {
        self.large() == LargeOrigin::User
    }

    /// `large() == LargeOrigin::System` の糖衣。
    pub fn is_system(self) -> bool {
        self.large() == LargeOrigin::System
    }

    /// id → Origin 逆引き（全域関数）。
    /// `lo <= id` を満たす区画のうち lo が最大のものを返す。
    /// 全区画より下（負値等）の場合は lo 最小の Origin に縮退。
    pub fn within(id: i64) -> Self {
        use strum::IntoEnumIterator;
        Origin::iter()
            .filter(|&o| o.block_lo() <= id)
            .max_by_key(|&o| o.block_lo())
            .unwrap_or_else(|| {
                Origin::iter()
                    .min_by_key(|&o| o.block_lo())
                    .expect("Origin must have at least one variant")
            })
    }
}

/// Origin の大分類 (ITEM.md §由来)。System 側は Builtin/File/Plugin を区別せず収束する。
#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Clone,
    Copy,
    Display,
    EnumString,
    IntoStaticStr,
    EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum LargeOrigin {
    System,
    User,
}

impl LargeOrigin {
    /// 大分類の snake_case 文字列（"system" / "user"）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

pub trait BiticalAssociate {
    const BITICAL: BiticalType;
}

impl BiticalAssociate for String {
    const BITICAL: BiticalType = BiticalType::String;
}
impl BiticalAssociate for i64 {
    const BITICAL: BiticalType = BiticalType::Integer;
}
impl BiticalAssociate for Uuid {
    const BITICAL: BiticalType = BiticalType::Uuid;
}
impl BiticalAssociate for bool {
    const BITICAL: BiticalType = BiticalType::Boolean;
}

/// ファイルサイズ（バイト単位）を表す型。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub struct FileSize(pub i64);
impl BiticalAssociate for FileSize {
    const BITICAL: BiticalType = BiticalType::Integer;
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
impl BiticalAssociate for FileTimestamp {
    const BITICAL: BiticalType = BiticalType::Integer;
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

// BiticalはInt, Float, String, Boolean等の、
// ハードウェア・プロセッサレベルのプリミティブな型を指す。
// アイテムの持つ値はこれらに収束する。
#[derive(Debug, PartialEq, Eq, Clone, Copy, strum::Display, strum::EnumIter)]
#[strum(serialize_all = "snake_case")]
#[repr(i32)]
pub enum BiticalType {
    String = 1,
    Integer = 2,
    Double = 3,
    Boolean = 4,
    Uuid = 5,
}
#[derive(Debug, Clone)]
pub enum Bitical {
    String(String),
    Integer(i64),
    Double(f64),
    Boolean(bool),
    Uuid(Uuid),
}

pub type Biticals = Vec<Option<Bitical>>;

// 等価性は「同じ値が保存されるか」= ビット同一性で定義する。
// IEEE 比較と異なり NaN == NaN、0.0 != -0.0 となるため、Eq を合法に実装できる。
impl PartialEq for Bitical {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Bitical::String(a), Bitical::String(b)) => a == b,
            (Bitical::Integer(a), Bitical::Integer(b)) => a == b,
            (Bitical::Double(a), Bitical::Double(b)) => {
                a.to_bits() == b.to_bits()
            }
            (Bitical::Boolean(a), Bitical::Boolean(b)) => a == b,
            (Bitical::Uuid(a), Bitical::Uuid(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for Bitical {}

// 変種間の順序は語彙上の登場順（String/Integer/Double/Boolean/Uuid）で固定する。
// Double は `f64::total_cmp` を用いる。NaN のビットパターン差・+0.0/-0.0 の区別を
// 全順序として扱う点が `to_bits()` 比較の PartialEq と整合する。
impl PartialOrd for Bitical {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bitical {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn variant_rank(b: &Bitical) -> u8 {
            match b {
                Bitical::String(_) => 0,
                Bitical::Integer(_) => 1,
                Bitical::Double(_) => 2,
                Bitical::Boolean(_) => 3,
                Bitical::Uuid(_) => 4,
            }
        }
        match (self, other) {
            (Bitical::String(a), Bitical::String(b)) => a.cmp(b),
            (Bitical::Integer(a), Bitical::Integer(b)) => a.cmp(b),
            (Bitical::Double(a), Bitical::Double(b)) => a.total_cmp(b),
            (Bitical::Boolean(a), Bitical::Boolean(b)) => a.cmp(b),
            (Bitical::Uuid(a), Bitical::Uuid(b)) => a.cmp(b),
            _ => variant_rank(self).cmp(&variant_rank(other)),
        }
    }
}

impl BiticalType {
    /// 保存先の EAV カラム（label_str/label_int/label_double/label_bool）を返す。
    pub fn to_column(&self) -> SType {
        match self {
            BiticalType::String | BiticalType::Uuid => SType::LabelStr,
            BiticalType::Integer => SType::LabelInt,
            BiticalType::Double => SType::LabelDouble,
            BiticalType::Boolean => SType::LabelBool,
        }
    }

    /// EAV の型付きラベルカラム（label_str/label_int/label_double/label_bool）を
    /// 重複なく列挙する。`Uuid` は `String` と同じ `LabelStr` に収束するため含まない。
    /// 並びは宣言順 = 実際の保存列順（base_tags.parquet 等の DDL・appender が
    /// この順に依存する）。
    pub fn to_columns() -> [SType; 4] {
        [
            BiticalType::String,
            BiticalType::Integer,
            BiticalType::Double,
            BiticalType::Boolean,
        ]
        .map(|t| t.to_column())
    }

    /// ラベルカラムの非 NULL 走査順。label_str は oneview で全型の VARCHAR
    /// フォールバックを兼ねる（apply_label_columns が常に設定する）ため、
    /// 型付きカラムが先に評価されるよう末尾に回す。
    pub fn to_columns_scan_order() -> [SType; 4] {
        let mut cols = Self::to_columns();
        cols.sort_by_key(|c| *c == BiticalType::String.to_column());
        cols
    }
}

impl Bitical {
    pub fn name(&self) -> BiticalType {
        match self {
            Bitical::String(_) => BiticalType::String,
            Bitical::Integer(_) => BiticalType::Integer,
            Bitical::Double(_) => BiticalType::Double,
            Bitical::Boolean(_) => BiticalType::Boolean,
            Bitical::Uuid(_) => BiticalType::Uuid,
        }
    }

    pub fn as_display_name(&self) -> String {
        match self {
            Bitical::String(s) => s.clone(),
            Bitical::Integer(i) => i.to_string(),
            Bitical::Boolean(true) => "TRUE".to_string(),
            Bitical::Boolean(false) => "FALSE".to_string(),
            Bitical::Double(d) => d.to_string(),
            Bitical::Uuid(u) => u.to_string(),
        }
    }
}

/// 書込境界。読込側の対応は `Bitical::from_db_value`（db/mod.rs）。
/// Uuid は Value にネイティブ variant が無いため Text に収束する（非対称）。
impl duckdb::ToSql for Bitical {
    fn to_sql(&self) -> duckdb::Result<duckdb::types::ToSqlOutput<'_>> {
        use duckdb::types::Value;
        let val = match self {
            Bitical::String(s) => Value::Text(s.clone()),
            Bitical::Integer(i) => Value::BigInt(*i),
            Bitical::Double(d) => Value::Double(*d),
            Bitical::Boolean(b) => Value::Boolean(*b),
            Bitical::Uuid(u) => Value::from(*u),
        };
        Ok(duckdb::types::ToSqlOutput::Owned(val))
    }
}

impl From<String> for Bitical {
    fn from(v: String) -> Self {
        Bitical::String(v)
    }
}
impl From<i64> for Bitical {
    fn from(v: i64) -> Self {
        Bitical::Integer(v)
    }
}
impl From<f64> for Bitical {
    fn from(v: f64) -> Self {
        Bitical::Double(v)
    }
}
impl From<bool> for Bitical {
    fn from(v: bool) -> Self {
        Bitical::Boolean(v)
    }
}
impl From<Uuid> for Bitical {
    fn from(v: Uuid) -> Self {
        Bitical::Uuid(v)
    }
}
impl From<FileSize> for Bitical {
    fn from(v: FileSize) -> Self {
        Bitical::Integer(v.0)
    }
}
impl From<FileTimestamp> for Bitical {
    fn from(v: FileTimestamp) -> Self {
        Bitical::Integer(v.0)
    }
}

impl TryFrom<Bitical> for String {
    type Error = Bitical;
    fn try_from(v: Bitical) -> Result<Self, Self::Error> {
        match v {
            Bitical::String(s) => Ok(s),
            other => Err(other),
        }
    }
}
impl TryFrom<Bitical> for i64 {
    type Error = Bitical;
    fn try_from(v: Bitical) -> Result<Self, Self::Error> {
        match v {
            Bitical::Integer(i) => Ok(i),
            other => Err(other),
        }
    }
}
impl TryFrom<Bitical> for f64 {
    type Error = Bitical;
    fn try_from(v: Bitical) -> Result<Self, Self::Error> {
        match v {
            Bitical::Double(d) => Ok(d),
            other => Err(other),
        }
    }
}
impl TryFrom<Bitical> for bool {
    type Error = Bitical;
    fn try_from(v: Bitical) -> Result<Self, Self::Error> {
        match v {
            Bitical::Boolean(b) => Ok(b),
            other => Err(other),
        }
    }
}
impl TryFrom<Bitical> for Uuid {
    type Error = Bitical;
    fn try_from(v: Bitical) -> Result<Self, Self::Error> {
        match v {
            Bitical::Uuid(u) => Ok(u),
            other => Err(other),
        }
    }
}
impl TryFrom<Bitical> for FileSize {
    type Error = Bitical;
    fn try_from(v: Bitical) -> Result<Self, Self::Error> {
        match v {
            Bitical::Integer(i) => Ok(FileSize(i)),
            other => Err(other),
        }
    }
}
impl TryFrom<Bitical> for FileTimestamp {
    type Error = Bitical;
    fn try_from(v: Bitical) -> Result<Self, Self::Error> {
        match v {
            Bitical::Integer(i) => Ok(FileTimestamp(i)),
            other => Err(other),
        }
    }
}

/// タグの「キー（型）」部分を表す SuperType。
/// システム定義の標準タグ（SType）と、自由なカスタムタグの両方を扱えます。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum TagType {
    Base(SType),
    Custom(String),
    /// 引用符で囲まれたリテラル。自動展開を行わない。
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

    /// `Nest`（`&:`）の単位元となるワイルドカードキー（`*:`）かどうかを判定します。
    pub fn is_base_key(&self) -> bool {
        crate::util::is_full_match_glob(self.as_str())
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

#[derive(Debug, Clone, PartialEq)]
pub enum LabelNode {
    DefaultLabelNode,
    Formatted(crate::query::format::Formatted),
}

/// タグの「値」部分（例: "rs", "1024"）。
/// 型は持たない。値がどの型のものかは、それを包む `TypedTag` が決める。
#[derive(Debug, Clone)]
pub struct Label {
    pub(crate) value: Bitical,
    node: LabelNode,
}

impl PartialEq for Label {
    /// `node` は比較しない（決定 14）。
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for Label {}

impl PartialOrd for Label {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Label {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl Label {
    pub(crate) fn other(v: impl Into<Bitical>) -> Self {
        Self { value: v.into(), node: LabelNode::DefaultLabelNode }
    }

    pub fn node(&self) -> &LabelNode {
        &self.node
    }

    pub(crate) fn set_node(&mut self, node: LabelNode) {
        self.node = node;
    }
}

/// 日付リテラルの精度を保持する中間型。
/// `chrono::DateTime` との衝突は完全修飾パスで区別する。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub enum DateTime {
    /// 年のみ（mtime コンテキストで Integer 由来）
    Year(i32),
    /// 年月（例: "2026-02"）
    YearMonth { year: i32, month: u32 },
    /// 年月日（例: "2026-02-01"）
    Date(chrono::NaiveDate),
    /// 時分（秒指定なし。例: "12:30"）
    Minute(chrono::NaiveDateTime),
    /// 時点（秒まで指定。例: "12:30:05"）
    Instant(chrono::DateTime<chrono::Local>),
}

impl DateTime {
    /// 期間の下限（開始時点）を NaiveDateTime で返す。
    pub fn floor(&self) -> Option<chrono::NaiveDateTime> {
        use chrono::{NaiveDate, TimeZone};
        match self {
            DateTime::Year(y) => {
                NaiveDate::from_ymd_opt(*y, 1, 1)?.and_hms_opt(0, 0, 0)
            }
            DateTime::YearMonth { year, month } => {
                NaiveDate::from_ymd_opt(*year, *month, 1)?.and_hms_opt(0, 0, 0)
            }
            DateTime::Date(d) => d.and_hms_opt(0, 0, 0),
            DateTime::Minute(ndt) => {
                use chrono::Timelike;
                ndt.with_second(0)
            }
            DateTime::Instant(dt) => Some(
                chrono::Local
                    .timestamp_opt(dt.timestamp(), 0)
                    .single()?
                    .naive_local(),
            ),
        }
    }

    /// 期間の上限（終了時点）を NaiveDateTime で返す。
    pub fn ceiling(&self) -> Option<chrono::NaiveDateTime> {
        use chrono::{NaiveDate, TimeZone};
        match self {
            DateTime::Year(y) => {
                NaiveDate::from_ymd_opt(*y, 12, 31)?.and_hms_opt(23, 59, 59)
            }
            DateTime::YearMonth { year, month } => {
                let last_day = last_day_of_month(*year, *month)?;
                NaiveDate::from_ymd_opt(*year, *month, last_day)?
                    .and_hms_opt(23, 59, 59)
            }
            DateTime::Date(d) => d.and_hms_opt(23, 59, 59),
            DateTime::Minute(ndt) => {
                use chrono::Timelike;
                ndt.with_second(59)
            }
            DateTime::Instant(dt) => Some(
                chrono::Local
                    .timestamp_opt(dt.timestamp(), 0)
                    .single()?
                    .naive_local(),
            ),
        }
    }

    pub fn as_display_str(&self) -> String {
        match self {
            DateTime::Year(y) => y.to_string(),
            DateTime::YearMonth { year, month } => format!("{year}-{month:02}"),
            DateTime::Date(d) => d.format("%Y-%m-%d").to_string(),
            DateTime::Minute(ndt) => ndt.format("%Y-%m-%d %H:%M").to_string(),
            DateTime::Instant(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        }
    }

    pub fn local_utc_offset_secs() -> i64 {
        chrono::Local::now().offset().local_minus_utc() as i64
    }

    pub fn utc_epoch_to_local_epoch(utc_epoch_secs: i64) -> i64 {
        utc_epoch_secs + Self::local_utc_offset_secs()
    }

    /// 任意タイムゾーンの `chrono::DateTime` を `Instant` バリアントとして生成する。
    pub fn from_localtime<Tz: chrono::TimeZone>(
        dt: chrono::DateTime<Tz>,
    ) -> Self {
        DateTime::Instant(dt.with_timezone(&chrono::Local))
    }

    /// 期間の開始点を UNIX タイムスタンプ (i64) で返す。DB/SQL バインド用フォールバック。
    pub fn to_timestamp(&self) -> i64 {
        use chrono::{Local, TimeZone};
        self.floor()
            .and_then(|ndt| Local.from_local_datetime(&ndt).earliest())
            .map(|dt| dt.timestamp())
            .unwrap_or(0)
    }

    /// この DateTime が表す期間を `DateTimeRange::Interval`（floor..ceiling の
    /// 生タイムスタンプ）へ変換する。検索/SQL 側が扱う生表現への変換点。
    pub fn to_interval(&self) -> Option<DateTimeRange> {
        use chrono::{Local, TimeZone};
        let to_ts = |ndt: chrono::NaiveDateTime| -> Option<i64> {
            Local
                .from_local_datetime(&ndt)
                .earliest()
                .map(|dt| dt.timestamp())
        };
        let start = to_ts(self.floor()?)?;
        let end = to_ts(self.ceiling()?)?;
        Some(DateTimeRange::interval(start, end))
    }
}

impl DateTime {
    /// 構造化日付（YYYY-MM-DD / YYYY-MM / M/D今年 / YYYY 単体）のみを解釈する。
    /// 自然言語・相対日付は対象外。`DateTimeRange::parse` がここを先に試してから
    /// 自然言語（`FromStr::from_str`）へ落ちるよう、独立して呼べる形にしている。
    /// `None` = 構造化日付の形をしていない（呼び出し元は他の解釈を試してよい）、
    /// `Some(Err(()))` = 形はしているが値が不正（例: 存在しない日付）。
    pub(crate) fn parse_structured(s: &str) -> Option<Result<DateTime, ()>> {
        use chrono::{Datelike, Local, NaiveDate};

        let s = s.trim();
        let parts: Vec<&str> = s
            .split(|c| c == '/' || c == '-')
            .filter(|p| !p.is_empty())
            .collect();

        // YYYY-MM-DD / YYYY/MM/DD
        if parts.len() == 3
            && parts[0].len() == 4
            && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
        {
            let Ok(y) = parts[0].parse::<i32>() else {
                return Some(Err(()));
            };
            let Ok(m) = parts[1].parse::<u32>() else {
                return Some(Err(()));
            };
            let Ok(d) = parts[2].parse::<u32>() else {
                return Some(Err(()));
            };
            return Some(
                NaiveDate::from_ymd_opt(y, m, d).map(DateTime::Date).ok_or(()),
            );
        }

        // YYYY-MM / YYYY/MM（4桁年）、M/D（今年。月<=12 かつ非4桁の1個目）
        if parts.len() == 2
            && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
        {
            let Ok(p1) = parts[0].parse::<i32>() else {
                return Some(Err(()));
            };
            let Ok(p2) = parts[1].parse::<u32>() else {
                return Some(Err(()));
            };
            if parts[0].len() == 4 || p1 > 12 {
                return Some(
                    NaiveDate::from_ymd_opt(p1, p2, 1)
                        .map(|_| DateTime::YearMonth { year: p1, month: p2 })
                        .ok_or(()),
                );
            }
            return Some(
                NaiveDate::from_ymd_opt(Local::now().year(), p1 as u32, p2)
                    .map(DateTime::Date)
                    .ok_or(()),
            );
        }

        None
    }
}

impl std::str::FromStr for DateTime {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        use chrono::{Datelike, Local};

        let s = s.trim();
        if let Some(result) = Self::parse_structured(s) {
            return result;
        }
        // 区切りも時間語彙も持たない裸の数値は日付ではない。chrono_english は
        // 独自ヒューリスティクスでこれらを掴んでしまうので、渡す前に弾く。
        if s.parse::<f64>().is_ok() {
            return Err(());
        }

        // 自然言語・相対日付（"today", "7d ago", "12:30", "12:30:05" 等）。
        // コロンの個数で精度を分類する: 0個 = 日精度（丸1日）、1個 = 分精度、
        // 2個 = 瞬間（秒まで指定済み）。
        let colon_count = s.matches(':').count();
        let classify = |dt: chrono::DateTime<Local>| -> DateTime {
            use chrono::Timelike;
            match colon_count {
                0 => DateTime::Date(dt.date_naive()),
                1 => DateTime::Minute(
                    dt.naive_local()
                        .with_second(0)
                        .unwrap_or_else(|| dt.naive_local()),
                ),
                _ => DateTime::from_localtime(dt),
            }
        };
        let s_lower = s.to_lowercase();
        let now = Local::now();
        if let Ok(dt) = chrono_english::parse_date_string(
            &s_lower,
            now,
            chrono_english::Dialect::Uk,
        ) {
            return Ok(classify(dt));
        }

        // ago 指定の明示的試行（parse_date_string が失敗しても
        // parse_duration が成功するケースの救済）
        if s_lower.contains("ago") {
            if let Ok(interval) = chrono_english::parse_duration(&s_lower) {
                use chrono_english::Interval;
                let past = match interval {
                    Interval::Seconds(sec) => {
                        now + chrono::Duration::seconds(sec.into())
                    }
                    Interval::Days(d) => now + chrono::Duration::days(d as i64),
                    Interval::Months(m) => {
                        let mut y = now.year();
                        let mut mo = now.month() as i32 + m;
                        while mo <= 0 {
                            y -= 1;
                            mo += 12;
                        }
                        now.with_year(y)
                            .and_then(|d| d.with_month(mo as u32))
                            .unwrap_or(now)
                    }
                };
                return Ok(classify(past));
            }
        }

        if let Ok(dt) = dateparser::parse_with_timezone(s, &Local) {
            return Ok(classify(dt.with_timezone(&Local)));
        }

        Err(())
    }
}

/// スロット制約の対象フィールド。宣言順が有意性の高い順であり、
/// スロット列の並びの唯一の定義。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl DateField {
    pub const ALL: [DateField; 6] = [
        DateField::Year,
        DateField::Month,
        DateField::Day,
        DateField::Hour,
        DateField::Minute,
        DateField::Second,
    ];
    pub const COUNT: usize = Self::ALL.len();

    /// 日付部（`-` 区切り）のフィールド。有意性の高い順。
    pub const DATE_PART: [DateField; 3] =
        [DateField::Year, DateField::Month, DateField::Day];
    /// 時刻部（`:` 区切り）のフィールド。有意性の高い順。
    pub const TIME_PART: [DateField; 3] =
        [DateField::Hour, DateField::Minute, DateField::Second];

    /// SQL の `EXTRACT(<field> FROM ...)` に渡すフィールド名。
    pub fn extract_name(self) -> &'static str {
        match self {
            DateField::Year => "YEAR",
            DateField::Month => "MONTH",
            DateField::Day => "DAY",
            DateField::Hour => "HOUR",
            DateField::Minute => "MINUTE",
            DateField::Second => "SECOND",
        }
    }
}

/// 日付スロット1つぶんの制約（自由 | 単一値）。
/// フィールド内の文字単位の部分 glob は扱わないので、値は単一値のみ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateSlot {
    Free,
    Value(i64),
}

/// 日付の絞り込み条件。区間（暦スロットに揃わない時点。`7d ago` 等）、または
/// YMDHMS 各フィールドのスロット制約（周期的な条件。`*-02-01` 等）のいずれかを表す。
/// op（Eq/Gt/...）は持たない — 適用は SQL 生成側の責務（DateTime::to_range とは異なる）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateTimeRange {
    Interval {
        start: i64,
        end: i64,
    },
    /// `DateField::ALL` と同じ順のスロット列。
    Slots([DateSlot; DateField::COUNT]),
}

impl DateTimeRange {
    pub fn interval(start: i64, end: i64) -> Self {
        DateTimeRange::Interval { start, end }
    }

    /// 区間形の場合のみ (start, end) を返す。スロット制約形は None。
    pub fn as_interval(&self) -> Option<(i64, i64)> {
        match self {
            DateTimeRange::Interval { start, end } => Some((*start, *end)),
            DateTimeRange::Slots(..) => None,
        }
    }

    /// フィールド単位の glob（例: `*-02-01` / `2026-*` / `12:*` / `*-02-01T12:*`）を
    /// スロット制約形へ翻訳する。
    ///
    /// 各フィールドは `*`（自由）か数値リテラル（単一値）のどちらかで、フィールド内の
    /// 文字単位の部分 glob（`2026-0*` / `20*`）は受け付けない — 「2000年以降」のような
    /// 範囲は glob ではなく比較式で書く。末尾の欠けたフィールドは自由になる
    /// （`2026-*` は2026年全体）。日付部と時刻部は `T` で区切る。`T` が無く `:` を含む
    /// ものは時刻のみのパターン（`12:*` は各日の12時台）。
    ///
    /// glob を含まないパターンは通常の日付解釈に任せるため None を返す。
    /// フィールド単位の glob を `(DateField, 生の値文字列)` の列へ分割する。
    /// 書かれたフィールドのみを返す（欠けた末尾フィールドは含まない）。
    pub(crate) fn split_slot_fields(pattern: &str) -> Option<Vec<(DateField, &str)>> {
        if !pattern.contains('*') {
            return None;
        }
        if pattern == "*" {
            return None;
        }
        let (date_part, time_part) = match pattern.split_once('T') {
            Some((d, t)) => (Some(d), Some(t)),
            None if pattern.contains(':') => (None, Some(pattern)),
            None => (Some(pattern), None),
        };

        let mut pairs = Vec::new();
        for (part, fields, sep) in [
            (date_part, DateField::DATE_PART, '-'),
            (time_part, DateField::TIME_PART, ':'),
        ] {
            let Some(part) = part else { continue };
            let values: Vec<&str> = part.split(sep).collect();
            if values.len() > fields.len() {
                return None;
            }
            for (field, value) in fields.iter().zip(values) {
                pairs.push((*field, value));
            }
        }
        Some(pairs)
    }

    pub fn parse_slot_glob(pattern: &str) -> Option<Self> {
        let fields = Self::split_slot_fields(pattern)?;
        let mut slots = [DateSlot::Free; DateField::COUNT];
        for (field, value) in fields {
            slots[field as usize] = Self::parse_slot_field(value)?;
        }
        Some(DateTimeRange::Slots(slots))
    }

    fn parse_slot_field(field: &str) -> Option<DateSlot> {
        if field == "*" {
            return Some(DateSlot::Free);
        }
        if !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit()) {
            return field.parse::<i64>().ok().map(DateSlot::Value);
        }
        None
    }

    /// `parse_structured` / `parse_slot_glob` / 自然言語（`FromStr`）を1本に束ねた入口。
    pub fn parse(s: &str) -> Option<Result<DateTimeRange, String>> {
        let s = s.trim();
        if let Some(range) = Self::parse_slot_glob(s) {
            return Some(Ok(range));
        }
        if let Some(result) = DateTime::parse_structured(s) {
            return Some(result.map_err(|_| format!("invalid date: {s}")).and_then(|dt| {
                dt.to_interval()
                    .ok_or_else(|| format!("cannot resolve date range: {s}"))
            }));
        }
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match s.parse::<DateTime>() {
            Ok(dt) => dt.to_interval().map(Ok),
            Err(()) => None,
        }
    }

    pub fn slots_min_timestamp(slots: &[DateSlot; DateField::COUNT]) -> i64 {
        use chrono::{Datelike, Days, Duration, Months, NaiveDate};

        let field = |f: DateField, min: i64| match slots[f as usize] {
            DateSlot::Value(n) => n,
            DateSlot::Free => min,
        };
        let year = field(DateField::Year, NaiveDate::MIN.year() as i64);
        let months = (field(DateField::Month, 1).max(1) - 1).clamp(0, u32::MAX as i64) as u32;
        let days = (field(DateField::Day, 1).max(1) - 1).clamp(0, i64::MAX) as u64;
        let hours = field(DateField::Hour, 0);
        let minutes = field(DateField::Minute, 0);
        let seconds = field(DateField::Second, 0);

        i32::try_from(year)
            .ok()
            .and_then(|y| NaiveDate::from_ymd_opt(y, 1, 1))
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .and_then(|dt| dt.checked_add_months(Months::new(months)))
            .and_then(|dt| dt.checked_add_days(Days::new(days)))
            .and_then(|dt| dt.checked_add_signed(Duration::hours(hours)))
            .and_then(|dt| dt.checked_add_signed(Duration::minutes(minutes)))
            .and_then(|dt| dt.checked_add_signed(Duration::seconds(seconds)))
            .map(|dt| dt.and_utc().timestamp())
            .unwrap_or(i64::MAX)
    }
}

fn last_day_of_month(year: i32, month: u32) -> Option<u32> {
    use chrono::NaiveDate;
    // 翌月の1日から1日引く
    let next_month = if month == 12 { 1 } else { month + 1 };
    let next_year = if month == 12 { year + 1 } else { year };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    use chrono::Datelike;
    Some((first_of_next - chrono::Duration::days(1)).day())
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Label {
    /// 文字列としての表現を取得します。
    pub fn as_str(&self) -> String {
        self.value.as_display_name()
    }

    /// 数値としての値を取得します（数値でない場合は 0）。
    pub fn as_i64(&self) -> i64 {
        match &self.value {
            Bitical::Integer(i) => *i,
            Bitical::Double(d) => *d as i64,
            _ => self.as_str().parse::<i64>().unwrap_or_default(),
        }
    }

    /// データベースの行から指定されたオフセットのカラム（LabelStr, LabelInt 等）を
    /// Label として物理デコードします。
    pub fn from_raw_row(r: &duckdb::Row, offset: usize) -> Self {
        let value = if let Ok(i) = r.get::<_, i64>(offset + 1) {
            Bitical::Integer(i)
        } else if let Ok(s) = r.get::<_, String>(offset) {
            Bitical::String(s)
        } else if let Ok(b) = r.get::<_, bool>(offset + 3) {
            Bitical::Boolean(b)
        } else if let Ok(d) = r.get::<_, f64>(offset + 2) {
            Bitical::Double(d)
        } else {
            Bitical::String(String::new())
        };
        Label::other(value)
    }

    pub fn value(&self) -> Bitical {
        self.value.clone()
    }
}

impl From<String> for Label {
    fn from(s: String) -> Self {
        Label::other(Bitical::String(s))
    }
}

impl From<&str> for Label {
    fn from(s: &str) -> Self {
        Label::other(Bitical::String(s.to_string()))
    }
}

impl From<bool> for Label {
    fn from(b: bool) -> Self {
        Label::other(Bitical::Boolean(b))
    }
}

impl From<i64> for Label {
    fn from(i: i64) -> Self {
        Label::other(Bitical::Integer(i))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedTagNode {
    DefaultTypedTag,
    Node(crate::query::Node),
}

/// 「キー:値」のペアを表す構造体。
#[derive(Debug, Clone)]
pub struct TypedTag {
    /// タグの値
    pub label: Label,
    tag_type: TagType,
    node: TypedTagNode,
}

impl PartialEq for TypedTag {
    /// `node` は比較しない（Node 込みの一致は名前付きメソッドで行う）。
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label && self.tag_type == other.tag_type
    }
}

impl std::fmt::Display for TypedTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.tag_type.as_str(), self.as_str())
    }
}

impl TypedTag {
    /// 新しい `TypedTag` を作成します。
    pub fn new(tagtype: impl Into<TagType>, value: impl Into<Bitical>) -> Self {
        Self {
            label: Label::other(value.into()),
            tag_type: tagtype.into(),
            node: TypedTagNode::DefaultTypedTag,
        }
    }

    /// 既存の `Label` を（解釈の段も含めて）そのまま持たせて TagType だけ変えます。
    /// `new` は `impl Into<Bitical>` 経由なので素の値しか渡せず、段が落ちます。
    pub fn retag(tagtype: impl Into<TagType>, label: &Label) -> Self {
        Self {
            label: label.clone(),
            tag_type: tagtype.into(),
            node: TypedTagNode::DefaultTypedTag,
        }
    }

    pub(crate) fn with_node(mut self, node: crate::query::Node) -> Self {
        self.node = TypedTagNode::Node(node);
        self
    }

    pub fn tag_type(&self) -> TagType {
        self.tag_type.clone()
    }

    pub fn value(&self) -> Bitical {
        self.label.value()
    }

    pub fn is_default_node(&self) -> bool {
        matches!(self.node, TypedTagNode::DefaultTypedTag)
    }

    /// 名札が指す既定形（`Type: := Label` の比較）。
    pub(crate) fn default_form(&self) -> crate::query::ast::QueryNode {
        crate::query::ast::QueryNode::Comparison(crate::query::ast::ComparisonNode {
            first: crate::query::ast::Operand::TypeRef(self.tag_type.clone()),
            rest: vec![(
                crate::query::ast::ComparisonOp::Label(crate::query::ast::BasicOp::Eq),
                crate::query::ast::Operand::Literal(self.label.clone()),
            )],
        })
    }

    pub fn node(&self) -> std::borrow::Cow<'_, crate::query::Node> {
        match &self.node {
            TypedTagNode::DefaultTypedTag => std::borrow::Cow::Owned(
                crate::query::Node::Query(Box::new(self.default_form())),
            ),
            TypedTagNode::Node(n) => std::borrow::Cow::Borrowed(n),
        }
    }

    /// 表示用の文字列。item_id のようにローカル形式を持つ型はここで解釈する
    /// （値そのものは `Label` が保持し、型に応じた読み方は `TypedTag` が決める）。
    pub fn as_str(&self) -> String {
        match (&self.tag_type, &self.label.value) {
            (TagType::Base(SType::ItemId), Bitical::Integer(i)) => {
                let o = Origin::within(*i);
                format!("{}({})", o.short(), i - o.block_lo())
            }
            _ => self.label.as_str(),
        }
    }
}

impl From<&str> for Bitical {
    fn from(s: &str) -> Self {
        Bitical::String(s.to_string())
    }
}

impl From<Label> for Bitical {
    fn from(l: Label) -> Self {
        l.value()
    }
}

impl From<&Label> for Bitical {
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
    pub typed_tag: TypedTag,
    pub origin: Origin,
}

impl Tags {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 新しいタグを追加します。
    pub fn push(&mut self, typed_tag: TypedTag, origin: Origin) {
        self.entries.push(TagEntry { typed_tag, origin });
    }

    /// 「型:値」のペアを生成するイテレータを返します。
    pub fn iter_typed_tags(&self) -> impl Iterator<Item = TypedTag> + '_ {
        self.entries.iter().map(|e| e.typed_tag.clone())
    }

    /// 指定された型のタグ値をリストとして取得します（リニアスキャン）。
    pub fn get_values(&self, key: &TagType) -> Vec<TagValue> {
        self.entries
            .iter()
            .filter(|e| e.typed_tag.tag_type() == *key)
            .map(|e| TagValue {
                label: e.typed_tag.label.clone(),
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
            map.entry(entry.typed_tag.tag_type())
                .or_default()
                .push(TagValue {
                    label: entry.typed_tag.label,
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
            map.entry(entry.typed_tag.tag_type())
                .or_default()
                .push(TagValue {
                    label: entry.typed_tag.label.clone(),
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
    BasenameScanHash,
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
    Query,
    // Type-specific tags
    Integer,
    Boolean,
    Double,
    Null,
    // Removed Files
    RemovedFile,
    RemovedFileAt,
    RemovedFilePath,
    RemovedFileSize,
    RemovedFileMtime,
    RemovedFileIsDir,
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
            Self::BasenameScanHash => "basename_scan_hash",
            Self::TypedTag => "tag",
            Self::Directory => "directory",
            Self::LabelStr => "label_str",
            Self::LabelInt => "label_int",
            Self::LabelDouble => "label_double",
            Self::LabelBool => "label_bool",
            Self::DataType => "data_type",
            Self::Value => "value",
            Self::Query => "query",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Double => "double",
            Self::Null => "null",
            Self::RemovedFile => "removed_file",
            Self::RemovedFileAt => "removed_file_at",
            Self::RemovedFilePath => "removed_file_path",
            Self::RemovedFileSize => "removed_file_size",
            Self::RemovedFileMtime => "removed_file_mtime",
            Self::RemovedFileIsDir => "removed_file_is_dir",
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
            "basename_scan_hash" => Ok(Self::BasenameScanHash),
            "tag" => Ok(Self::TypedTag),
            "directory" => Ok(Self::Directory),
            "label_str" => Ok(Self::LabelStr),
            "label_int" => Ok(Self::LabelInt),
            "label_double" => Ok(Self::LabelDouble),
            "label_bool" => Ok(Self::LabelBool),
            "data_type" => Ok(Self::DataType),
            "value" => Ok(Self::Value),
            "query" => Ok(Self::Query),
            "integer" => Ok(Self::Integer),
            "boolean" => Ok(Self::Boolean),
            "double" => Ok(Self::Double),
            "null" => Ok(Self::Null),
            "removed_file" => Ok(Self::RemovedFile),
            "removed_file_at" => Ok(Self::RemovedFileAt),
            "removed_file_path" => Ok(Self::RemovedFilePath),
            "removed_file_size" => Ok(Self::RemovedFileSize),
            "removed_file_mtime" => Ok(Self::RemovedFileMtime),
            "removed_file_is_dir" => Ok(Self::RemovedFileIsDir),
            _ => Err(format!("Unknown SType: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests_types {
    use super::*;

    #[test]
    fn test_date_time_utc_epoch_to_local_epoch_matches_chrono_local() {
        use chrono::{Local, TimeZone};
        // 2024-12-31 23:00:00 JST を表す UTC 秒
        let utc_epoch = Local
            .with_ymd_and_hms(2024, 12, 31, 23, 0, 0)
            .unwrap()
            .timestamp();
        let local_epoch = DateTime::utc_epoch_to_local_epoch(utc_epoch);
        // ローカル時刻として素朴に読める（timezone なしで解釈した）暦フィールドが
        // 元のローカル時刻と一致すること
        let naive = chrono::DateTime::from_timestamp(local_epoch, 0)
            .unwrap()
            .naive_utc();
        assert_eq!(naive.format("%Y-%m-%d %H:%M:%S").to_string(), "2024-12-31 23:00:00");
    }

    #[test]
    fn test_bitical_equality_is_bitwise() {
        // 保存値の同一性 = ビット同一性
        fn requires_eq<T: Eq>(_: &T) {}
        requires_eq(&Bitical::Integer(1));

        assert_eq!(Bitical::Double(1.5), Bitical::Double(1.5));
        assert_ne!(Bitical::Double(1.5), Bitical::Double(2.5));
        // NaN は自分自身と等しい（IEEE 比較とは異なる）
        assert_eq!(Bitical::Double(f64::NAN), Bitical::Double(f64::NAN));
        // 0.0 と -0.0 はビットが異なるため等しくない
        assert_ne!(Bitical::Double(0.0), Bitical::Double(-0.0));

        assert_eq!(Bitical::Integer(1), Bitical::Integer(1));
        assert_eq!(
            Bitical::String("a".to_string()),
            Bitical::String("a".to_string())
        );
        assert_eq!(Bitical::Boolean(true), Bitical::Boolean(true));
        assert_ne!(Bitical::String("1".to_string()), Bitical::Integer(1));

        let id = Uuid::new_v4();
        assert_eq!(Bitical::Uuid(id), Bitical::Uuid(id));
        assert_ne!(Bitical::Uuid(id), Bitical::Uuid(Uuid::new_v4()));
        assert_eq!(Bitical::Uuid(id).name(), BiticalType::Uuid);
    }

    #[test]
    fn test_bitical_ord_within_variant() {
        assert!(Bitical::Integer(1) < Bitical::Integer(2));
        assert!(
            Bitical::String("a".to_string()) < Bitical::String("b".to_string())
        );
        assert!(Bitical::Boolean(false) < Bitical::Boolean(true));
        assert!(Bitical::Double(1.0) < Bitical::Double(2.0));
        // total_cmp: -0.0 < +0.0（PartialEq のビット非等価性と整合）
        assert!(Bitical::Double(-0.0) < Bitical::Double(0.0));
    }

    #[test]
    fn test_bitical_ord_is_consistent_with_bitwise_eq() {
        // 同じビットパターンなら cmp は Equal を返す
        assert_eq!(
            Bitical::Double(f64::NAN).cmp(&Bitical::Double(f64::NAN)),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            Bitical::Double(0.0).cmp(&Bitical::Double(0.0)),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn test_bitical_ord_across_variants_is_stable() {
        // 変種間の順序は固定（String < Integer < Double < Boolean < Uuid）。
        // 具体的な並びより、推移律・反対称律を満たす全順序であることを確認する。
        let mut values = vec![
            Bitical::Uuid(Uuid::new_v4()),
            Bitical::Boolean(true),
            Bitical::Double(1.0),
            Bitical::Integer(1),
            Bitical::String("a".to_string()),
        ];
        values.sort();
        assert!(matches!(values[0], Bitical::String(_)));
        assert!(matches!(values[1], Bitical::Integer(_)));
        assert!(matches!(values[2], Bitical::Double(_)));
        assert!(matches!(values[3], Bitical::Boolean(_)));
        assert!(matches!(values[4], Bitical::Uuid(_)));
    }

    #[test]
    fn test_label_derives_ord_from_bitical() {
        // Label::other(Bitical) の Ord は Bitical::cmp に委譲される
        assert!(
            Label::other(Bitical::Integer(1))
                < Label::other(Bitical::Integer(2))
        );
    }

    #[test]
    fn typed_tag_is_default_node_true_for_fresh_tag() {
        let tt = TypedTag::new(SType::Size, 1024i64);
        assert!(tt.is_default_node());
    }

    #[test]
    fn typed_tag_node_builds_default_comparison_form_for_name_tag() {
        let tt = TypedTag::new(SType::Size, 1024i64);
        let expected = crate::query::Node::Query(Box::new(
            crate::query::ast::QueryNode::Comparison(crate::query::ast::ComparisonNode {
                first: crate::query::ast::Operand::TypeRef(tt.tag_type()),
                rest: vec![(
                    crate::query::ast::ComparisonOp::Label(crate::query::ast::BasicOp::Eq),
                    crate::query::ast::Operand::Literal(tt.label.clone()),
                )],
            }),
        ));
        assert_eq!(tt.node().as_ref(), &expected);
    }

    #[test]
    fn typed_tag_node_returns_real_node_unchanged_when_not_default() {
        let mut tt = TypedTag::new(SType::Size, 1024i64);
        let real = crate::query::Node::Query(Box::new(
            crate::query::ast::QueryNode::ColumnMatch {
                tag: SType::Size,
                label: tt.label.clone(),
            },
        ));
        tt.node = TypedTagNode::Node(real.clone());
        assert!(!tt.is_default_node());
        assert_eq!(tt.node().as_ref(), &real);
    }

    #[test]
    fn label_node_is_default_for_fresh_label() {
        let label = Label::from("x");
        assert_eq!(label.node(), &LabelNode::DefaultLabelNode);
    }

    #[test]
    fn label_node_returns_formatted_when_set() {
        let mut label = Label::from("1MB");
        let formatted = LabelNode::Formatted(
            crate::query::format::Formatted::Bitical(Bitical::Integer(1)),
        );
        label.node = formatted.clone();
        assert_eq!(label.node(), &formatted);
    }

    #[test]
    fn test_bitical_type_to_column() {
        assert_eq!(BiticalType::String.to_column(), SType::LabelStr);
        assert_eq!(BiticalType::Uuid.to_column(), SType::LabelStr);
        assert_eq!(BiticalType::Integer.to_column(), SType::LabelInt);
        assert_eq!(BiticalType::Double.to_column(), SType::LabelDouble);
        assert_eq!(BiticalType::Boolean.to_column(), SType::LabelBool);
    }

    #[test]
    fn test_bitical_type_to_columns_matches_storage_order() {
        // to_columns は宣言順 = 実際の保存列順（base_tags.parquet 等）。
        // 並びを変えると既存 parquet と DDL・appender の列順がズレる。
        assert_eq!(
            BiticalType::to_columns(),
            [
                SType::LabelStr,
                SType::LabelInt,
                SType::LabelDouble,
                SType::LabelBool,
            ]
        );
    }

    #[test]
    fn test_bitical_type_to_columns_scan_order_label_str_last() {
        // 走査順では label_str（全型の VARCHAR フォールバック）が末尾に来る
        assert_eq!(
            BiticalType::to_columns_scan_order(),
            [
                SType::LabelInt,
                SType::LabelDouble,
                SType::LabelBool,
                SType::LabelStr,
            ]
        );
    }

    #[test]
    fn test_bitical_to_sql_binds_all_variants() {
        use duckdb::types::{ToSqlOutput, Value};
        use duckdb::ToSql;

        fn bind(b: Bitical) -> Value {
            match b.to_sql().unwrap() {
                ToSqlOutput::Owned(v) => v,
                other => panic!("expected owned value, got {:?}", other),
            }
        }

        assert_eq!(
            bind(Bitical::String("a".to_string())),
            Value::Text("a".to_string())
        );
        assert_eq!(bind(Bitical::Integer(42)), Value::BigInt(42));
        assert_eq!(bind(Bitical::Double(1.5)), Value::Double(1.5));
        assert_eq!(bind(Bitical::Boolean(true)), Value::Boolean(true));
        let id = Uuid::new_v4();
        assert_eq!(bind(Bitical::Uuid(id)), Value::Text(id.to_string()));
    }

    #[test]
    fn test_bitical_associate_const() {
        assert_eq!(String::BITICAL, BiticalType::String);
        assert_eq!(i64::BITICAL, BiticalType::Integer);
        assert_eq!(Uuid::BITICAL, BiticalType::Uuid);
        assert_eq!(bool::BITICAL, BiticalType::Boolean);
        assert_eq!(FileSize::BITICAL, BiticalType::Integer);
        assert_eq!(FileTimestamp::BITICAL, BiticalType::Integer);
    }

    #[test]
    fn test_bitical_from_conversions() {
        assert_eq!(
            Bitical::from("a".to_string()),
            Bitical::String("a".to_string())
        );
        assert_eq!(Bitical::from(1i64), Bitical::Integer(1));
        assert_eq!(Bitical::from(1.5f64), Bitical::Double(1.5));
        assert_eq!(Bitical::from(true), Bitical::Boolean(true));
        let id = Uuid::new_v4();
        assert_eq!(Bitical::from(id), Bitical::Uuid(id));
        assert_eq!(Bitical::from(FileSize(42)), Bitical::Integer(42));
        assert_eq!(Bitical::from(FileTimestamp(99)), Bitical::Integer(99));
    }

    #[test]
    fn test_bitical_try_from_round_trip() {
        assert_eq!(
            String::try_from(Bitical::String("a".to_string())),
            Ok("a".to_string())
        );
        assert_eq!(i64::try_from(Bitical::Integer(1)), Ok(1));
        assert_eq!(f64::try_from(Bitical::Double(1.5)), Ok(1.5));
        assert_eq!(bool::try_from(Bitical::Boolean(true)), Ok(true));
        let id = Uuid::new_v4();
        assert_eq!(Uuid::try_from(Bitical::Uuid(id)), Ok(id));
        assert_eq!(FileSize::try_from(Bitical::Integer(42)), Ok(FileSize(42)));
        assert_eq!(
            FileTimestamp::try_from(Bitical::Integer(99)),
            Ok(FileTimestamp(99))
        );

        assert_eq!(
            i64::try_from(Bitical::String("x".to_string())),
            Err(Bitical::String("x".to_string()))
        );
    }

    #[test]
    fn test_bitical_from_matches_bitical_associate() {
        assert_eq!(String::BITICAL, Bitical::from("a".to_string()).name());
        assert_eq!(i64::BITICAL, Bitical::from(1i64).name());
        assert_eq!(Uuid::BITICAL, Bitical::from(Uuid::new_v4()).name());
        assert_eq!(bool::BITICAL, Bitical::from(true).name());
        assert_eq!(FileSize::BITICAL, Bitical::from(FileSize(1)).name());
        assert_eq!(
            FileTimestamp::BITICAL,
            Bitical::from(FileTimestamp(1)).name()
        );
    }

    #[test]
    fn test_label_from_string() {
        let label = Label::from("hello".to_string());
        assert_eq!(label.as_str(), "hello");
        assert_eq!(label.value(), Bitical::String("hello".to_string()));

        let numeric = Label::from("42".to_string());
        assert_eq!(numeric.as_i64(), 42);
    }

    #[test]
    fn test_typed_tag_display() {
        let tt = TypedTag::new("extension", "rs");
        assert_eq!(tt.to_string(), "extension:rs");

        let tt_int = TypedTag::new("size", 1024i64);
        assert_eq!(tt_int.to_string(), "size:1024");
    }

    #[test]
    fn test_typed_tag_retag_keeps_value_and_changes_type() {
        let tt = TypedTag::new("extension", "rs");
        assert_eq!(tt.tag_type(), TagType::from("extension"));

        let retagged = TypedTag::retag("category", &tt.label);
        assert_eq!(retagged.tag_type(), TagType::from("category"));
        assert_eq!(retagged.label, tt.label);
    }

    #[test]
    fn test_tags_iter_typed_tags() {
        let mut tags = Tags::new();
        tags.push(TypedTag::new("project", "A"), Origin::User);
        tags.push(TypedTag::new("project", "B"), Origin::User);
        tags.push(TypedTag::new("extension", "rs"), Origin::User);

        let mut results: Vec<String> =
            tags.iter_typed_tags().map(|tt| tt.to_string()).collect();
        results.sort();

        assert_eq!(results.len(), 3);
        assert!(results.contains(&"project:A".to_string()));
        assert!(results.contains(&"project:B".to_string()));
        assert!(results.contains(&"extension:rs".to_string()));
    }

    #[test]
    fn label_content_has_correct_tag_type_and_str() {
        let label = TypedTag::new(SType::Content, "hello").label;
        assert_eq!(label.as_str(), "hello");
    }

    #[test]
    fn label_no_longer_carries_type_identity() {
        let size = TypedTag::new(SType::Size, 1024).label;
        let rank = TypedTag::new(SType::Rank, 1024).label;
        assert_eq!(
            size, rank,
            "Label must not distinguish types by itself; only the enclosing \
             TypedTag's tag_type does"
        );
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
    fn test_item_id_settling_as_i64_casts_counter() {
        let id = ItemId::Volatile(456).settle(Origin::Plugin);
        assert_eq!(id.as_i64(), 456);
    }

    #[test]
    fn test_item_id_settle_reuses_counter() {
        let vid = ItemId::Volatile(456);
        let sid = vid.settle(Origin::Plugin);
        assert_eq!(sid, ItemId::Settling(Origin::Plugin, 456));
        assert!(sid.is_settling());
        assert!(!sid.is_volatile());
    }

    #[test]
    fn test_origin_large() {
        // 大分類 (User/System)。System 側 (Builtin/File/Plugin) は LargeOrigin::System に収束する。
        assert_eq!(Origin::User.large(), LargeOrigin::User);
        assert_eq!(Origin::Builtin.large(), LargeOrigin::System);
        assert_eq!(Origin::File.large(), LargeOrigin::System);
        assert_eq!(Origin::Plugin.large(), LargeOrigin::System);
    }

    #[test]
    fn test_origin_is_user_is_system() {
        assert!(Origin::User.is_user());
        assert!(!Origin::User.is_system());

        assert!(!Origin::Builtin.is_user());
        assert!(Origin::Builtin.is_system());
        assert!(!Origin::File.is_user());
        assert!(Origin::File.is_system());
        assert!(!Origin::Plugin.is_user());
        assert!(Origin::Plugin.is_system());
    }

    #[test]
    fn test_large_origin_display() {
        assert_eq!(LargeOrigin::System.to_string(), "system");
        assert_eq!(LargeOrigin::User.to_string(), "user");
    }

    #[test]
    fn test_item_id_display() {
        // Stored は Origin 区画のローカル形式
        assert_eq!(ItemId::Stored(123).to_string(), "User(123)");
        assert_eq!(
            ItemId::Stored(Origin::Builtin.block_lo() + 10).to_string(),
            "Sys(10)"
        );
        assert_eq!(
            ItemId::Stored(Origin::File.block_lo() + 1).to_string(),
            "File(1)"
        );

        // Volatile は区画によらず ~(n)
        assert_eq!(ItemId::Volatile(456).to_string(), "~(456)");

        // Settling は ~{origin.short()}(n)
        assert_eq!(
            ItemId::Volatile(3).settle(Origin::Plugin).to_string(),
            "~Plg(3)"
        );
        assert_eq!(
            ItemId::Volatile(5).settle(Origin::User).to_string(),
            "~User(5)"
        );
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

    // --- Phase 2: DateTime::FromStr ---

    #[test]
    fn test_datetime_from_str_ymd_hyphen() {
        use chrono::NaiveDate;
        let dt: DateTime = "2026-02-01".parse().unwrap();
        assert_eq!(
            dt,
            DateTime::Date(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap())
        );
    }

    #[test]
    fn test_datetime_from_str_ymd_slash() {
        use chrono::NaiveDate;
        let dt: DateTime = "2026/02/01".parse().unwrap();
        assert_eq!(
            dt,
            DateTime::Date(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap())
        );
    }

    #[test]
    fn test_datetime_from_str_year_month() {
        let dt: DateTime = "2026-02".parse().unwrap();
        assert_eq!(
            dt,
            DateTime::YearMonth {
                year: 2026,
                month: 2
            }
        );
    }

    /// 裸の4桁整数は区切りも時間語彙も持たない目印なしの表記なので、
    /// parse_structured はもう主張しない（Year 単体解釈は MtimeFn 側の型文脈の知識）。
    #[test]
    fn test_date_time_parse_structured_declines_bare_year() {
        assert_eq!(DateTime::parse_structured("2026"), None);
    }

    #[test]
    fn test_datetime_from_str_declines_bare_integer() {
        for s in ["2026", "0", "123456"] {
            assert!(
                s.parse::<DateTime>().is_err(),
                "should decline a bare integer with no marker: {s}"
            );
        }
    }

    #[test]
    fn test_datetime_from_str_today() {
        // コロンなしの自然言語日付は日精度（丸1日を表す。util::parse_datetime の
        // 「非コロン一致は丸1日」広げと同じ精度に統合。旧 Instant 判定は撤回）
        let dt: DateTime = "today".parse().unwrap();
        assert!(matches!(dt, DateTime::Date(_)));
    }

    #[test]
    fn test_datetime_from_str_relative() {
        // "7d ago" にもコロンが無いため日精度
        let dt: DateTime = "7d ago".parse().unwrap();
        assert!(matches!(dt, DateTime::Date(_)));
    }

    #[test]
    fn test_datetime_from_str_hm_minute_precision() {
        // コロン1個（秒指定なし）は分精度
        let dt: DateTime = "12:30".parse().unwrap();
        match dt {
            DateTime::Minute(ndt) => {
                use chrono::Timelike;
                assert_eq!((ndt.hour(), ndt.minute()), (12, 30));
            }
            other => panic!("expected Minute, got {other:?}"),
        }
        let floor = dt.floor().unwrap();
        let ceiling = dt.ceiling().unwrap();
        use chrono::Timelike;
        assert_eq!(floor.second(), 0);
        assert_eq!(ceiling.second(), 59);
        assert_eq!(floor.minute(), 30);
        assert_eq!(ceiling.minute(), 30);
    }

    #[test]
    fn test_datetime_from_str_hms_instant_precision() {
        // コロン2個（秒指定あり）は瞬間（floor == ceiling）
        let dt: DateTime = "12:30:05".parse().unwrap();
        assert!(matches!(dt, DateTime::Instant(_)));
        assert_eq!(dt.floor(), dt.ceiling());
    }

    #[test]
    fn test_datetime_from_str_md_shorthand_this_year() {
        use chrono::{Datelike, Local, NaiveDate};
        let dt: DateTime = "1/10".parse().unwrap();
        let this_year = Local::now().year();
        assert_eq!(
            dt,
            DateTime::Date(NaiveDate::from_ymd_opt(this_year, 1, 10).unwrap())
        );
    }

    #[test]
    fn test_datetime_from_str_ym_slash() {
        let dt: DateTime = "2013/1".parse().unwrap();
        assert_eq!(
            dt,
            DateTime::YearMonth {
                year: 2013,
                month: 1
            }
        );
    }

    #[test]
    fn test_datetime_from_str_unknown_returns_err() {
        let result = "not_a_date".parse::<DateTime>();
        assert!(result.is_err());
    }

    // --- DateTime / Label::Date ---

    #[test]
    fn test_datetime_year_variant() {
        let dt = DateTime::Year(2026);
        assert!(matches!(dt, DateTime::Year(2026)));
    }

    #[test]
    fn test_datetime_year_month_variant() {
        let dt = DateTime::YearMonth {
            year: 2026,
            month: 2,
        };
        assert!(matches!(
            dt,
            DateTime::YearMonth {
                year: 2026,
                month: 2
            }
        ));
    }

    #[test]
    fn test_datetime_date_variant() {
        use chrono::NaiveDate;
        let d = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = DateTime::Date(d);
        assert!(matches!(dt, DateTime::Date(_)));
    }

    #[test]
    fn test_datetime_instant_variant() {
        use chrono::Local;
        let now = Local::now();
        let dt = DateTime::Instant(now);
        assert!(matches!(dt, DateTime::Instant(_)));
    }

    #[test]
    fn test_label_value_date_as_display_name() {
        use chrono::NaiveDate;
        let d = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        assert_eq!(DateTime::Date(d).as_display_str(), "2026-02-01");
    }

    #[test]
    fn test_label_value_date_year_as_display_name() {
        assert_eq!(DateTime::Year(2026).as_display_str(), "2026");
    }

    #[test]
    fn test_label_value_date_year_month_as_display_name() {
        assert_eq!(
            DateTime::YearMonth {
                year: 2026,
                month: 2,
            }
            .as_display_str(),
            "2026-02"
        );
    }

    #[test]
    fn test_datetime_year_floor_ceiling() {
        use chrono::NaiveDate;
        let dt = DateTime::Year(2026);
        let floor = dt.floor().unwrap();
        let ceiling = dt.ceiling().unwrap();
        assert_eq!(
            floor,
            NaiveDate::from_ymd_opt(2026, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        );
        assert_eq!(
            ceiling,
            NaiveDate::from_ymd_opt(2026, 12, 31)
                .unwrap()
                .and_hms_opt(23, 59, 59)
                .unwrap()
        );
    }

    #[test]
    fn test_datetime_year_month_floor_ceiling() {
        use chrono::NaiveDate;
        let dt = DateTime::YearMonth {
            year: 2026,
            month: 2,
        };
        let floor = dt.floor().unwrap();
        let ceiling = dt.ceiling().unwrap();
        assert_eq!(
            floor,
            NaiveDate::from_ymd_opt(2026, 2, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        );
        assert_eq!(
            ceiling,
            NaiveDate::from_ymd_opt(2026, 2, 28)
                .unwrap()
                .and_hms_opt(23, 59, 59)
                .unwrap()
        );
    }

    #[test]
    fn test_datetime_date_floor_ceiling() {
        use chrono::NaiveDate;
        let d = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = DateTime::Date(d);
        let floor = dt.floor().unwrap();
        let ceiling = dt.ceiling().unwrap();
        assert_eq!(floor, d.and_hms_opt(0, 0, 0).unwrap());
        assert_eq!(ceiling, d.and_hms_opt(23, 59, 59).unwrap());
    }

    #[test]
    fn test_datetime_instant_floor_ceiling() {
        use chrono::{Local, TimeZone};
        let local_dt = Local.with_ymd_and_hms(2026, 2, 1, 12, 0, 0).unwrap();
        let ts = local_dt.timestamp();
        let dt = DateTime::Instant(local_dt);
        let floor = dt.floor().unwrap();
        let ceiling = dt.ceiling().unwrap();
        // floor/ceiling は NaiveDateTime(local) → Local → timestamp で元の ts に戻る
        let floor_ts = Local
            .from_local_datetime(&floor)
            .earliest()
            .unwrap()
            .timestamp();
        let ceiling_ts = Local
            .from_local_datetime(&ceiling)
            .earliest()
            .unwrap()
            .timestamp();
        assert_eq!(floor_ts, ts);
        assert_eq!(ceiling_ts, ts);
    }

    // --- DateTimeRange（旧 util::DatetimeRange。区間 | スロット制約の2形） ---

    #[test]
    fn test_date_time_range_interval_round_trips() {
        let range = DateTimeRange::interval(100, 200);
        assert_eq!(range.as_interval(), Some((100, 200)));
    }

    #[test]
    fn test_date_time_range_interval_equality() {
        assert_eq!(
            DateTimeRange::interval(100, 200),
            DateTimeRange::interval(100, 200)
        );
        assert_ne!(
            DateTimeRange::interval(100, 200),
            DateTimeRange::interval(100, 201)
        );
    }

    /// スロット制約は生タイムスタンプの区間を持たない。
    #[test]
    fn test_date_time_range_slots_have_no_interval() {
        let range = DateTimeRange::parse_slot_glob("*-02-01").unwrap();
        assert_eq!(range.as_interval(), None);
    }

    /// フィールド単位の glob として受理するパターン。
    /// フィールドが足りない分は自由（`2026-*` は2026年全体）。
    #[test]
    fn test_date_time_range_parse_slot_glob_accepts_field_globs() {
        for pattern in [
            "*-*-*",
            "*-02-01",
            "2026-*",
            "2026-*-01",
            "*-*-15",
            "12:*",
            "*-02-01T12:*",
        ] {
            assert!(
                DateTimeRange::parse_slot_glob(pattern).is_some(),
                "受理されるべき: {pattern}"
            );
        }
    }

    /// 裸の `*` は区切りも単位も持たない汎用の全一致 glob なので、日付型と見做せない。
    #[test]
    fn test_date_time_range_parse_slot_glob_declines_bare_wildcard() {
        assert!(
            DateTimeRange::parse_slot_glob("*").is_none(),
            "should decline a bare *"
        );
    }

    /// フィールド内の文字単位の部分 glob は受理しない（年の前方一致も含む）。
    /// 「2000年以降」のような範囲は glob ではなく比較式で書く。
    #[test]
    fn test_date_time_range_parse_slot_glob_rejects_partial_field_glob() {
        for pattern in [
            "2026-0*", "20*", "*-0*-01", "20*-1*", "12:3*", "2026-02-0*",
        ] {
            assert!(
                DateTimeRange::parse_slot_glob(pattern).is_none(),
                "フィールド内の部分 glob は拒否されるべき: {pattern}"
            );
        }
    }

    /// glob を含まないものは通常の日付解釈に任せる（スロットにはしない）。
    #[test]
    fn test_date_time_range_parse_slot_glob_requires_glob_char() {
        for pattern in ["2026-02-01", "2026", "12:30"] {
            assert!(
                DateTimeRange::parse_slot_glob(pattern).is_none(),
                "glob でないものは拒否されるべき: {pattern}"
            );
        }
    }

    /// フィールド数の不正・空フィールドは受理しない。
    #[test]
    fn test_date_time_range_parse_slot_glob_rejects_malformed() {
        for pattern in ["2026-", "*-", "*-1-2-3", "12:*:*:*", "-*"] {
            assert!(
                DateTimeRange::parse_slot_glob(pattern).is_none(),
                "壊れたパターンは拒否されるべき: {pattern}"
            );
        }
    }

    /// 欠けた末尾フィールドはペアとして返らない（パディングしない）。
    #[test]
    fn test_split_slot_fields_omits_trailing_free_fields() {
        assert_eq!(
            DateTimeRange::split_slot_fields("2026-*"),
            Some(vec![(DateField::Year, "2026"), (DateField::Month, "*")])
        );
    }

    #[test]
    fn test_split_slot_fields_returns_written_values_only() {
        assert_eq!(
            DateTimeRange::split_slot_fields("*-02-01T12:*"),
            Some(vec![
                (DateField::Year, "*"),
                (DateField::Month, "02"),
                (DateField::Day, "01"),
                (DateField::Hour, "12"),
                (DateField::Minute, "*"),
            ])
        );
    }

    #[test]
    fn test_date_time_to_interval_matches_floor_ceiling() {
        use chrono::{Local, NaiveDate, TimeZone};
        let dt = DateTime::Date(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());
        let range = dt.to_interval().unwrap();
        let (start, end) = range.as_interval().unwrap();
        let expected_start = Local
            .from_local_datetime(&dt.floor().unwrap())
            .earliest()
            .unwrap()
            .timestamp();
        let expected_end = Local
            .from_local_datetime(&dt.ceiling().unwrap())
            .earliest()
            .unwrap()
            .timestamp();
        assert_eq!(start, expected_start);
        assert_eq!(end, expected_end);
        assert!(start < end);
    }

    // --- DateTimeRange::parse (unifies parse_structured / parse_slot_glob / FromStr) ---

    #[test]
    fn test_date_time_range_parse_delegates_to_slot_glob() {
        assert_eq!(
            DateTimeRange::parse("*-02-01"),
            Some(Ok(DateTimeRange::parse_slot_glob("*-02-01").unwrap()))
        );
    }

    #[test]
    fn test_date_time_range_parse_delegates_to_structured_date() {
        use chrono::NaiveDate;
        let dt = DateTime::Date(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());
        assert_eq!(DateTimeRange::parse("2026-02-01"), Some(Ok(dt.to_interval().unwrap())));
    }

    #[test]
    fn test_date_time_range_parse_claims_natural_language() {
        let dt: DateTime = "today".parse().unwrap();
        assert_eq!(DateTimeRange::parse("today"), Some(Ok(dt.to_interval().unwrap())));
    }

    #[test]
    fn test_date_time_range_parse_reports_invalid_calendar_date() {
        assert!(matches!(DateTimeRange::parse("2026-13-45"), Some(Err(_))));
    }

    /// 区切り記号も時間語彙も持たない裸の数値は日付の目印を持たない。
    /// 小数も同様（chrono_english は `0.1` を日付として掴んでしまう）。
    #[test]
    fn test_date_time_range_parse_declines_bare_number() {
        for s in ["2026", "0", "123456", "0.1", "3.14", "1e5"] {
            assert_eq!(DateTimeRange::parse(s), None, "should decline a bare number: {s}");
        }
    }

    #[test]
    fn test_date_time_range_parse_declines_unrecognized_garbage() {
        assert_eq!(DateTimeRange::parse("not_a_date"), None);
    }

    #[test]
    fn test_date_time_range_parse_declines_malformed_glob() {
        for pattern in ["2026-0*", "20*", "*-0*-01"] {
            assert_eq!(
                DateTimeRange::parse(pattern),
                None,
                "should decline a malformed field glob: {pattern}"
            );
        }
    }
}

impl From<SType> for &'static str {
    fn from(stype: SType) -> Self {
        stype.as_str()
    }
}
