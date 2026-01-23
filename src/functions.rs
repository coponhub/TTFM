use crate::db::{SqlType, TargetTable};
use crate::taggers::{ColumnDef, TagValue, Tagger};
use crate::types::{
    DBType, FileSize, FileTimestamp, Name, SType, StaticName, METADATA_ERROR,
};
use crate::util::SafeMetadata;
use anyhow::Result;
use chrono::Local;
use path_slash::PathExt;
use std::path::Path;

/// 特定の TypedTag に関する**定義・抽出の統合単位**。
///
/// 新しいタグ機能（例：Exif情報、Gitステータスなど）を追加する場合は、
/// このトレイトを実装した構造体を作成し、`FunctionRegistry` に登録します。
pub trait IndexingFunction: Send + Sync {
    /// この機能の識別子名を取得します。
    fn name(&self) -> Name;

    /// この機能が保持する `Tagger`（抽出ロジック実行部）を取得します。
    fn tagger(&self) -> Option<&dyn Tagger> {
        None
    }

    /// このタグのスキャンにおける役割を返します。
    fn role(&self) -> ScanRole {
        ScanRole::Other
    }

    /// パスのみから値を生成できる場合, その値を返します。
    /// （移動処理などで, 実際にファイルを開かずにタグを更新するために使用）
    fn generate_from_path(&self, _path: &Path) -> Option<TagValue> {
        None
    }

    /// このタグのデフォルトのランク値（優先度）を返します。
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::DEFAULT
    }
}

/// 型レベルでのタグ定義情報を保持するトレイト。
pub trait TagDefinition {
    /// タグの 識別子名。
    fn name() -> StaticName;
    /// スキャンにおける役割。
    const ROLE: ScanRole;
    /// 対応する Rust の型。
    type RustType: DBType + std::fmt::Debug + PartialEq + Clone;
    /// パスとメタデータから値を生成します。
    fn generate(path: &Path, metadata: &SafeMetadata)
        -> Result<Self::RustType>;
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
        Self {
            value: self.value.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ScanRole {
    Location,
    ScanId,
    Integrity,
    Other,
    /// インデックス作成時の抽出対象外（定義とランクのみ提供）
    DefinitionOnly,
}

pub struct ScanColumn {
    pub name: &'static str,
    pub sql_type: SqlType,
    pub role: ScanRole,
}

crate::define_scan_entry! {
    path: PathFunction,
    inode: InodeFunction,
    size: SizeBytesFunction,
    mtime: ModifiedTsFunction,
}

// --- Utilities ---

/// 指定されたファイルパスのメタデータを取得します。

// ========================================================
// 1. Path Function
// ========================================================

/// ファイルパス抽出ロジック。
struct PathTagger;

impl Tagger for PathTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: PathFunction::NAME.to_string(),
            sql_type: SqlType::VARCHAR,
            target_table: TargetTable::Locations,
        }]
    }
    /// ファイルの絶対パスを抽出し、パスセパレータを正規化します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        // Windowsのバックスラッシュをスラッシュに正規化
        let p = path.to_slash_lossy().to_string();
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
        Self { tagger: PathTagger }
    }
}

impl IndexingFunction for PathFunction {
    fn name(&self) -> Name {
        SType::Path.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        let p = path.to_slash_lossy().to_string();
        Some(TagValue::Text(p))
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::PATH
    }
}

impl TagDefinition for PathFunction {
    fn name() -> StaticName {
        SType::Path.into()
    }
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        Ok(path.to_slash_lossy().to_string())
    }
}

// ========================================================
// 2. ParentDir Function
// ========================================================

struct ParentDirTagger;

impl Tagger for ParentDirTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: <ParentDirFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::VARCHAR,
            target_table: TargetTable::Locations,
        }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let parent = path
            .parent()
            .map(|p| p.to_slash_lossy().to_string())
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
    /// 新しい `ParentDirFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: ParentDirTagger,
        }
    }
}

impl IndexingFunction for ParentDirFunction {
    fn name(&self) -> Name {
        SType::Parentdir.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        path.parent()
            .map(|p| TagValue::Text(p.to_slash_lossy().to_string()))
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::PARENT_DIR
    }
}

impl TagDefinition for ParentDirFunction {
    fn name() -> StaticName {
        SType::Parentdir.into()
    }
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        let parent = path
            .parent()
            .map(|p| p.to_slash_lossy().to_string())
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
            name: <FilenameFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::VARCHAR,
            target_table: TargetTable::Locations,
        }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let m = match std::fs::metadata(path) {
            Ok(real_m) => SafeMetadata::new(&real_m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(e.into())
            }
            Err(_) => SafeMetadata::recovered(),
        };
        let name = FilenameFunction::generate(path, &m)?; 
        Ok(vec![TagValue::Text(name)])
    }
}

/// ファイル名（`filename`）に関する機能。
///
/// # Examples
/// - Query: `filename:report` -> 名前に "report" を含むファイルを検索
pub struct FilenameFunction {
    tagger: FilenameTagger,
}

impl FilenameFunction {
    /// 新しい `FilenameFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: FilenameTagger,
        }
    }
}

impl IndexingFunction for FilenameFunction {
    fn name(&self) -> Name {
        SType::Filename.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        Self::generate(path, &SafeMetadata::recovered())
            .ok()
            .map(TagValue::Text)
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::FILENAME
    }
}

impl TagDefinition for FilenameFunction {
    fn name() -> StaticName {
        SType::Filename.into()
    }
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: &SafeMetadata,
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
            name: <StemFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::VARCHAR,
            target_table: TargetTable::BaseTags,
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
    /// 新しい `StemFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self { tagger: StemTagger }
    }
}

impl IndexingFunction for StemFunction {
    fn name(&self) -> Name {
        SType::Stem.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
}

impl TagDefinition for StemFunction {
    fn name() -> StaticName {
        SType::Stem.into()
    }
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: &SafeMetadata,
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
            name: <ExtensionFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::VARCHAR,
            target_table: TargetTable::Locations,
        }]
    }
    /// ファイルの拡張子を抽出し、小文字化します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let ext = path
            .extension()
            .map(|e| {
                let s = e.to_string_lossy().to_string().to_lowercase();
                TagValue::Text(s)
            })
            .unwrap_or(TagValue::Null);
        Ok(vec![ext])
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
    /// 新しい `ExtensionFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: ExtensionTagger,
        }
    }
}

impl IndexingFunction for ExtensionFunction {
    fn name(&self) -> Name {
        SType::Extension.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        path.extension().map(|e| {
            TagValue::Text(e.to_string_lossy().to_string().to_lowercase())
        })
    }
}

impl TagDefinition for ExtensionFunction {
    fn name() -> StaticName {
        SType::Extension.into()
    }
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(
        path: &Path,
        _metadata: &SafeMetadata,
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
            name: <DirectoryFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::BOOLEAN,
            target_table: TargetTable::BaseTags,
        }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Boolean(path.is_dir())])
    }
}

/// ディレクトリ判定（`is_dir`）に関する機能。
///
/// # Examples
/// - Query: `is_dir:true` -> ディレクトリを検索
pub struct DirectoryFunction {
    tagger: DirectoryTagger,
}

impl DirectoryFunction {
    /// 新しい `DirectoryFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: DirectoryTagger,
        }
    }
}

impl IndexingFunction for DirectoryFunction {
    fn name(&self) -> Name {
        SType::IsDir.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
}

impl TagDefinition for DirectoryFunction {
    fn name() -> StaticName {
        SType::IsDir.into()
    }
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = bool;
    fn generate(
        _path: &Path,
        metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        Ok(metadata.is_dir())
    }
}

// ========================================================
// 7. Size Bytes Function
// ========================================================

struct SizeBytesTagger;

impl Tagger for SizeBytesTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: <SizeBytesFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::BIGINT,
            target_table: TargetTable::Locations,
        }]
    }
    /// ファイルサイズ（バイト数）を抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let m = match std::fs::metadata(path) {
            Ok(real_m) => SafeMetadata::new(&real_m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(e.into())
            }
            Err(_) => SafeMetadata::recovered(),
        };
        let size = SizeBytesFunction::generate(path, &m)?;
        Ok(vec![TagValue::BigInt(size.0)])
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
    /// 新しい `SizeBytesFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: SizeBytesTagger,
        }
    }
}

impl IndexingFunction for SizeBytesFunction {
    fn name(&self) -> Name {
        SType::Size.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn role(&self) -> ScanRole {
        ScanRole::Integrity
    }
}

impl TagDefinition for SizeBytesFunction {
    fn name() -> StaticName {
        SType::Size.into()
    }
    const ROLE: ScanRole = ScanRole::Integrity;
    type RustType = FileSize;
    fn generate(
        _path: &Path,
        metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        Ok(FileSize(metadata.len()))
    }
}

// ========================================================
// 8. Modified TS Function
// ========================================================

struct ModifiedTsTagger;

impl Tagger for ModifiedTsTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: <ModifiedTsFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::BIGINT,
            target_table: TargetTable::Locations,
        }]
    }
    /// 最終更新日時のUNIXタイムスタンプを抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let m = match std::fs::metadata(path) {
            Ok(real_m) => SafeMetadata::new(&real_m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(e.into())
            }
            Err(_) => SafeMetadata::recovered(),
        };
        let ts = ModifiedTsFunction::generate(path, &m)?;
        Ok(vec![TagValue::BigInt(ts.0)])
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
    /// 新しい `ModifiedTsFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: ModifiedTsTagger,
        }
    }
}

impl IndexingFunction for ModifiedTsFunction {
    fn name(&self) -> Name {
        SType::Mtime.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn role(&self) -> ScanRole {
        ScanRole::Integrity
    }
}

impl TagDefinition for ModifiedTsFunction {
    fn name() -> StaticName {
        SType::Mtime.into()
    }
    const ROLE: ScanRole = ScanRole::Integrity;
    type RustType = FileTimestamp;
    fn generate(
        _path: &Path,
        metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        Ok(FileTimestamp(metadata.modified()))
    }
}

// ========================================================
// 9. Inode Function
// ========================================================

struct InodeTagger;

impl Tagger for InodeTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: <InodeFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::UUID,
            target_table: TargetTable::FileReferences,
        }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let file_ref = crate::get_file_ref(path)?;
        Ok(vec![TagValue::Uuid(file_ref)])
    }
}

/// ファイル識別子（`inode`）に関する機能。
pub struct InodeFunction {
    tagger: InodeTagger,
}

impl InodeFunction {
    pub fn new() -> Self {
        Self {
            tagger: InodeTagger,
        }
    }
}

impl IndexingFunction for InodeFunction {
    fn name(&self) -> Name {
        SType::FileId.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn role(&self) -> ScanRole {
        ScanRole::ScanId
    }
}

impl TagDefinition for InodeFunction {
    fn name() -> StaticName {
        SType::FileId.into()
    }
    const ROLE: ScanRole = ScanRole::ScanId;
    type RustType = crate::types::FileRef;
    fn generate(
        path: &Path,
        _metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        crate::get_file_ref(path)
    }
}

// ========================================================
// 11. Type From Ext Function
// ========================================================

struct TypeFromExtTagger;

impl Tagger for TypeFromExtTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: <TypeFromExtFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::VARCHAR,
            target_table: TargetTable::BaseTags,
        }]
    }
    /// ファイルの種類（"Folder", "XXX File"など）を判定して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let m = match std::fs::metadata(path) {
            Ok(real_m) => SafeMetadata::new(&real_m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(e.into())
            }
            Err(_) => SafeMetadata::recovered(),
        };
        Ok(vec![TagValue::Text(TypeFromExtFunction::generate(
            path, &m,
        )?)])
    }
}

impl TagDefinition for TypeFromExtFunction {
    fn name() -> StaticName {
        SType::TypeFromExt.into()
    }
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = String;
    fn generate(
        path: &Path,
        metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        if metadata.is_dir() {
            return Ok("Folder".to_string());
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .unwrap_or_else(|| "File".to_string());
        Ok(ext)
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
    /// 新しい `TypeFromExtFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: TypeFromExtTagger,
        }
    }
}

impl IndexingFunction for TypeFromExtFunction {
    fn name(&self) -> Name {
        SType::TypeFromExt.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::TYPE_FROM_EXT
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
            name: <SizeStrFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::VARCHAR,
            target_table: TargetTable::BaseTags,
        }]
    }
    /// ファイルサイズを読みやすい文字列（例: "1.5 MB"）に変換して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let m = match std::fs::metadata(path) {
            Ok(real_m) => SafeMetadata::new(&real_m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(e.into())
            }
            Err(_) => SafeMetadata::recovered(),
        };
        Ok(vec![TagValue::Text(SizeStrFunction::generate(path, &m)?)])
    }
}

impl TagDefinition for SizeStrFunction {
    fn name() -> StaticName {
        SType::SizeStr.into()
    }
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = String;
    fn generate(
        _path: &Path,
        metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        Ok(if metadata.len() == METADATA_ERROR {
            "-".to_string()
        } else {
            SizeStrTagger::format_size(metadata.len() as u64)
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
    /// 新しい `SizeStrFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: SizeStrTagger,
        }
    }
}

impl IndexingFunction for SizeStrFunction {
    fn name(&self) -> Name {
        SType::SizeStr.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::SIZE_STR
    }
}

// ========================================================
// 11. Modified Str Function
// ========================================================

struct ModifiedStrTagger;

impl Tagger for ModifiedStrTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: <ModifiedStrFunction as TagDefinition>::name().to_string(),
            sql_type: SqlType::VARCHAR,
            target_table: TargetTable::BaseTags,
        }]
    }
    /// 最終更新日時を読みやすい文字列（例: "2024-01-01 12:00"）に変換して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let m = match std::fs::metadata(path) {
            Ok(real_m) => SafeMetadata::new(&real_m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(e.into())
            }
            Err(_) => SafeMetadata::recovered(),
        };
        let val = ModifiedStrFunction::generate(path, &m)?;
        Ok(vec![TagValue::Text(val)])
    }
}

impl TagDefinition for ModifiedStrFunction {
    fn name() -> StaticName {
        SType::ModifiedStr.into()
    }
    const ROLE: ScanRole = ScanRole::Other;
    type RustType = String;
    fn generate(
        _path: &Path,
        metadata: &SafeMetadata,
    ) -> Result<Self::RustType> {
        Ok(if metadata.modified() == METADATA_ERROR {
            "-".to_string()
        } else {
            let datetime: chrono::DateTime<Local> = (std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(metadata.modified() as u64))
            .into();
            datetime.format("%Y-%m-%d %H:%M").to_string()
        })
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
    /// 新しい `ModifiedStrFunction` インスタンスを作成します。
    pub fn new() -> Self {
        Self {
            tagger: ModifiedStrTagger,
        }
    }
}

impl IndexingFunction for ModifiedStrFunction {
    fn name(&self) -> Name {
        SType::ModifiedStr.into()
    }
    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(&self.tagger)
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::MODIFIED_STR
    }
}

// ========================================================
// 12. Definition Only Functions
// ========================================================

pub struct NameIndexingFunction;
impl IndexingFunction for NameIndexingFunction {
    fn name(&self) -> Name {
        SType::Name.into()
    }
    fn role(&self) -> ScanRole {
        ScanRole::DefinitionOnly
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::NAME
    }
}

pub struct KindIndexingFunction;
impl IndexingFunction for KindIndexingFunction {
    fn name(&self) -> Name {
        SType::ItemKind.into()
    }
    fn role(&self) -> ScanRole {
        ScanRole::DefinitionOnly
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::ITEM_KIND
    }
}

pub struct ContentIndexingFunction;
impl IndexingFunction for ContentIndexingFunction {
    fn name(&self) -> Name {
        SType::Content.into()
    }
    fn role(&self) -> ScanRole {
        ScanRole::DefinitionOnly
    }
    fn default_rank(&self) -> crate::types::Rank {
        crate::rank::SystemRank::CONTENT
    }
}

// ========================================================
// Unit Tests
// ========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Col;
    use sea_query::{PostgresQueryBuilder, Query};

    // Helper functions for tests...

    #[test]
    fn test_extension_tagger_logic() {
        let tagger = ExtensionTagger;

        // 1. 拡張子がある場合
        let path = Path::new("test.rs");
        let values = tagger.tag_file(path).unwrap();
        assert_eq!(values[0], TagValue::Text("rs".to_string()));

        // 2. 拡張子がない場合 (今回の修正ポイント)
        let path_no_ext = Path::new("no_extension");
        let values = tagger.tag_file(path_no_ext).unwrap();
        assert_eq!(values[0], TagValue::Null);
    }

    #[test]
    fn test_size_bytes_function() {
        let f = SizeBytesFunction::new();
        assert_eq!(f.role(), ScanRole::Integrity);
    }

    #[test]
    fn test_inode_function() {
        let f = InodeFunction::new();
        assert_eq!(f.role(), ScanRole::ScanId);
        assert_eq!(f.tagger().unwrap().get_columns()[0].name, "file_id");
    }

    #[test]
    fn test_col_file_id_direct() {
        let mut query = Query::select();
        query.column(Col::FileId);
        let sql = query.to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("\"file_id\""),
            "Direct Col::FileId should be snake_case"
        );
    }

    #[test]
    fn test_metadata_generate_error_handling() {
        // SafeMetadata::recovered() を使った時のエラーハンドリング挙動を確認
        let safe_m = SafeMetadata::recovered();
        let path = Path::new("dummy");

        assert_eq!(
            SizeBytesFunction::generate(path, &safe_m).unwrap().0,
            METADATA_ERROR
        );
        assert_eq!(
            ModifiedTsFunction::generate(path, &safe_m).unwrap().0,
            METADATA_ERROR
        );
        assert_eq!(SizeStrFunction::generate(path, &safe_m).unwrap(), "-");
        assert_eq!(ModifiedStrFunction::generate(path, &safe_m).unwrap(), "-");
    }

    #[test]
    #[cfg(unix)]
    fn test_metadata_generate_loop_recovery() {
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let loop_link = dir.path().join("loop");
        // 自分自身を指すシンボリックリンクを作成
        symlink(&loop_link, &loop_link).unwrap();

        let safe_m = SafeMetadata::recovered();

        // metadata() は ELOOP エラー（エラー値へのフォールバック対象）になるはず
        assert_eq!(
            SizeBytesFunction::generate(&loop_link, &safe_m).unwrap().0,
            METADATA_ERROR
        );
        assert_eq!(
            ModifiedTsFunction::generate(&loop_link, &safe_m).unwrap().0,
            METADATA_ERROR
        );

        // Str系 もエラー値 "-" になるはず
        assert_eq!(
            SizeStrFunction::generate(&loop_link, &safe_m).unwrap(),
            "-"
        );
        assert_eq!(
            ModifiedStrFunction::generate(&loop_link, &safe_m).unwrap(),
            "-"
        );
    }

    #[test]
    fn test_metadata_generate_success() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let m = std::fs::metadata(&path).unwrap();
        let safe_m = SafeMetadata::new(&m);

        assert_eq!(SizeBytesFunction::generate(&path, &safe_m).unwrap().0, 5);
        assert!(ModifiedTsFunction::generate(&path, &safe_m).unwrap().0 > 0);
        assert_eq!(SizeStrFunction::generate(&path, &safe_m).unwrap(), "5.0 B");
        assert!(!ModifiedStrFunction::generate(&path, &safe_m)
            .unwrap()
            .is_empty());
    }
}
