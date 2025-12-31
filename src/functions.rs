use anyhow::Result;
use std::path::Path;
use std::time::UNIX_EPOCH;
use chrono::{DateTime, Local};
use crate::types::{TypedTag, DBType, FileSize, FileTimestamp};
use crate::taggers::{Tagger, ColumnDef, TagValue, TargetTable};

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
    fn to_sql(&self, tag: &TypedTag) -> Option<String>;

    /// このタグが「スキャンID（同一性確認のキー）」であるかどうかを返します。
    /// 例: inode
    fn is_scanid(&self) -> bool { false }

    /// このタグが「整合性チェック（変更検知）」に使われるかどうかを返します。
    /// 例: size, mtime, hash
    fn is_integrity(&self) -> bool { false }

    /// パスのみから値を生成できる場合、その値を返します。
    fn generate_from_path(&self, _path: &Path) -> Option<TagValue> { None }
}

/// 型レベルでのタグ定義情報を保持するトレイト。
pub trait TagDefinition {
    /// タグの識別子名。
    const NAME: &'static str;
    /// スキャンにおける役割。
    const ROLE: ScanRole;
    /// 対応する Rust の型。
    type RustType: DBType + std::fmt::Debug + PartialEq + Clone;
    /// パスとメタデータから値を生成します。
    fn generate(path: &Path, metadata: &std::fs::Metadata) -> Self::RustType;
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

#[derive(Debug, PartialEq, Clone)]
pub enum ScanRole {
    Location,
    ScanId,
    Integrity,
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
///
/// # Arguments
///
/// * `s` - エスケープ対象の文字列
pub(crate) fn escape(s: &str) -> String {
    s.replace("'", "''")
}

// ========================================================
// 1. Path Function
// ========================================================

/// ファイルパス抽出ロジック。
struct PathTagger;

impl Tagger for PathTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: PathFunction::NAME.to_string(), sql_type: "TEXT", target_table: TargetTable::Locations }]
    }
    /// ファイルの絶対パスを抽出し、パスセパレータを正規化します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        // Windowsのバックスラッシュをスラッシュに正規化
        Ok(vec![TagValue::Text(path.to_string_lossy().replace('\\', "/"))])
    }
}

/// ファイルのフルパス（`path`）に関する機能。
///
/// # Examples
/// - Query: `path:documents` -> パスに "documents" を含むファイルを検索
pub struct PathFunction { tagger: PathTagger }

impl PathFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "path";
    /// 新しい `PathFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: PathTagger } }
}

impl TagFunction for PathFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            return Some(format!("l.{} ILIKE '%{}%'", Self::NAME, escape(&tag.tag.0)));
        }
        None
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        Some(TagValue::Text(path.to_string_lossy().replace('\\', "/")))
    }
}

impl TagDefinition for PathFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Location;
    type RustType = String;
    fn generate(path: &Path, _metadata: &std::fs::Metadata) -> Self::RustType {
        path.to_string_lossy().replace('\\', "/")
    }
}

// ========================================================
// 2. ParentDir Function
// ========================================================

struct ParentDirTagger;

impl Tagger for ParentDirTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: ParentDirFunction::NAME.to_string(), sql_type: "TEXT", target_table: TargetTable::Locations }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let parent = path.parent().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        Ok(vec![TagValue::Text(parent)])
    }
}

/// 親ディレクトリパス（`parentdir`）に関する機能。
///
/// # Examples
/// - Query: `parentdir:src` -> 親ディレクトリが ".../src" または "src" であるファイルを検索
pub struct ParentDirFunction { tagger: ParentDirTagger }

impl ParentDirFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "parentdir";
    /// 新しい `ParentDirFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: ParentDirTagger } }
}

impl TagFunction for ParentDirFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            let val = escape(&tag.tag.0);
            return Some(format!("(l.{} ILIKE '%/{}' OR l.{} = '{}')", Self::NAME, val, Self::NAME, val));
        }
        None
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        Some(TagValue::Text(path.parent().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default()))
    }
}

// ========================================================
// 3. Filename Function
// ========================================================

struct FilenameTagger;

impl Tagger for FilenameTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: FilenameFunction::NAME.to_string(), sql_type: "TEXT", target_table: TargetTable::Locations }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Text(path.file_name().unwrap_or_default().to_string_lossy().to_string())])
    }
}

/// ファイル名（`filename`）に関する機能。
///
/// # Examples
/// - Query: `filename:report` -> ファイル名に "report" を含むファイルを検索（ディレクトリ除外）
pub struct FilenameFunction { tagger: FilenameTagger }

impl FilenameFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "filename";
    /// 新しい `FilenameFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: FilenameTagger } }
}

impl TagFunction for FilenameFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            let val = escape(&tag.tag.0);
            return Some(format!(
                "(l.{} ILIKE '%{}%' AND NOT EXISTS (SELECT 1 FROM __TAGS_TABLE__ t WHERE t.entity_id = e.id AND t.tag_type = '{}' AND t.tag_value = 'TRUE'))",
                Self::NAME, val, DirectoryFunction::NAME
            ));
        }
        None
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        Some(TagValue::Text(path.file_name().unwrap_or_default().to_string_lossy().to_string()))
    }
}

// ========================================================
// 4. Stem Function
// ========================================================

struct StemTagger;

impl Tagger for StemTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: StemFunction::NAME.to_string(), sql_type: "TEXT", target_table: TargetTable::Tags }]
    }
    /// 拡張子を除いたファイル名（ステム）を抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Text(path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())])
    }
}

/// 拡張子を除いたファイル名（`stem`）に関する機能。
///
/// # Examples
/// - Query: `stem:image` -> 拡張子なし名に対する検索（現状はファイル名検索）
pub struct StemFunction { tagger: StemTagger }

impl StemFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "stem";
    /// 新しい `StemFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: StemTagger } }
}

impl TagFunction for StemFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
             let val = escape(&tag.tag.0);
             return Some(format!(
                "(l.{} ILIKE '%{}%' AND NOT EXISTS (SELECT 1 FROM __TAGS_TABLE__ t WHERE t.entity_id = e.id AND t.tag_type = '{}' AND t.tag_value = 'TRUE'))",
                FilenameFunction::NAME, val, DirectoryFunction::NAME
            ));
        }
        None
    }
}

// ========================================================
// 5. Extension Function
// ========================================================

struct ExtensionTagger;

impl Tagger for ExtensionTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: ExtensionFunction::NAME.to_string(), sql_type: "TEXT", target_table: TargetTable::Locations }]
    }
    /// ファイルの拡張子を抽出し、小文字化します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Text(path.extension().map(|e| e.to_string_lossy().to_string().to_lowercase()).unwrap_or_default())])
    }
}

/// 拡張子（`extension`）に関する機能。
///
/// # Examples
/// - Query: `extension:rs` または `ext:rs` -> 拡張子が "rs" のファイルを検索
pub struct ExtensionFunction { tagger: ExtensionTagger }

impl ExtensionFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "extension";
    /// 新しい `ExtensionFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: ExtensionTagger } }
}

impl TagFunction for ExtensionFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            let val = escape(&tag.tag.0);
            return Some(format!("l.{} = '{}'", Self::NAME, val));
        }
        None
    }
    fn generate_from_path(&self, path: &Path) -> Option<TagValue> {
        Some(TagValue::Text(path.extension().map(|e| e.to_string_lossy().to_string().to_lowercase()).unwrap_or_default()))
    }
}

// ========================================================
// 6. Directory Function (Logic)
// ========================================================

struct DirectoryTagger;

impl Tagger for DirectoryTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: DirectoryFunction::NAME.to_string(), sql_type: "BOOLEAN", target_table: TargetTable::Tags }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Boolean(path.is_dir())])
    }
}

/// ディレクトリ判定（`directory`）に関する機能。
///
/// # Examples
/// - Query: `directory:src` -> 名前に "src" を含むディレクトリを検索
pub struct DirectoryFunction { tagger: DirectoryTagger }

impl DirectoryFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "directory";
    /// 新しい `DirectoryFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: DirectoryTagger } }
}

impl TagFunction for DirectoryFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            let val = escape(&tag.tag.0);
            return Some(format!(
                "(l.{} ILIKE '%{}%' AND EXISTS (SELECT 1 FROM __TAGS_TABLE__ t WHERE t.entity_id = e.id AND t.tag_type = '{}' AND t.tag_value = 'TRUE'))",
                FilenameFunction::NAME, val, Self::NAME
            ));
        }
        None
    }
}

// ========================================================
// 7. Size Bytes Function
// ========================================================

struct SizeBytesTagger;

impl Tagger for SizeBytesTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: SizeBytesFunction::NAME.to_string(), sql_type: "BIGINT", target_table: TargetTable::Entities }]
    }
    /// ファイルサイズ（バイト数）を抽出します。ディレクトリの場合は0とします。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let size = if path.is_dir() { 0 } else { std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
        Ok(vec![TagValue::BigInt(size as i64)])
    }
}

/// ファイルサイズ（バイト単位、`size_bytes`）に関する機能。
///
/// # Examples
/// - Query: `size_bytes:1024` -> サイズがちょうど1024バイトのファイルを検索
pub struct SizeBytesFunction { tagger: SizeBytesTagger }

impl SizeBytesFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "size";
    /// 新しい `SizeBytesFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: SizeBytesTagger } }
}

impl TagFunction for SizeBytesFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            return Some(format!("e.{} = {}", Self::NAME, escape(&tag.tag.0)));
        }
        None
    }
    fn is_integrity(&self) -> bool { true }
}

impl TagDefinition for SizeBytesFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Integrity;
    type RustType = FileSize;
    fn generate(path: &Path, metadata: &std::fs::Metadata) -> Self::RustType {
        let size = if path.is_dir() { 0 } else { metadata.len() as i64 };
        FileSize(size)
    }
}

// ========================================================
// 8. Modified TS Function
// ========================================================

struct ModifiedTsTagger;

impl Tagger for ModifiedTsTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: ModifiedTsFunction::NAME.to_string(), sql_type: "BIGINT", target_table: TargetTable::Entities }]
    }
    /// 最終更新日時のUNIXタイムスタンプを抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let ts = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
            .unwrap_or(0);
        Ok(vec![TagValue::BigInt(ts)])
    }
}

/// 更新日時（UNIXタイムスタンプ、`modified_ts`）に関する機能。
///
/// # Examples
/// - Query: `modified_ts:1700000000` -> 指定のタイムスタンプを持つファイルを検索
pub struct ModifiedTsFunction { tagger: ModifiedTsTagger }

impl ModifiedTsFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "mtime";
    /// 新しい `ModifiedTsFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: ModifiedTsTagger } }
}

impl TagFunction for ModifiedTsFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            return Some(format!("e.{} = {}", Self::NAME, escape(&tag.tag.0)));
        }
        None
    }
    fn is_integrity(&self) -> bool { true }
}

impl TagDefinition for ModifiedTsFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::Integrity;
    type RustType = FileTimestamp;
    fn generate(_path: &Path, metadata: &std::fs::Metadata) -> Self::RustType {
        let ts = metadata.modified()
            .and_then(|t| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
            .unwrap_or(0);
        FileTimestamp(ts)
    }
}

// ========================================================
// 9. Inode Function
// ========================================================

struct InodeTagger;

impl Tagger for InodeTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: InodeFunction::NAME.to_string(), sql_type: "VARCHAR", target_table: TargetTable::Entities }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let inode = crate::get_inode_string(path);
        Ok(vec![TagValue::Text(inode)])
    }
}

/// ファイル識別子（`inode`）に関する機能。
pub struct InodeFunction { tagger: InodeTagger }

impl InodeFunction {
    pub const NAME: &'static str = "inode";
    pub fn new() -> Self { Self { tagger: InodeTagger } }
}

impl TagFunction for InodeFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            return Some(format!("e.{} = '{}'", Self::NAME, escape(&tag.tag.0)));
        }
        None
    }
    fn is_scanid(&self) -> bool { true }
}

impl TagDefinition for InodeFunction {
    const NAME: &'static str = Self::NAME;
    const ROLE: ScanRole = ScanRole::ScanId;
    type RustType = String;
    fn generate(path: &Path, _metadata: &std::fs::Metadata) -> Self::RustType {
        crate::get_inode_string(path)
    }
}

// ========================================================
// 10. Kind Function
// ========================================================

struct KindTagger;

impl Tagger for KindTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: KindFunction::NAME.to_string(), sql_type: "TEXT", target_table: TargetTable::Tags }]
    }
    /// ファイルの種類（"Folder", "XXX File"など）を判定して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let is_dir = path.is_dir();
        let ext = path.extension().map(|e| e.to_string_lossy().to_string().to_lowercase()).unwrap_or_default();
        let kind = if is_dir { "Folder".to_string() } else if ext.is_empty() { "File".to_string() } else { format!("{} File", ext.to_uppercase()) };
        Ok(vec![TagValue::Text(kind)])
    }
}

/// ファイルの種類（`kind`）に関する機能。
///
/// # Examples
/// - Query: `kind:Folder` -> ディレクトリを検索
/// - Query: `kind:PDF` -> PDFファイルを検索
pub struct KindFunction { tagger: KindTagger }

impl KindFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "kind";
    /// 新しい `KindFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: KindTagger } }
}

impl TagFunction for KindFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            return Some(format!(
                "EXISTS (SELECT 1 FROM __TAGS_TABLE__ t WHERE t.entity_id = e.id AND t.tag_type = '{}' AND t.tag_value ILIKE '%{}%')",
                Self::NAME, escape(&tag.tag.0)
            ));
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
        vec![ColumnDef { name: SizeStrFunction::NAME.to_string(), sql_type: "TEXT", target_table: TargetTable::Tags }]
    }
    /// ファイルサイズを読みやすい文字列（例: "1.5 MB"）に変換して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let size = if path.is_dir() { 0 } else { std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
        let str = if path.is_dir() { "-".to_string() } else { Self::format_size(size) };
        Ok(vec![TagValue::Text(str)])
    }
}

/// ファイルサイズ文字列表現（`size_str`）に関する機能。
///
/// # Examples
/// - Query: `size_str:KB` -> キロバイト単位のファイルを検索
pub struct SizeStrFunction { tagger: SizeStrTagger }

impl SizeStrFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "size_str";
    /// 新しい `SizeStrFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: SizeStrTagger } }
}

impl TagFunction for SizeStrFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            return Some(format!(
                "EXISTS (SELECT 1 FROM __TAGS_TABLE__ t WHERE t.entity_id = e.id AND t.tag_type = '{}' AND t.tag_value ILIKE '%{}%')",
                Self::NAME, escape(&tag.tag.0)
            ));
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
        vec![ColumnDef { name: ModifiedStrFunction::NAME.to_string(), sql_type: "TEXT", target_table: TargetTable::Tags }]
    }
    /// 最終更新日時を読みやすい文字列（例: "2024-01-01 12:00"）に変換して抽出します。
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let ts = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let str = ts.map(|t| Self::format_time(t)).unwrap_or_default();
        Ok(vec![TagValue::Text(str)])
    }
}

/// 更新日時文字列表現（`modified_str`）に関する機能。
///
/// # Examples
/// - Query: `modified_str:2024` -> 2024年に更新されたファイルを検索
pub struct ModifiedStrFunction { tagger: ModifiedStrTagger }

impl ModifiedStrFunction {
    /// この機能の識別子名。
    pub const NAME: &'static str = "modified_str";
    /// 新しい `ModifiedStrFunction` インスタンスを作成します。
    pub fn new() -> Self { Self { tagger: ModifiedStrTagger } }
}

impl TagFunction for ModifiedStrFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == Self::NAME {
            return Some(format!(
                "EXISTS (SELECT 1 FROM __TAGS_TABLE__ t WHERE t.entity_id = e.id AND t.tag_type = '{}' AND t.tag_value ILIKE '%{}%')",
                Self::NAME, escape(&tag.tag.0)
            ));
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
        let sql = f.to_sql(&ttag(PathFunction::NAME, "foo")).unwrap();
        assert_eq!(sql, format!("l.{} ILIKE '%foo%'", PathFunction::NAME));
    }

    #[test]
    fn test_filename_function() {
        let f = FilenameFunction::new();
        let sql = f.to_sql(&ttag(FilenameFunction::NAME, "report")).unwrap();
        assert!(sql.contains(&format!("l.{} ILIKE '%report%'", FilenameFunction::NAME)));
        assert!(sql.contains("NOT EXISTS"));
        assert!(sql.contains(&format!("tag_type = '{}'", DirectoryFunction::NAME)));
    }

    #[test]
    fn test_extension_function() {
        let f = ExtensionFunction::new();
        let sql = f.to_sql(&ttag(ExtensionFunction::NAME, "rs")).unwrap();
        assert_eq!(sql, format!("l.{} = 'rs'", ExtensionFunction::NAME));
    }

    #[test]
    fn test_size_bytes_function() {
        let f = SizeBytesFunction::new();
        let sql = f.to_sql(&ttag(SizeBytesFunction::NAME, "123")).unwrap();
        assert_eq!(sql, format!("e.{} = 123", SizeBytesFunction::NAME));
        assert!(f.is_integrity());
    }

    #[test]
    fn test_inode_function() {
        let f = InodeFunction::new();
        assert!(f.is_scanid());
        assert_eq!(f.tagger().get_columns()[0].name, "inode");
    }
}
