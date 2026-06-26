// Copyright (C) 2026 coponhub
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
    /// システム定義アイテム（type/tag 定義）— item_references、負側区画
    System,
    /// ユーザー作成アイテム（note 等）— item_references、正側区画 0
    User,
    /// ファイル — file_references、正側区画 8
    File,
}

impl Origin {
    /// 区画幅 B = 2^58（i64 空間を 64 分割した 1 区画のサイズ）。
    pub const SPACE_SIZE: i64 = 1 << 58;

    /// origin の区画 index。負値は負側区画。Origin 追加時はここだけ変える。
    pub fn space_index(self) -> i64 {
        match self {
            Origin::System => -1,
            Origin::User => 0,
            Origin::File => 8,
        }
    }

    /// 区画下端 lo = index * SPACE_SIZE。
    pub fn space_lo(self) -> i64 {
        self.space_index() * Self::SPACE_SIZE
    }

    /// 区画上端 hi（排他）。直上 origin の lo、最上位は i64::MAX。
    pub fn space_hi(self) -> i64 {
        use strum::IntoEnumIterator;
        let lo = self.space_lo();
        Origin::iter()
            .map(|o| o.space_lo())
            .filter(|&l| l > lo)
            .min()
            .unwrap_or(i64::MAX)
    }

    /// origin の短縮ラベル。System→"Sys"、User→"User"、File→"File"。
    pub fn short(self) -> &'static str {
        match self {
            Origin::System => "Sys",
            Origin::User => "User",
            Origin::File => "File",
        }
    }

    /// id → Origin 逆引き（全域関数）。
    /// `lo <= id` を満たす区画のうち lo が最大のものを返す。
    /// 全区画より下（負値等）の場合は lo 最小の Origin に縮退。
    pub fn within(id: i64) -> Self {
        use strum::IntoEnumIterator;
        Origin::iter()
            .filter(|&o| o.space_lo() <= id)
            .max_by_key(|&o| o.space_lo())
            .unwrap_or_else(|| {
                Origin::iter()
                    .min_by_key(|&o| o.space_lo())
                    .expect("Origin must have at least one variant")
            })
    }
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
    Content(String), // write 専用。item_references.content 固定カラムへの書き込みを表現する。
    Extension(String),
    Path(String),
    ItemId(i64),
    FileId(Uuid),
    IsDir(bool),
    /// 日付リテラル（normalize_label で生成される中間表現）
    Date(DateTime),

    // --- 汎用・未解決型 ---
    /// 標準外のタグ、または明示的にドメインを特定しない汎用値。
    /// タグの型（TagType）を自律的に保持します。
    Other(TagType, LabelValue),
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
    /// 時点（"today", "7d ago" など）
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
            DateTime::Instant(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        }
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
}

impl std::str::FromStr for DateTime {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        use chrono::{Local, NaiveDate};

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
            let y: i32 = parts[0].parse().map_err(|_| ())?;
            let m: u32 = parts[1].parse().map_err(|_| ())?;
            let d: u32 = parts[2].parse().map_err(|_| ())?;
            return NaiveDate::from_ymd_opt(y, m, d)
                .map(DateTime::Date)
                .ok_or(());
        }

        // YYYY-MM / YYYY/MM
        if parts.len() == 2
            && parts[0].len() == 4
            && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
        {
            let y: i32 = parts[0].parse().map_err(|_| ())?;
            let m: u32 = parts[1].parse().map_err(|_| ())?;
            NaiveDate::from_ymd_opt(y, m, 1).ok_or(())?;
            return Ok(DateTime::YearMonth { year: y, month: m });
        }

        // YYYY（4桁年単体）
        if parts.len() == 1
            && parts[0].len() == 4
            && parts[0].chars().all(|c| c.is_ascii_digit())
        {
            let y: i32 = parts[0].parse().map_err(|_| ())?;
            if (1000..=9999).contains(&y) {
                return Ok(DateTime::Year(y));
            }
            return Err(());
        }

        // 自然言語・相対日付（"today", "7d ago" 等）
        let s_lower = s.to_lowercase();
        let now = Local::now();
        if let Ok(dt) = chrono_english::parse_date_string(
            &s_lower,
            now,
            chrono_english::Dialect::Uk,
        ) {
            return Ok(DateTime::from_localtime(dt));
        }
        if let Ok(dt) = dateparser::parse_with_timezone(s, &Local) {
            return Ok(DateTime::from_localtime(dt));
        }

        Err(())
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
    Date(DateTime), // -> "date"
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
            LabelValue::Date(dt) => dt.as_display_str(),
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
            | Label::Content(s)
            | Label::Extension(s)
            | Label::Path(s) => s.clone(),
            Label::Rank(i) | Label::Size(i) | Label::Mtime(i) => i.to_string(),
            Label::ItemId(i) => {
                let o = Origin::within(*i);
                format!("{}({})", o.short(), i - o.space_lo())
            }
            Label::FileId(u) => u.to_string(),
            Label::IsDir(b) => LabelValue::Boolean(*b).as_display_name(),
            Label::Date(dt) => dt.as_display_str(),
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
            Label::Date(_) => 0,
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
            Label::Content(_) => TagType::Base(SType::Content),
            Label::Extension(_) => TagType::Base(SType::Extension),
            Label::Path(_) => TagType::Base(SType::Path),
            Label::ItemId(_) => TagType::Base(SType::ItemId),
            Label::FileId(_) => TagType::Base(SType::FileId),
            Label::IsDir(_) => TagType::Base(SType::IsDir),
            Label::Date(_) => TagType::Base(SType::Mtime),
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
            | Label::Content(s)
            | Label::Extension(s)
            | Label::Path(s) => LabelValue::String(s.clone()),
            Label::Rank(i)
            | Label::Size(i)
            | Label::Mtime(i)
            | Label::ItemId(i) => LabelValue::Integer(*i),
            Label::FileId(u) => LabelValue::String(u.to_string()),
            Label::IsDir(b) => LabelValue::Boolean(*b),
            Label::Date(dt) => LabelValue::Date(dt.clone()),
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
            LabelValue::Date(dt) => Value::BigInt(dt.to_timestamp()),
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
    Query,
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
            Self::Query => "query",
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
            "query" => Ok(Self::Query),
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
    fn label_content_has_correct_tag_type_and_str() {
        let label = Label::Content("hello".to_string());
        assert_eq!(label.tag_type(), TagType::Base(SType::Content));
        assert_eq!(label.as_str(), "hello");
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

    #[test]
    fn test_datetime_from_str_year_only() {
        let dt: DateTime = "2026".parse().unwrap();
        assert_eq!(dt, DateTime::Year(2026));
    }

    #[test]
    fn test_datetime_from_str_today() {
        let dt: DateTime = "today".parse().unwrap();
        assert!(matches!(dt, DateTime::Instant(_)));
    }

    #[test]
    fn test_datetime_from_str_relative() {
        let dt: DateTime = "7d ago".parse().unwrap();
        assert!(matches!(dt, DateTime::Instant(_)));
    }

    #[test]
    fn test_datetime_from_str_unknown_returns_err() {
        let result = "not_a_date".parse::<DateTime>();
        assert!(result.is_err());
    }

    // --- Phase 1: DateTime / LabelValue::Date / Label::Date ---

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
        let lv = LabelValue::Date(DateTime::Date(d));
        assert_eq!(lv.as_display_name(), "2026-02-01");
    }

    #[test]
    fn test_label_value_date_year_as_display_name() {
        let lv = LabelValue::Date(DateTime::Year(2026));
        assert_eq!(lv.as_display_name(), "2026");
    }

    #[test]
    fn test_label_value_date_year_month_as_display_name() {
        let lv = LabelValue::Date(DateTime::YearMonth {
            year: 2026,
            month: 2,
        });
        assert_eq!(lv.as_display_name(), "2026-02");
    }

    #[test]
    fn test_label_date_value_returns_date_variant() {
        use chrono::NaiveDate;
        let d = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let label = Label::Date(DateTime::Date(d));
        assert!(matches!(label.value(), LabelValue::Date(DateTime::Date(_))));
    }

    #[test]
    fn test_label_date_as_str() {
        use chrono::NaiveDate;
        let d = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let label = Label::Date(DateTime::Date(d));
        assert_eq!(label.as_str(), "2026-02-01");
    }

    #[test]
    fn test_label_date_as_i64_returns_zero() {
        let label = Label::Date(DateTime::Year(2026));
        assert_eq!(label.as_i64(), 0);
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
}

impl From<SType> for &'static str {
    fn from(stype: SType) -> Self {
        stype.as_str()
    }
}
