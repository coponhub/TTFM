use anyhow::{Result, Context};
use std::path::Path;
use std::time::UNIX_EPOCH;
use chrono::{DateTime, Local};
use sea_query::{Expr, SimpleExpr, Alias, extension::postgres::PgExpr};
use crate::types::{TypedTag, DBType, FileSize, FileTimestamp};
use crate::taggers::{Tagger, ColumnDef, TagValue, TargetTable};
use crate::db::{Tbl, Col};

/// 特定の TypedTag に関する**定義・検索・抽出の統合単位**。
/// 
/// 新しいタグ機能（例：Exif情報、Gitステータスなど）を追加する場合は、
/// このトレイトを実装した構造体を作成し、`FunctionRegistry` に登録します。
pub trait TagFunction: Send + Sync {
    /// この機能が保持する `Tagger`（抽出ロジック実行部）を取得します。
    fn tagger(&self) -> &dyn Tagger;

    /// 指定された `TypedTag` に対する検索SQL条件を生成します。
    /// 
    /// この機能が担当しないタグ（キーが一致しない等）の場合は `None` を返します。
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr>;

    /// このタグのスキャンにおける役割を返します。
    fn role(&self) -> ScanRole { ScanRole::Other }

    /// パスのみから値を生成できる場合、その値を返します。
    /// （移動処理などで、実際にファイルを開かずにタグを更新するために使用）
    fn generate_from_path(&self, _path: &Path) -> Option<TagValue> { None }
}

/// 型レベルでのタグ定義情報を保持するトレイト。
pub trait TagDefinition {
    /// タグ의 識別子名。
    const NAME: &'static str;
    /// スキャンにおける役割。
    const ROLE: ScanRole;
    /// 対応する Rust の型。
    type RustType: DBType + std::fmt::Debug + PartialEq + Clone;
    /// パスと（もしあれば）メタデータから値を生成します。
    fn generate(path: &Path, metadata: Option<&std::fs::Metadata>) -> Result<Self::RustType>;
}

/// `TagDefinition` に基づく値を保持するコンテナ。
pub struct Field<D: TagDefinition> {
    pub value: D::RustType,
}

impl<D: TagDefinition> std::fmt::Debug for Field<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

impl<D: TagDefinition> PartialEq for Field<D> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<D: TagDefinition> Clone for Field<D> {
    fn clone(&self) -> Self {
        Self { value: self.value.clone() }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ScanRole {
    Location,
    ScanId,
    Integrity,
    Other,
}

pub struct ScanColumn {
    pub name: &'static str,
    pub sql_type: &'static str,
    pub role: ScanRole,
}

crate::define_scan_entry! {
    path: PathFunction,
    inode: InodeFunction,
    size: SizeBytesFunction,
    mtime: ModifiedTsFunction,
}

// --- Utilities ---

/// SQLインジェクションを防ぐための簡易エスケープ処理。
/// 文字列内のシングルクォートを2つ重ねてエスケープします。
/// 特定のタグが `file_tags` テーブルに存在するかを確認する EXISTS 式を生成します。
///
/// # Arguments
/// * `tag_type` - タグの種類（例: "directory", "mimetype"）
/// * `tag_value` - 検索する値
/// * `exact` - true の場合は完全一致（=）、false の場合は部分一致（ILIKE）を使用します。
pub(crate) fn exists_in_tags(
    tag_type: &str,
    tag_value: &str,
    exact: bool,
) -> SimpleExpr {
    let mut query = sea_query::Query::select();
    query
        .expr(Expr::val(1))
        .from(Alias::new("all_tags"))
        .and_where(Expr::col(Col::EntityId).eq(Expr::col((Tbl::EntAlias, Col::Id))))
        .and_where(Expr::col(Col::TagType).eq(tag_type.to_string()));

    if exact {
        query.and_where(Expr::col(Col::TagValue).eq(tag_value.to_string()));
    } else {
        query.and_where(Expr::col(Col::TagValue).ilike(format!("%{}%", tag_value)));
    }

    Expr::exists(query.to_owned()).into()
}

// ========================================================
// 1. Path Function
// ========================================================

/// ファイルパス抽出ロジック。
struct PathTagger;

impl Tagger for PathTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: PathFunction::NAME.to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::Locations,
        }]
    }
    /// ファイルの絶対パスを抽出し、パスセパレータを正規化します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        // Windowsのバックスラッシュをスラッシュに正規化
        let p = path.to_string_lossy().replace('\\', "/");
        Ok(vec![TagValue::Text(p)])
    }
}

/// ファイルのフルパス（`path`）に関する機能。
///
/// # Examples
/// - Query: `path:documents` -> パスに "documents" を含むファイルを検索
pub struct PathFunction {
    tagger: PathTagger,
}

impl PathFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "path";
    /// 新しい `PathFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: PathTagger,
        }
    }
}

impl TagFunction for PathFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let expr = Expr::col((Tbl::LocAlias, Col::Path))
                .ilike(format!("%{}%", tag.tag.0));
            return Some(expr.into());
        }
        None
    }
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        let p = path.to_string_lossy().replace('\\', "/");
        Some(TagValue::Text(p))
    }
}

impl TagDefinition for PathFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        Ok(path.to_string_lossy().replace('\\', "/"))
    }
}

// ========================================================
// 2. ParentDir Function
// ========================================================

struct ParentDirTagger;

impl Tagger for ParentDirTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: ParentDirFunction::NAME.to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::Locations,
        }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        Ok(vec![TagValue::Text(parent)])
    }
}

/// 親ディレクトリパス（`parentdir`）に関する機能。
///
/// # Examples
/// - Query: `parentdir:src` -> 親ディレクトリが ".../src" または "src" であるファイルを検索
pub struct ParentDirFunction {
    tagger: ParentDirTagger,
}

impl ParentDirFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "parentdir";
    /// 新しい `ParentDirFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: ParentDirTagger,
        }
    }
}

impl TagFunction for ParentDirFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let val = &tag.tag.0;
            let expr = Expr::col((Tbl::LocAlias, Col::ParentDir))
                .ilike(format!("%/{}", val))
                .or(Expr::col((Tbl::LocAlias, Col::ParentDir)).eq(val.clone()));
            return Some(expr.into());
        }
        None
    }
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        Self::generate(path, None).ok().map(TagValue::Text)
    }
}

impl TagDefinition for ParentDirFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        Ok(parent)
    }
}

// ========================================================
// 3. Filename Function
// ========================================================

struct FilenameTagger;

impl Tagger for FilenameTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: FilenameFunction::NAME.to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::Locations,
        }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        Ok(vec![TagValue::Text(name)])
    }
}

/// ファイル名（`filename`）に関する機能。
///
/// # Examples
/// - Query: `filename:report` -> ファイル名に "report" を含むファイルを検索（ディレクトリ除外）
pub struct FilenameFunction {
    tagger: FilenameTagger,
}

impl FilenameFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "filename";
    /// 新しい `FilenameFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: FilenameTagger,
        }
    }
}

impl TagFunction for FilenameFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let val = &tag.tag.0;
            let expr = Expr::col((Tbl::LocAlias, Col::Filename))
                .ilike(format!("%{}%", val))
                .and(exists_in_tags(DirectoryFunction::NAME, "TRUE", true).not());
            return Some(expr.into());
        }
        None
    }
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        Self::generate(path, None).ok().map(TagValue::Text)
    }
}

impl TagDefinition for FilenameFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        Ok(name)
    }
}

// ========================================================
// 4. Stem Function
// ========================================================

struct StemTagger;

impl Tagger for StemTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: StemFunction::NAME.to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::FileTags,
        }]
    }
    /// 拡張子を除いたファイル名（ステム）を抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(vec![TagValue::Text(stem)])
    }
}

/// 拡張子を除いたファイル名（`stem`）に関する機能。
///
/// # Examples
/// - Query: `stem:image` -> 拡張子なし名に対する検索（現状はファイル名検索）
pub struct StemFunction {
    tagger: StemTagger,
}

impl StemFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "stem";
    /// 新しい `StemFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: StemTagger,
        }
    }
}

impl TagFunction for StemFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let val = &tag.tag.0;
            let expr = Expr::col((Tbl::LocAlias, Col::Filename))
                .ilike(format!("%{}%", val))
                .and(exists_in_tags(DirectoryFunction::NAME, "TRUE", true).not());
            return Some(expr.into());
        }
        None
    }
}

impl TagDefinition for StemFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(stem)
    }
}

// ========================================================
// 5. Extension Function
// ========================================================

struct ExtensionTagger;

impl Tagger for ExtensionTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: ExtensionFunction::NAME.to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::Locations,
        }]
    }
    /// ファイルの拡張子を抽出し、小文字化します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_string().to_lowercase())
            .unwrap_or_default();
        Ok(vec![TagValue::Text(ext)])
    }
}

/// 拡張子（`extension`）に関する機能。
///
/// # Examples
/// - Query: `extension:rs` または `ext:rs` -> 拡張子が "rs" のファイルを検索
pub struct ExtensionFunction {
    tagger: ExtensionTagger,
}

impl ExtensionFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "extension";
    /// 新しい `ExtensionFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: ExtensionTagger,
        }
    }
}

impl TagFunction for ExtensionFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let expr =
                Expr::col((Tbl::LocAlias, Col::Extension)).eq(tag.tag.0.clone());
            return Some(expr.into());
        }
        None
    }
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        Self::generate(path, None).ok().map(TagValue::Text)
    }
}

impl TagDefinition for ExtensionFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_string().to_lowercase())
            .unwrap_or_default();
        Ok(ext)
    }
}

// ========================================================
// 6. Directory Function (Logic)
// ========================================================

struct DirectoryTagger;

impl Tagger for DirectoryTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: DirectoryFunction::NAME.to_string(),
            sql_type: "BOOLEAN",
            target_table: TargetTable::FileTags,
        }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Boolean(path.is_dir())])
    }
}

/// ディレクトリ判定（`directory`）に関する機能。
///
/// # Examples
/// - Query: `directory:src` -> 名前に "src" を含むディレクトリを検索
pub struct DirectoryFunction {
    tagger: DirectoryTagger,
}

impl DirectoryFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "directory";
    /// 新しい `DirectoryFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: DirectoryTagger,
        }
    }
}

impl TagFunction for DirectoryFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let val = &tag.tag.0;
            let expr = Expr::col((Tbl::LocAlias, Col::Filename))
                .ilike(format!("%{}%", val))
                .and(exists_in_tags(Self::NAME, "TRUE", true));
            return Some(expr.into());
        }
        None
    }
}

impl TagDefinition for DirectoryFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = bool;
    fn generate(
        path: &Path,
        _metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        Ok(path.is_dir())
    }
}

// ========================================================
// 7. Size Bytes Function
// ========================================================

struct SizeBytesTagger;

impl Tagger for SizeBytesTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: SizeBytesFunction::NAME.to_string(),
            sql_type: "BIGINT",
            target_table: TargetTable::FileEntities,
        }]
    }
    /// ファイルサイズ（バイト数）を抽出します。ディレクトリの場合は0とします。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let size = if path.is_dir() {
            0
        } else {
            match std::fs::metadata(path) {
                Ok(m) => m.len(),
                Err(e) => {
                    eprintln!("Warning: Failed to get metadata for size {:?}: {}", path, e);
                    0
                }
            }
        };
        Ok(vec![TagValue::BigInt(size as i64)])
    }
}

/// ファイルサイズ（バイト単位、`size_bytes`）に関する機能。
///
/// # Examples
/// - Query: `size_bytes:1024` -> サイズがちょうど1024バイトのファイルを検索
pub struct SizeBytesFunction {
    tagger: SizeBytesTagger,
}

impl SizeBytesFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "size";
    /// 新しい `SizeBytesFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: SizeBytesTagger,
        }
    }
}

impl TagFunction for SizeBytesFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let expr = Expr::col((Tbl::EntAlias, Col::Size)).eq(tag.tag.0.clone());
            return Some(expr.into());
        }
        None
    }
    fn role(&self) -> ScanRole {
        ScanRole::Integrity
    }
}

impl TagDefinition for SizeBytesFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Integrity;
    type RustType = FileSize;
    fn generate(
        path: &Path,
        metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let size = if path.is_dir() {
            0
        } else if let Some(m) = metadata {
            m.len()
        } else {
            std::fs::metadata(path)
                .context("Failed to get metadata for size")?
                .len()
        };
        Ok(FileSize(size as i64))
    }
}

// ========================================================
// 8. Modified TS Function
// ========================================================

struct ModifiedTsTagger;

impl Tagger for ModifiedTsTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: ModifiedTsFunction::NAME.to_string(),
            sql_type: "BIGINT",
            target_table: TargetTable::FileEntities,
        }]
    }
    /// 最終更新日時のUNIXタイムスタンプを抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let ts = match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64,
            Err(e) => {
                // ファイルが見つからない等のエラーはスキャン中に発生しうる
                eprintln!("Warning: Failed to get mtime for {:?}: {}", path, e);
                0
            }
        };
        Ok(vec![TagValue::BigInt(ts)])
    }
}

/// 更新日時（UNIXタイムスタンプ、`modified_ts`）に関する機能。
///
/// # Examples
/// - Query: `modified_ts:1700000000` -> 指定のタイムスタンプを持つファイルを検索
pub struct ModifiedTsFunction {
    tagger: ModifiedTsTagger,
}

impl ModifiedTsFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "mtime";
    /// 新しい `ModifiedTsFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: ModifiedTsTagger,
        }
    }
}

impl TagFunction for ModifiedTsFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let expr =
                Expr::col((Tbl::EntAlias, Col::Mtime)).eq(tag.tag.0.clone());
            return Some(expr.into());
        }
        None
    }
    fn role(&self) -> ScanRole {
        ScanRole::Integrity
    }
}

impl TagDefinition for ModifiedTsFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Integrity;
    type RustType = FileTimestamp;
    fn generate(
        path: &Path,
        metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let ts_res = if let Some(m) = metadata {
            m.modified()
        } else {
            std::fs::metadata(path).and_then(|m| m.modified())
        };

        let ts = ts_res.with_context(|| {
            format!("Failed to get mtime for {:?}", path)
        })?;
        let secs = ts
            .duration_since(UNIX_EPOCH)
            .context("Time went backwards")?
            .as_secs() as i64;
        Ok(FileTimestamp(secs))
    }
}

// ========================================================
// 9. Inode Function
// ========================================================

struct InodeTagger;

impl Tagger for InodeTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: InodeFunction::NAME.to_string(),
            sql_type: "VARCHAR",
            target_table: TargetTable::FileEntities,
        }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let inode = crate::get_inode_string(path);
        Ok(vec![TagValue::Text(inode)])
    }
}

/// ファイル識別子（`inode`）に関する機能。
pub struct InodeFunction {
    tagger: InodeTagger,
}

impl InodeFunction {
    pub const NAME: &'static str = "inode";
    pub fn new() -> Self {
        Self {
            tagger: InodeTagger,
        }
    }
}

impl TagFunction for InodeFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            let expr =
                Expr::col((Tbl::EntAlias, Col::Inode)).eq(tag.tag.0.clone());
            return Some(expr.into());
        }
        None
    }
    fn role(&self) -> ScanRole {
        ScanRole::ScanId
    }
}

impl TagDefinition for InodeFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::ScanId;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        Ok(crate::get_inode_string(path))
    }
}

// ========================================================
// 10. Type From Ext Function
// ========================================================

struct TypeFromExtTagger;

impl Tagger for TypeFromExtTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: TypeFromExtFunction::NAME.to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::FileTags,
        }]
    }
    /// ファイルの種類（"Folder", "XXX File"など）を判定して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Text(TypeFromExtFunction::generate(path, None)?)])
    }
}

impl TagDefinition for TypeFromExtFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let is_dir = path.is_dir();
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_string().to_lowercase())
            .unwrap_or_default();
        Ok(if is_dir {
            "Folder".to_string()
        } else if ext.is_empty() {
            "File".to_string()
        } else {
            format!("{} File", ext.to_uppercase())
        })
    }
}

/// ファイルの種類（`type_from_ext`）に関する機能。
///
/// # Examples
/// - Query: `type_from_ext:Folder` -> ディレクトリを検索
/// - Query: `type_from_ext:PDF` -> PDFファイルを検索
pub struct TypeFromExtFunction {
    tagger: TypeFromExtTagger,
}

impl TypeFromExtFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "type_from_ext";
    /// 新しい `TypeFromExtFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: TypeFromExtTagger,
        }
    }
}

impl TagFunction for TypeFromExtFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            return Some(exists_in_tags(Self::NAME, &tag.tag.0, false));
        }
        None
    }
}

// ========================================================
// 10. Size Str Function
// ========================================================

struct SizeStrTagger;

impl SizeStrTagger {
    /// バイト数を読みやすい文字列（KB, MBなど）に変換するヘルパー。
    fn format_size(bytes: u64) -> String {
        const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
        let mut size = bytes as f64;
        let mut unit_index = 0;
        while size >= 1024.0 && unit_index < UNITS.len() - 1 {
            size /= 1024.0;
            unit_index += 1;
        }
        format!("{:.1} {}", size, UNITS[unit_index])
    }
}

impl Tagger for SizeStrTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: SizeStrFunction::NAME.to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::FileTags,
        }]
    }
    /// ファイルサイズを読みやすい文字列（例: "1.5 MB"）に変換して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Text(SizeStrFunction::generate(path, None)?)])
    }
}

impl TagDefinition for SizeStrFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = String;
    fn generate(
        path: &Path,
        metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let size = if path.is_dir() {
            0
        } else if let Some(m) = metadata {
            m.len()
        } else {
            std::fs::metadata(path)
                .context("Failed to get metadata for size_str")?
                .len()
        };

        Ok(if path.is_dir() {
            "-".to_string()
        } else {
            SizeStrTagger::format_size(size)
        })
    }
}

/// ファイルサイズ文字列表現（`size_str`）に関する機能。
///
/// # Examples
/// - Query: `size_str:KB` -> キロバイト単位のファイルを検索
pub struct SizeStrFunction {
    tagger: SizeStrTagger,
}

impl SizeStrFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "size_str";
    /// 新しい `SizeStrFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: SizeStrTagger,
        }
    }
}

impl TagFunction for SizeStrFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            return Some(exists_in_tags(Self::NAME, &tag.tag.0, false));
        }
        None
    }
}

// ========================================================
// 11. Modified Str Function
// ========================================================

struct ModifiedStrTagger;

impl ModifiedStrTagger {
    /// システム時間を "YYYY-MM-DD HH:MM" 形式に変換するヘルパー。
    fn format_time(time: std::time::SystemTime) -> String {
        let datetime: DateTime<Local> = time.into();
        datetime.format("%Y-%m-%d %H:%M").to_string()
    }
}

impl Tagger for ModifiedStrTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: ModifiedStrFunction::NAME.to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::FileTags,
        }]
    }
    /// 最終更新日時を読みやすい文字列（例: "2024-01-01 12:00"）に変換して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let val = ModifiedStrFunction::generate(path, None)?;
        Ok(vec![TagValue::Text(val)])
    }
}

impl TagDefinition for ModifiedStrFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = String;
    fn generate(
        path: &Path,
        metadata: Option<&std::fs::Metadata>,
    ) -> Result<Self::RustType> {
        let ts_res = if let Some(m) = metadata {
            m.modified()
        } else {
            std::fs::metadata(path).and_then(|m| m.modified())
        };

        Ok(ts_res.map(ModifiedStrTagger::format_time).unwrap_or_default())
    }
}

/// 更新日時文字列表現（`modified_str`）に関する機能。
///
/// # Examples
/// - Query: `modified_str:2024` -> 2024年に更新されたファイルを検索
pub struct ModifiedStrFunction {
    tagger: ModifiedStrTagger,
}

impl ModifiedStrFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "modified_str";
    /// 新しい `ModifiedStrFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: ModifiedStrTagger,
        }
    }
}

impl TagFunction for ModifiedStrFunction {
    fn tagger(&self) -> &dyn Tagger {
        &self.tagger
    }
    fn to_expr(&self, tag: &TypedTag) -> Option<SimpleExpr> {
        if tag.tagtype.0 == Self::NAME {
            return Some(exists_in_tags(Self::NAME, &tag.tag.0, false));
        }
        None
    }
}

// ========================================================
// Unit Tests
// ========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{TypedTag, TagType, Tag};
    use sea_query::{Query, PostgresQueryBuilder};

    // Helper to stringify a SimpleExpr for testing
    fn to_sql(expr: SimpleExpr) -> String {
        let sql = Query::select()
            .expr(expr)
            .to_string(PostgresQueryBuilder);
        // "SELECT ..." -> remove "SELECT "
        sql.strip_prefix("SELECT ").unwrap_or(&sql).to_string()
    }

    // Helper to create a TypedTag
    fn ttag(key: &str, value: &str) -> TypedTag {
        TypedTag {
            tagtype: TagType(key.to_string()),
            tag: Tag(value.to_string()),
        }
    }

    #[test]
    fn test_path_function() {
        let f = PathFunction::new();
        let expr = f.to_expr(&ttag(PathFunction::NAME, "foo")).unwrap();
        let sql = to_sql(expr);
        assert_eq!(sql, "\"l\".\"path\" ILIKE '%foo%'" );
    }

    #[test]
    fn test_filename_function() {
        let f = FilenameFunction::new();
        let expr = f.to_expr(&ttag(FilenameFunction::NAME, "report")).unwrap();
        let sql = to_sql(expr);
        assert!(sql.contains("\"l\".\"filename\" ILIKE '%report%'" ));
        assert!(sql.contains("NOT EXISTS"));
        assert!(sql.contains("\"tag_type\" = 'directory'"));
    }

    #[test]
    fn test_extension_function() {
        let f = ExtensionFunction::new();
        let expr = f.to_expr(&ttag(ExtensionFunction::NAME, "rs")).unwrap();
        let sql = to_sql(expr);
        assert_eq!(sql, "\"l\".\"extension\" = 'rs'");
    }

    #[test]
    fn test_size_bytes_function() {
        let f = SizeBytesFunction::new();
        let expr = f.to_expr(&ttag(SizeBytesFunction::NAME, "123")).unwrap();
        let sql = to_sql(expr);
        assert_eq!(sql, "\"e\".\"size\" = '123'");
        assert_eq!(f.role(), ScanRole::Integrity);
    }

    #[test]
    fn test_inode_function() {
        let f = InodeFunction::new();
        assert_eq!(f.role(), ScanRole::ScanId);
        assert_eq!(f.tagger().get_columns()[0].name, "inode");
    }
}