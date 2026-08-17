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

use crate::types::Bitical;
pub use crate::types::BiticalType;
use anyhow::{Context, Result};
use duckdb::Connection;
use sea_query::{
    Alias, BinOper, ColumnDef as SeaColumnDef, Expr, Func, Iden, IntoIden,
    IntoTableRef, SimpleExpr, Table, TableCreateStatement, TableRef,
};
use std::path::{Path, PathBuf};
use strum::{Display, EnumIter};

pub mod identifier;
pub mod sql;

pub use self::sql::CustomFunc;

/// カラムが所属すべきテーブル。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, Display, Iden)]
#[strum(serialize_all = "snake_case")]
pub enum TargetTable {
    FileReferences,
    Locations,
    BaseTags,
    TagsByLocation,
    ItemReferences,
    SystemTags,
    UserTags,
    DataTypes,
    RemovedFiles,
}

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub bitical_type: BiticalType,
    pub target_table: TargetTable,
}

pub fn open_connection() -> duckdb::Result<Connection> {
    let config = duckdb::Config::default().enable_autoload_extension(false)?;
    Connection::open_in_memory_with_flags(config)
}

/// DB接続とParquetファイルのパスを管理する構造体（設計書4.4 IndexStore）。
pub struct Store {
    pub conn: Connection,
    pub db_dir: PathBuf,
}

impl Store {
    /// db_dir を準備しインメモリDB接続を開く。
    /// テーブル初期化は呼び出し元が `Indexer::new(store, registry).initialize_tables()` で行う。
    pub fn open(db_dir: impl AsRef<Path>) -> Result<Self> {
        let db_dir = db_dir.as_ref().to_path_buf();
        if !db_dir.exists() {
            std::fs::create_dir_all(&db_dir).with_context(|| {
                format!("Failed to create db dir: {:?}", db_dir)
            })?;
        }
        let conn = open_connection()
            .context("Failed to open in-memory DuckDB connection")?;
        Ok(Self { conn, db_dir })
    }

    /// 同一インメモリDBを共有するクローンを返す（テスト用）。
    /// テーブルはすでに共有されるため initialize_tables は不要。
    pub fn try_clone(&self) -> Result<Self> {
        let conn = self
            .conn
            .try_clone()
            .context("Failed to clone DuckDB connection")?;
        Ok(Self {
            conn,
            db_dir: self.db_dir.clone(),
        })
    }

    /// 指定されたディレクトリを物理削除する（Clear コマンド用）。
    pub fn delete_database(db_dir: &Path) -> Result<()> {
        if db_dir.exists() {
            std::fs::remove_dir_all(db_dir).with_context(|| {
                format!("Failed to remove db dir: {:?}", db_dir)
            })?;
        }
        Ok(())
    }

    /// インデックスをクリアしてディレクトリを空に再作成する。
    pub fn clear(&self) -> Result<()> {
        if self.db_dir.exists() {
            std::fs::remove_dir_all(&self.db_dir)
                .context("Failed to clear database directory")?;
        }
        std::fs::create_dir_all(&self.db_dir)
            .context("Failed to recreate database directory")?;
        Ok(())
    }

    /// ターゲットテーブルに対応するパスを生成します。
    pub fn path_for_target(&self, target: TargetTable) -> PathBuf {
        self.db_dir.join(format!("{}.parquet", target))
    }

    /// 一時的なスキャン結果の保存先パスを返します。
    pub fn temp_scan_path(&self) -> PathBuf {
        self.db_dir.join("current_scan.parquet")
    }

    /// 一時的な生存 ID リストの保存先パスを返します。
    pub fn temp_live_path(&self) -> PathBuf {
        self.db_dir.join("live_ids.parquet")
    }

    /// ファイルインデックスに関連する Parquet ファイルおよびキャッシュを削除する。
    pub fn clear_index(&self) -> Result<()> {
        let targets = [
            TargetTable::FileReferences,
            TargetTable::Locations,
            TargetTable::BaseTags,
            TargetTable::TagsByLocation,
        ];
        for target in targets {
            let path = self.path_for_target(target);
            if path.exists() {
                std::fs::remove_file(&path).with_context(|| {
                    format!("Failed to remove index file: {:?}", path)
                })?;
            }
        }

        // 一時ファイルの削除
        let temp_files = [self.temp_scan_path(), self.temp_live_path()];
        for path in temp_files {
            if path.exists() {
                std::fs::remove_file(&path).with_context(|| {
                    format!("Failed to remove temporary file: {:?}", path)
                })?;
            }
        }

        // キャッシュディレクトリの削除
        let cache_dir = self.db_dir.join("cache");
        if cache_dir.exists() {
            std::fs::remove_dir_all(&cache_dir).with_context(|| {
                format!("Failed to remove cache directory: {:?}", cache_dir)
            })?;
        }

        Ok(())
    }
}

/// データベースのテーブル名を表す識別子。
#[derive(Iden, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tbl {
    FileReferences,
    Locations,
    BaseTags,
    TagsByLocation,
    ItemReferences,
    SystemTags,
    UserTags,
    DataTypes,
    RemovedFiles,
    #[iden = "oneview"]
    OneView,
    #[iden = "_oneview"]
    _OneView,

    // --- Diff Tables ---
    FileReferencesDiff,
    LocationsDiff,
    BaseTagsDiff,
    TagsByLocationDiff,
    ItemReferencesDiff,
    SystemTagsDiff,
    UserTagsDiff,
    DataTypesDiff,

    // --- Work Tables ---
    Scan,
    Live,
    Item,
    IdItem,
    Target,
    Master,
}

/// SQL クエリのデータソース（OneView テーブルまたは Parquet ファイル）。
#[derive(Clone, Debug)]
pub enum Src {
    OneView,
    Parquet(String),
}

impl Src {
    /// format! 内でテーブル式として使う文字列を返す。
    pub fn table_str(&self) -> String {
        match self {
            Src::OneView => Iden::to_string(&Tbl::OneView),
            Src::Parquet(path) => {
                format!("read_parquet('{}')", path.replace('\'', "''"))
            }
        }
    }
}

impl IntoTableRef for &Src {
    fn into_table_ref(self) -> TableRef {
        match self {
            Src::OneView => TableRef::Table(Tbl::OneView.into_iden()),
            Src::Parquet(path) => TableRef::FunctionCall(
                Func::cust(DuckDbFunc::ReadParquet)
                    .arg(Expr::val(path.as_str())),
                Alias::new("src").into_iden(),
            ),
        }
    }
}

/// SQL 内部で使われる中間的な識別子（サブクエリエイリアス・中間カラム名）。
/// `use crate::db::Pronoun::*;` でインポートして使用する。
#[derive(Iden, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pronoun {
    // --- 元 Tbl から移行 ---
    Sub,
    Diff,
    AllHits,
    TopItems,
    PickedIds,
    GroupTotal,
    Rn,
    // --- サブクエリエイリアス ---
    View,
    Proj,
    Pivot,
    NvFilter,
    Pk,
    // --- JOIN エイリアス ---
    #[iden = "L"]
    L,
    #[iden = "R"]
    R,
    // --- 中間カラム ---
    Nvalue,
    Group,
    Representative,
    Label,
    Scalar,
    // --- 追加分 ---
    Agg,
    Ctx,
    Filter,
    Tags,
    Deduped,
    Val,
    Kind,
    Key,
    Stored,
    Volatile,
    OrdSrc,
}

/// Volatile Column
/// クエリの結果にのみあるような永続化されていないカラム名
#[derive(Iden, Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum VCol {
    Total,
}

impl Iden for BiticalType {
    fn unquoted(&self, s: &mut dyn std::fmt::Write) {
        match self {
            BiticalType::String => write!(s, "VARCHAR").unwrap(),
            BiticalType::Integer => write!(s, "BIGINT").unwrap(),
            BiticalType::Double => write!(s, "DOUBLE").unwrap(),
            BiticalType::Boolean => write!(s, "BOOLEAN").unwrap(),
            BiticalType::Uuid => write!(s, "UUID").unwrap(),
        }
    }
}

impl BiticalType {
    pub fn prepare_column<'a>(
        &self,
        def: &'a mut SeaColumnDef,
    ) -> &'a mut SeaColumnDef {
        match self {
            BiticalType::String => def.string(),
            BiticalType::Integer => def.big_integer(),
            BiticalType::Double => def.double(),
            BiticalType::Boolean => def.boolean(),
            BiticalType::Uuid => def.custom(BiticalType::Uuid),
        }
    }

    pub fn from_col(col: Col) -> Self {
        match col {
            Col::LabelStr => BiticalType::String,
            Col::LabelInt
            | Col::ItemId
            | Col::Rank
            | Col::ScanHash
            | Col::BasenameScanHash => BiticalType::Integer,
            Col::LabelDouble => BiticalType::Double,
            Col::LabelBool => BiticalType::Boolean,
            _ => BiticalType::String,
        }
    }

    /// この型が DuckDB の `typeof()` として返す型名。Integer/Uuid は None
    /// （`SUM(BIGINT)` が `HUGEINT` を返すなど整数系は typeof 名を固定できない
    /// ため、既定枝として扱われる）。`REAL`/`FLOAT` 型のカラムはこのプロジェクトの
    /// スキーマに存在せず、集約も DOUBLE に昇格するため `FLOAT` は考慮しない。
    pub(crate) fn to_typeofstr(&self) -> Option<&'static str> {
        match self {
            BiticalType::Boolean => Some("BOOLEAN"),
            BiticalType::Double => Some("DOUBLE"),
            BiticalType::String => Some("VARCHAR"),
            BiticalType::Integer | BiticalType::Uuid => None,
        }
    }

    /// タグ値 UNION（`CustomFunc::union_type`）でこの型が収束するアーム。
    /// アーム名は収束先の Display、SQL 型は Iden 綴り。Uuid は文字列表現のため
    /// String アームに収束する。
    pub(crate) fn union_arm(&self) -> BiticalType {
        match self {
            BiticalType::Uuid => BiticalType::String,
            other => *other,
        }
    }
}

impl Bitical {
    /// duckdb から読み込んだ生の値を Bitical に変換します（読込境界）。
    /// 書込側の対応は `impl ToSql for Bitical`（types.rs）。
    /// Union は再帰的に解き、List は先頭要素を採用する保険的措置、
    /// それ以外の未対応 variant は debug 文字列にフォールバックします。
    /// SQL NULL は None として表現します。
    pub fn from_db_value(v: duckdb::types::Value) -> Option<Bitical> {
        use duckdb::types::Value;
        match v {
            Value::Union(inner) => Bitical::from_db_value(*inner),
            Value::Boolean(b) => Some(Bitical::Boolean(b)),
            Value::Int(i) => Some(Bitical::Integer(i as i64)),
            Value::BigInt(i) => Some(Bitical::Integer(i)),
            Value::HugeInt(i) => Some(Bitical::Integer(i as i64)),
            Value::Float(f) => Some(Bitical::Double(f as f64)),
            Value::Double(d) => Some(Bitical::Double(d)),
            Value::Text(s) => Some(Bitical::String(s)),
            Value::Null => None,
            Value::List(l) => {
                l.into_iter().next().and_then(Bitical::from_db_value)
            }
            other => Some(Bitical::String(format!("{:?}", other))),
        }
    }

    /// `from_db_value` のスカラー限定版。Union 再帰・List の先頭要素採用・
    /// debug文字列フォールバックは行わず、対応外の型は素直に `None` を返す。
    pub fn from_scalar_db_value(v: &duckdb::types::Value) -> Option<Bitical> {
        use duckdb::types::Value;
        match v {
            Value::Text(_)
            | Value::BigInt(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Double(_)
            | Value::Boolean(_) => Bitical::from_db_value(v.clone()),
            _ => None,
        }
    }

    pub fn to_simple_expr(&self) -> SimpleExpr {
        match self {
            Bitical::String(s) => Expr::val(s.clone()).into(),
            Bitical::Integer(i) => Expr::val(*i).into(),
            Bitical::Double(d) => Expr::val(*d).into(),
            Bitical::Boolean(b) => Expr::val(*b).into(),
            Bitical::Uuid(u) => Expr::val(u.to_string()).into(),
        }
    }

    pub fn to_col_expr(&self) -> (Col, SimpleExpr) {
        (self.name().to_column(), self.to_simple_expr())
    }

    /// 書込先カラムと、そのカラムへ保存する形へ収束した値のペアを返す。
    /// Uuid は label_uuid カラムが無いため LabelStr へ文字列として収束する
    /// （物理カラム経路ではネイティブ UUID のまま保存されるので、この収束は
    /// EAV ラベルカラム限定）。
    pub fn to_col_value(&self) -> (Col, Bitical) {
        let value = match self {
            Bitical::Uuid(u) => Bitical::String(u.to_string()),
            other => other.clone(),
        };
        (self.name().to_column(), value)
    }

    /// 列一致条件（GLOB/等価）を表す SQL 式を返す。`tag` が `SType::Label`
    /// （仮想ラベル列）の場合は、値の物理型に応じたカラム（LabelStr/LabelInt 等）
    /// へ読み替える（Boolean/Double は元々 tag に関わらず固定カラム、という
    /// 既存挙動を踏襲）。
    pub fn to_column_match_expr(&self, tag: Col) -> SimpleExpr {
        match self {
            Bitical::Integer(i) => {
                let t = if matches!(tag, Col::Label) {
                    Col::LabelInt
                } else {
                    tag
                };
                Expr::col(t).eq(*i)
            }
            Bitical::String(s) => {
                let t = if matches!(tag, Col::Label) {
                    Col::LabelStr
                } else {
                    tag
                };
                Expr::col(t)
                    .binary(BinOper::Custom("GLOB"), Expr::val(s.clone()))
            }
            Bitical::Boolean(b) => Expr::col(Col::LabelBool).eq(*b),
            Bitical::Double(d) => Expr::col(Col::LabelDouble).eq(*d),
            Bitical::Uuid(u) => {
                let t = if matches!(tag, Col::Label) {
                    Col::LabelStr
                } else {
                    tag
                };
                Expr::col(t).eq(u.to_string())
            }
        }
    }
}

/// 値として使用される定数文字列の識別子。
/// マジックストリングを排除するために使用します。
#[derive(
    Clone, Copy, Debug, strum::Display, strum::EnumString, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum Val {
    Builtin,
    User,
    File,
    Plugin,
    Note,
    ItemKind,
    Rank,
    Name,
    Unknown,
    Key,
    Value,
}

impl sea_query::Iden for Val {
    fn unquoted(&self, s: &mut dyn std::fmt::Write) {
        let val: &'static str = (*self).into();
        write!(s, "{}", val).unwrap();
    }
}

/// クエリ結果の動的カラム名を表す識別子。
/// データベーステーブルのカラムではなく、
/// SQL結果を整形するために使用される一時的な名前。
#[derive(
    Clone, Copy, Debug, strum::Display, strum::EnumString, strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum QueryResultCol {
    Tags,
}

impl sea_query::Iden for QueryResultCol {
    fn unquoted(&self, s: &mut dyn std::fmt::Write) {
        let val: &'static str = (*self).into();
        write!(s, "{}", val).unwrap();
    }
}

/// 共通で使用されるカラム名を表す識別子。
pub use crate::types::SType as Col;

impl sea_query::Iden for Col {
    fn unquoted(&self, s: &mut dyn std::fmt::Write) {
        let val: &'static str = (*self).into();
        write!(s, "{}", val).unwrap();
    }
}

impl Col {
    pub fn from_str(s: &str) -> Option<Self> {
        <Self as std::str::FromStr>::from_str(s).ok()
    }

    pub fn item_references_columns() -> [Self; 5] {
        [
            Self::ItemId,
            Self::Rank,
            Self::Name,
            Self::ItemKind,
            Self::Content,
        ]
    }

    pub fn tag_value_columns() -> Vec<Self> {
        std::iter::once(Self::Types)
            .chain(BiticalType::to_columns())
            .chain(std::iter::once(Self::Origin))
            .chain(std::iter::once(Self::TypedTag))
            .collect()
    }

    pub fn raw_tag_row_columns() -> [Self; 8] {
        [
            Self::ItemId,
            Self::ItemKind,
            Self::Type,
            Self::LabelStr,
            Self::LabelInt,
            Self::LabelDouble,
            Self::LabelBool,
            Self::Origin,
        ]
    }

    pub fn removed_file_columns() -> [(Col, Col); 5] {
        [
            (Col::RemovedFileAt, Col::RemovedFileAt),
            (Col::RemovedFilePath, Col::Path),
            (Col::RemovedFileSize, Col::Size),
            (Col::RemovedFileMtime, Col::Mtime),
            (Col::RemovedFileIsDir, Col::IsDir),
        ]
    }
}

/// DuckDB 固有の関数名を表す識別子。
#[derive(Iden, Clone, Copy)]
pub enum DuckDbFunc {
    ReadParquet,
    Coalesce,
    List,
    Concat,
    ParquetKvMetadata,
    StructPack,
    ListSlice,
    RowNumber,
    Count,
    AnyValue,
    ListValue,
    #[iden = "typeof"]
    TypeOf,
    StringAgg,
    StartsWith,
}

#[derive(Iden, Clone, Copy)]
pub enum DuckDbKeyword {
    #[iden = "DISTINCT ON"]
    DistinctOn,
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
            TargetTable::FileReferences => {
                create.col(SeaColumnDef::new(Col::ItemId).big_integer());
                create.col(SeaColumnDef::new(Col::Rank).big_integer());
                for c in columns
                    .iter()
                    .filter(|c| c.target_table == TargetTable::FileReferences)
                {
                    let iden = Col::from_str(&c.name)
                        .map(|c| c.into_iden())
                        .unwrap_or_else(|| crate::util::alias_from(&c.name));
                    let mut def = SeaColumnDef::new(iden);
                    c.bitical_type.prepare_column(&mut def);
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
                    c.bitical_type.prepare_column(&mut def);
                    create.col(&mut def);
                }
                create.col(SeaColumnDef::new(Col::ScanHash).big_integer());
                create.col(
                    SeaColumnDef::new(Col::BasenameScanHash).big_integer(),
                );
            }
            TargetTable::BaseTags
            | TargetTable::TagsByLocation
            | TargetTable::SystemTags
            | TargetTable::UserTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Type).string());
                for l_col in BiticalType::to_columns() {
                    let mut def = SeaColumnDef::new(l_col);
                    BiticalType::from_col(l_col).prepare_column(&mut def);
                    create.col(&mut def);
                }
            }
            TargetTable::ItemReferences => {
                for col in Col::item_references_columns() {
                    let mut def = SeaColumnDef::new(col);
                    BiticalType::from_col(col).prepare_column(&mut def);
                    create.col(&mut def);
                }
            }
            TargetTable::DataTypes => {
                create
                    .col(SeaColumnDef::new(Col::Type).string())
                    .col(SeaColumnDef::new(Col::DataType).integer());
            }
            TargetTable::RemovedFiles => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Rank).big_integer())
                    .col(
                        SeaColumnDef::new(Col::FileId)
                            .custom(BiticalType::Uuid),
                    )
                    .col(SeaColumnDef::new(Col::ScanHash).big_integer())
                    .col(SeaColumnDef::new(Col::BasenameScanHash).big_integer())
                    .col(SeaColumnDef::new(Col::Path).string())
                    .col(SeaColumnDef::new(Col::Size).big_integer())
                    .col(SeaColumnDef::new(Col::Mtime).big_integer())
                    .col(SeaColumnDef::new(Col::IsDir).boolean())
                    .col(SeaColumnDef::new(Col::RemovedFileAt).big_integer());
            }
        }
        create
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::{Expr, PostgresQueryBuilder, Query};

    #[test]
    fn test_store_open_disables_extension_autoload() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        for setting in
            ["autoinstall_known_extensions", "autoload_known_extensions"]
        {
            let value: String = store
                .conn
                .query_row(
                    &format!("SELECT current_setting('{setting}')::VARCHAR"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(value, "false", "{setting} は無効であるべき");
        }
    }

    #[test]
    fn test_bitical_type_repr_values() {
        assert_eq!(BiticalType::String as i32, 1);
        assert_eq!(BiticalType::Integer as i32, 2);
        assert_eq!(BiticalType::Double as i32, 3);
        assert_eq!(BiticalType::Boolean as i32, 4);
        assert_eq!(BiticalType::Uuid as i32, 5);
    }

    #[test]
    fn test_bitical_type_iden_spelling() {
        fn spell(t: BiticalType) -> String {
            let mut s = String::new();
            t.unquoted(&mut s);
            s
        }
        assert_eq!(spell(BiticalType::String), "VARCHAR");
        assert_eq!(spell(BiticalType::Integer), "BIGINT");
        assert_eq!(spell(BiticalType::Double), "DOUBLE");
        assert_eq!(spell(BiticalType::Boolean), "BOOLEAN");
        assert_eq!(spell(BiticalType::Uuid), "UUID");
    }

    #[test]
    fn test_bitical_type_prepare_column_ddl() {
        let cases = [
            (BiticalType::String, "varchar"),
            (BiticalType::Integer, "bigint"),
            (BiticalType::Double, "double"),
            (BiticalType::Boolean, "bool"),
            (BiticalType::Uuid, "uuid"),
        ];
        for (bt, expected_kw) in cases {
            let mut def = SeaColumnDef::new(Alias::new("c"));
            bt.prepare_column(&mut def);
            let sql = Table::create()
                .col(&mut def)
                .to_string(PostgresQueryBuilder)
                .to_lowercase();
            assert!(
                sql.contains(expected_kw),
                "{sql} should contain {expected_kw}"
            );
        }
    }

    #[test]
    fn test_bitical_from_db_value_scalars() {
        use duckdb::types::Value;
        assert_eq!(
            Bitical::from_db_value(Value::Boolean(true)),
            Some(Bitical::Boolean(true))
        );
        assert_eq!(
            Bitical::from_db_value(Value::Int(1)),
            Some(Bitical::Integer(1))
        );
        assert_eq!(
            Bitical::from_db_value(Value::BigInt(2)),
            Some(Bitical::Integer(2))
        );
        assert_eq!(
            Bitical::from_db_value(Value::HugeInt(3)),
            Some(Bitical::Integer(3))
        );
        assert_eq!(
            Bitical::from_db_value(Value::Float(1.5)),
            Some(Bitical::Double(1.5))
        );
        assert_eq!(
            Bitical::from_db_value(Value::Double(2.5)),
            Some(Bitical::Double(2.5))
        );
        assert_eq!(
            Bitical::from_db_value(Value::Text("a".to_string())),
            Some(Bitical::String("a".to_string()))
        );
        assert_eq!(Bitical::from_db_value(Value::Null), None);
    }

    #[test]
    fn test_bitical_from_db_value_union_and_list() {
        use duckdb::types::Value;
        assert_eq!(
            Bitical::from_db_value(Value::Union(Box::new(Value::BigInt(7)))),
            Some(Bitical::Integer(7))
        );
        assert_eq!(
            Bitical::from_db_value(Value::List(vec![
                Value::Text("x".to_string()),
                Value::Text("y".to_string()),
            ])),
            Some(Bitical::String("x".to_string()))
        );
        assert_eq!(Bitical::from_db_value(Value::List(vec![])), None);
    }

    #[test]
    fn test_bitical_from_db_value_fallback_debug_string() {
        use duckdb::types::Value;
        let v = Value::Blob(vec![1, 2, 3]);
        assert_eq!(
            Bitical::from_db_value(v.clone()),
            Some(Bitical::String(format!("{:?}", v)))
        );
    }

    #[test]
    fn test_to_col_value_routes_and_converges() {
        assert_eq!(
            Bitical::Integer(1).to_col_value(),
            (Col::LabelInt, Bitical::Integer(1))
        );
        assert_eq!(
            Bitical::String("a".into()).to_col_value(),
            (Col::LabelStr, Bitical::String("a".into()))
        );
        assert_eq!(
            Bitical::Double(1.5).to_col_value(),
            (Col::LabelDouble, Bitical::Double(1.5))
        );
        assert_eq!(
            Bitical::Boolean(true).to_col_value(),
            (Col::LabelBool, Bitical::Boolean(true))
        );
        // Uuid は label_uuid カラムが無いため LabelStr へ文字列として収束
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            Bitical::Uuid(id).to_col_value(),
            (Col::LabelStr, Bitical::String(id.to_string()))
        );
    }

    #[test]
    fn test_to_column_match_expr_caret_is_literal_not_prefix_glob() {
        let expr =
            Bitical::String("^foo".into()).to_column_match_expr(Col::Label);
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("'^foo'"),
            "value should stay literal '^foo': {}",
            sql
        );
        assert!(
            !sql.contains("'foo*'"),
            "must not convert to prefix glob 'foo*': {}",
            sql
        );
    }

    #[test]
    fn test_union_value_varchar() {
        let expr =
            CustomFunc::union_value(BiticalType::String, Expr::val("hello"));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("union_value(\"string\" :="),
            "should contain union_value(\"string\" :=: {}",
            sql
        );
        assert!(sql.contains("'hello'"), "should contain value: {}", sql);
    }

    #[test]
    fn test_union_value_boolean() {
        let expr =
            CustomFunc::union_value(BiticalType::Boolean, Expr::val(true));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("union_value(\"boolean\" :="),
            "should contain union_value(\"boolean\" :=: {}",
            sql
        );
    }

    #[test]
    fn test_union_value_bigint() {
        let expr =
            CustomFunc::union_value(BiticalType::Integer, Expr::val(42i64));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("union_value(\"integer\" :="),
            "should contain union_value(\"integer\" :=: {}",
            sql
        );
        assert!(sql.contains("42"), "should contain 42: {}", sql);
    }

    #[test]
    fn test_union_value_double() {
        let expr =
            CustomFunc::union_value(BiticalType::Double, Expr::val(3.14f64));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("union_value(\"double\" :="),
            "should contain union_value(\"double\" :=: {}",
            sql
        );
    }

    #[test]
    fn test_representative_union_type_spelling() {
        // アーム名は BiticalType の Display（予約語対策で引用符付き）、順序は宣言順
        assert_eq!(
            CustomFunc::representative_union_type(),
            "UNION(\"string\" VARCHAR, \"integer\" BIGINT, \
             \"double\" DOUBLE, \"boolean\" BOOLEAN, \"uuid\" UUID)"
        );
    }

    #[test]
    fn test_union_type_spelling() {
        // アーム名は BiticalType の Display（予約語対策で引用符付き）、
        // 順序は宣言順（Uuid は string に収束）
        assert_eq!(
            CustomFunc::union_type(),
            "UNION(\"string\" VARCHAR, \"integer\" BIGINT, \
             \"double\" DOUBLE, \"boolean\" BOOLEAN)"
        );
    }

    #[test]
    fn test_struct_pack_tag_generates_three_fields() {
        let expr = CustomFunc::struct_pack_tag(
            Expr::val("name").into(),
            CustomFunc::union_value(BiticalType::String, Expr::val("foo")),
            Expr::val("system").into(),
        );
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("struct_pack"),
            "should contain struct_pack: {}",
            sql
        );
        assert!(
            sql.contains("\"tag_type\""),
            "should contain tag_type field: {}",
            sql
        );
        assert!(
            sql.contains("\"value\""),
            "should contain value field: {}",
            sql
        );
        assert!(
            sql.contains("\"origin\""),
            "should contain origin field: {}",
            sql
        );
    }

    #[test]
    fn test_eav_union_value_generates_case_when() {
        let expr = CustomFunc::eav_union_value();
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(sql.contains("CASE"), "should have CASE: {}", sql);
        // CASE アームは走査順（BiticalType::to_columns_scan_order）で並ぶ
        let positions: Vec<usize> = BiticalType::to_columns_scan_order()
            .map(|c| {
                sql.find(&c.name()).unwrap_or_else(|| {
                    panic!("arm for {} not found: {sql}", c.name())
                })
            })
            .into();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "arms must follow scan order: {sql}"
        );
        assert!(
            sql.contains("union_value(\"integer\" :="),
            "should have int arm: {}",
            sql
        );
        assert!(
            sql.contains("union_value(\"string\" :="),
            "should have str arm: {}",
            sql
        );
        assert!(
            sql.contains("union_value(\"boolean\" :="),
            "should have bool arm: {}",
            sql
        );
        assert!(
            sql.contains("union_value(\"double\" :="),
            "should have double arm: {}",
            sql
        );
    }

    #[test]
    fn test_list_value_generates_function_call() {
        let expr = CustomFunc::list_value([
            Expr::val(1i64).into(),
            Expr::val(2i64).into(),
        ]);
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("list_value"),
            "should contain list_value: {}",
            sql
        );
    }

    #[test]
    fn test_count_star() {
        let sql = Query::select()
            .expr(CustomFunc::count_star())
            .to_string(PostgresQueryBuilder);
        assert!(sql.contains("count(*)"), "should contain count(*): {}", sql);
    }

    #[test]
    fn test_string_agg() {
        let expr =
            CustomFunc::string_agg(Expr::col(Col::LabelStr), Expr::val(","));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("string_agg"),
            "should contain string_agg: {}",
            sql
        );
    }

    #[test]
    fn test_try_cast_double() {
        let expr = CustomFunc::try_cast_double(Expr::col(Col::LabelInt));
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("TRY_CAST") && sql.contains("DOUBLE"),
            "should contain TRY_CAST ... DOUBLE: {}",
            sql
        );
    }

    #[test]
    fn test_count_over_multi() {
        use sea_query::{Alias, IntoIden};
        let cols = vec![
            Alias::new("key0").into_iden(),
            Alias::new("key1").into_iden(),
        ];
        let sql = Query::select()
            .expr(CustomFunc::count_over_multi(&cols))
            .to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("count(*)") && sql.contains("PARTITION BY"),
            "should contain count(*) OVER PARTITION BY: {}",
            sql
        );
        assert!(
            sql.contains("key0") && sql.contains("key1"),
            "should contain both keys: {}",
            sql
        );
    }

    #[test]
    fn test_row_number_over_multi() {
        use sea_query::{Alias, IntoIden, Order};
        let cols = vec![Alias::new("label_value").into_iden()];
        let order_bys = vec![(Col::ItemId, Order::Desc)];
        let sql = Query::select()
            .expr(CustomFunc::row_number_over_multi(&cols, order_bys))
            .to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("row_number()") && sql.contains("PARTITION BY"),
            "should contain row_number() OVER PARTITION BY: {}",
            sql
        );
        assert!(
            sql.contains("label_value"),
            "should contain partition key: {}",
            sql
        );
    }

    #[test]
    fn test_clear_index() {
        use std::fs;
        let temp_dir = tempfile::tempdir().unwrap();
        let db_dir = temp_dir.path().to_path_buf();

        // 削除対象のダミーファイルを作成
        let files_to_delete = [
            db_dir.join("file_references.parquet"),
            db_dir.join("locations.parquet"),
            db_dir.join("base_tags.parquet"),
            db_dir.join("tags_by_location.parquet"),
            db_dir.join("current_scan.parquet"),
            db_dir.join("live_ids.parquet"),
        ];
        // 削除対象のダミーディレクトリを作成
        let cache_dir = db_dir.join("cache");
        let cache_file = cache_dir.join("dummy_cache.parquet");

        // 残す対象のダミーファイルを作成
        let files_to_keep = [
            db_dir.join("user_tags.parquet"),
            db_dir.join("item_references.parquet"),
            db_dir.join("system_tags.parquet"),
        ];

        // 物理ファイル・ディレクトリ作成
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(&cache_file, b"cache").unwrap();
        for file in &files_to_delete {
            fs::write(file, b"test").unwrap();
        }
        for file in &files_to_keep {
            fs::write(file, b"keep").unwrap();
        }

        // Storeをオープン
        let store = Store::open(&db_dir).unwrap();

        // clear_index を呼び出し
        store.clear_index().unwrap();

        // 削除されたことを検証
        for file in &files_to_delete {
            assert!(!file.exists(), "File should be deleted: {:?}", file);
        }
        assert!(!cache_dir.exists(), "Cache directory should be deleted");

        // 残っていることを検証
        for file in &files_to_keep {
            assert!(file.exists(), "File should be kept: {:?}", file);
        }
    }

    #[test]
    fn test_tags_by_location_target_table() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = Store::open(temp_dir.path()).unwrap();
        let path = store.path_for_target(TargetTable::TagsByLocation);
        assert!(path.ends_with("tags_by_location.parquet"));
    }
}
