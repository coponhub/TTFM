use anyhow::Result;
use std::path::Path;
use std::time::UNIX_EPOCH;
use chrono::{DateTime, Local};
use crate::types::{TypedTag, TagType};
use crate::taggers::{Tagger, ColumnDef, TagValue};

/// 機能拡張の単位を表すトレイト。
/// 
/// 以下の2つの責任を持ちます：
/// 1. **抽出 (Tagger)**: ファイルからメタデータ（タグ値）を抽出してDBに保存する。
/// 2. **検索 (Search)**: ユーザーの検索クエリ（TypedTag）をSQL条件式に変換する。
pub trait TagFunction: Send + Sync {
    /// この機能が保持するTagger（抽出ロジック）を取得します。
    fn tagger(&self) -> &dyn Tagger;

    /// 指定された `TypedTag` に対する検索SQL条件を生成します。
    /// この機能が担当しないタグの場合は `None` を返します。
    fn to_sql(&self, tag: &TypedTag) -> Option<String>;
}

// --- Utilities ---

/// SQLインジェクションを防ぐための簡易エスケープ処理。
/// シングルクォートを2つ重ねてエスケープします。
fn escape(s: &str) -> String {
    s.replace("'", "''")
}

// ========================================================
// 1. Path Function
// ========================================================

/// ファイルパス抽出ロジック。
struct PathTagger;

impl Tagger for PathTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: TagType::PATH.to_string(), sql_type: "TEXT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        // Windowsのバックスラッシュをスラッシュに正規化
        Ok(vec![TagValue::Text(path.to_string_lossy().replace('\\', "/"))])
    }
}

/// ファイルのフルパス（`path`）に関する機能。
/// `path:foo` のような検索を担当します。
pub struct PathFunction { tagger: PathTagger }

impl PathFunction {
    pub fn new() -> Self { Self { tagger: PathTagger } }
}

impl TagFunction for PathFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::PATH {
            return Some(format!("{} ILIKE '%{}%'", TagType::PATH, escape(&tag.tag.0)));
        }
        None
    }
}

// ========================================================
// 2. ParentDir Function
// ========================================================

struct ParentDirTagger;

impl Tagger for ParentDirTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: TagType::PARENT_DIR.to_string(), sql_type: "TEXT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let parent = path.parent().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        Ok(vec![TagValue::Text(parent)])
    }
}

/// 親ディレクトリパス（`parentdir`）に関する機能。
/// `parent:src` のような検索を担当します。
pub struct ParentDirFunction { tagger: ParentDirTagger }

impl ParentDirFunction {
    pub fn new() -> Self { Self { tagger: ParentDirTagger } }
}

impl TagFunction for ParentDirFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::PARENT_DIR {
            let val = escape(&tag.tag.0);
            return Some(format!("({} ILIKE '%/{}' OR {} = '{}')", TagType::PARENT_DIR, val, TagType::PARENT_DIR, val));
        }
        None
    }
}

// ========================================================
// 3. Filename Function
// ========================================================

struct FilenameTagger;

impl Tagger for FilenameTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: TagType::FILENAME.to_string(), sql_type: "TEXT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Text(path.file_name().unwrap_or_default().to_string_lossy().to_string())])
    }
}

/// ファイル名（`filename`）に関する機能。
/// `filename:report` のような検索を担当します。ディレクトリは除外されます。
pub struct FilenameFunction { tagger: FilenameTagger }

impl FilenameFunction {
    pub fn new() -> Self { Self { tagger: FilenameTagger } }
}

impl TagFunction for FilenameFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::FILENAME {
            return Some(format!("({} ILIKE '%{}%' AND {} = FALSE)", TagType::FILENAME, escape(&tag.tag.0), TagType::DIRECTORY));
        }
        None
    }
}

// ========================================================
// 4. Stem Function
// ========================================================

struct StemTagger;

impl Tagger for StemTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: TagType::STEM.to_string(), sql_type: "TEXT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Text(path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())])
    }
}

/// 拡張子を除いたファイル名（`stem`）に関する機能。
/// 実質的にはファイル名検索と同じ挙動を提供します。
pub struct StemFunction { tagger: StemTagger }

impl StemFunction {
    pub fn new() -> Self { Self { tagger: StemTagger } }
}

impl TagFunction for StemFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::STEM {
            // Stem検索もファイル名検索として処理（拡張子部分が含まれていてもヒットさせるため、現状はFilenameと同じ扱い）
            return Some(format!("({} ILIKE '%{}%' AND {} = FALSE)", TagType::FILENAME, escape(&tag.tag.0), TagType::DIRECTORY));
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
        vec![ColumnDef { name: TagType::EXTENSION.to_string(), sql_type: "TEXT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Text(path.extension().map(|e| e.to_string_lossy().to_string().to_lowercase()).unwrap_or_default())])
    }
}

/// 拡張子（`extension`）に関する機能。
/// `ext:rs` のような検索を担当します。ディレクトリは除外されます。
pub struct ExtensionFunction { tagger: ExtensionTagger }

impl ExtensionFunction {
    pub fn new() -> Self { Self { tagger: ExtensionTagger } }
}

impl TagFunction for ExtensionFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::EXTENSION {
            let val = escape(&tag.tag.0);
            return Some(format!("({} = '{}' AND {} = FALSE)", TagType::EXTENSION, val, TagType::DIRECTORY));
        }
        None
    }
}

// ========================================================
// 6. Directory Function (Logic)
// ========================================================

struct DirectoryTagger;

impl Tagger for DirectoryTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: TagType::DIRECTORY.to_string(), sql_type: "BOOLEAN" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        Ok(vec![TagValue::Boolean(path.is_dir())])
    }
}

/// ディレクトリ判定（`directory`）に関する機能。
/// `directory:foo` とすると、`foo` という名前を含む**ディレクトリ**を検索します。
pub struct DirectoryFunction { tagger: DirectoryTagger }

impl DirectoryFunction {
    pub fn new() -> Self { Self { tagger: DirectoryTagger } }
}

impl TagFunction for DirectoryFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::DIRECTORY {
            let val = escape(&tag.tag.0);
            return Some(format!("({} ILIKE '%{}%' AND {} = TRUE)", TagType::FILENAME, val, TagType::DIRECTORY));
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
        vec![ColumnDef { name: TagType::SIZE_BYTES.to_string(), sql_type: "BIGINT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let size = if path.is_dir() { 0 } else { std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
        Ok(vec![TagValue::BigInt(size as i64)])
    }
}

/// ファイルサイズ（バイト単位、`size_bytes`）に関する機能。
/// 数値一致検索などを担当します。
pub struct SizeBytesFunction { tagger: SizeBytesTagger }

impl SizeBytesFunction {
    pub fn new() -> Self { Self { tagger: SizeBytesTagger } }
}

impl TagFunction for SizeBytesFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::SIZE_BYTES {
            return Some(format!("{} = {}", TagType::SIZE_BYTES, escape(&tag.tag.0)));
        }
        None
    }
}

// ========================================================
// 8. Modified TS Function
// ========================================================

struct ModifiedTsTagger;

impl Tagger for ModifiedTsTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: TagType::MODIFIED_TS.to_string(), sql_type: "BIGINT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let ts = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
            .unwrap_or(0);
        Ok(vec![TagValue::BigInt(ts)])
    }
}

/// 更新日時（タイムスタンプ、`modified_ts`）に関する機能。
pub struct ModifiedTsFunction { tagger: ModifiedTsTagger }

impl ModifiedTsFunction {
    pub fn new() -> Self { Self { tagger: ModifiedTsTagger } }
}

impl TagFunction for ModifiedTsFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::MODIFIED_TS {
            return Some(format!("{} = {}", TagType::MODIFIED_TS, escape(&tag.tag.0)));
        }
        None
    }
}

// ========================================================
// 9. Kind Function
// ========================================================

struct KindTagger;

impl Tagger for KindTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: TagType::KIND.to_string(), sql_type: "TEXT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let is_dir = path.is_dir();
        let ext = path.extension().map(|e| e.to_string_lossy().to_string().to_lowercase()).unwrap_or_default();
        let kind = if is_dir { "Folder".to_string() } else if ext.is_empty() { "File".to_string() } else { format!("{} File", ext.to_uppercase()) };
        Ok(vec![TagValue::Text(kind)])
    }
}

/// ファイルの種類（`kind`）に関する機能。
/// "Folder", "PDF File" などの表示用文字列を作成・検索します。
pub struct KindFunction { tagger: KindTagger }

impl KindFunction {
    pub fn new() -> Self { Self { tagger: KindTagger } }
}

impl TagFunction for KindFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::KIND {
            return Some(format!("{} ILIKE '%{}%'", TagType::KIND, escape(&tag.tag.0)));
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
        vec![ColumnDef { name: TagType::SIZE_STR.to_string(), sql_type: "TEXT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let size = if path.is_dir() { 0 } else { std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
        let str = if path.is_dir() { "-".to_string() } else { Self::format_size(size) };
        Ok(vec![TagValue::Text(str)])
    }
}

/// ファイルサイズ文字列表現（`size_str`）に関する機能。
/// "1.5 MB" のような文字列を検索対象にします。
pub struct SizeStrFunction { tagger: SizeStrTagger }

impl SizeStrFunction {
    pub fn new() -> Self { Self { tagger: SizeStrTagger } }
}

impl TagFunction for SizeStrFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::SIZE_STR {
            return Some(format!("{} ILIKE '%{}%'", TagType::SIZE_STR, escape(&tag.tag.0)));
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
        vec![ColumnDef { name: TagType::MODIFIED_STR.to_string(), sql_type: "TEXT" }]
    }
    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let ts = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let str = ts.map(|t| Self::format_time(t)).unwrap_or_default();
        Ok(vec![TagValue::Text(str)])
    }
}

/// 更新日時文字列表現（`modified_str`）に関する機能。
/// "2024-01-01" のような日付文字列を検索対象にします。
pub struct ModifiedStrFunction { tagger: ModifiedStrTagger }

impl ModifiedStrFunction {
    pub fn new() -> Self { Self { tagger: ModifiedStrTagger } }
}

impl TagFunction for ModifiedStrFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == TagType::MODIFIED_STR {
            return Some(format!("{} ILIKE '%{}%'", TagType::MODIFIED_STR, escape(&tag.tag.0)));
        }
        None
    }
}

// ========================================================
// 12. User Tags Function
// ========================================================

struct UserTagsTagger;

impl Tagger for UserTagsTagger {
    fn get_columns(&self) -> Vec<ColumnDef> {
        vec![ColumnDef { name: TagType::TAGS.to_string(), sql_type: "MAP(TEXT, TEXT)" }]
    }
    fn tag_file(&self, _path: &Path) -> Result<Vec<TagValue>> {
        // 現在は常に空のタグマップを返します。将来的にCLIやGUIで付与されたタグをここで読み込むことができます。
        Ok(vec![TagValue::Null])
    }
}

/// ユーザー定義タグ（`tags`）に関する機能。
/// 未知のタグタイプ（`project:alpha`など）はすべてここで処理されます。
pub struct UserTagsFunction { tagger: UserTagsTagger }

impl UserTagsFunction {
    pub fn new() -> Self { Self { tagger: UserTagsTagger } }
}

impl TagFunction for UserTagsFunction {
    fn tagger(&self) -> &dyn Tagger { &self.tagger }
    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        // Fallback for unknown tags
        let key = &tag.tagtype.0;
        let val = escape(&tag.tag.0);
        Some(format!("element_at({}, '{}') ILIKE '%{}%'", TagType::TAGS, key, val))
    }
}

// ========================================================
// Unit Tests
// ========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;
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
        // SQL Check
        let sql = f.to_sql(&ttag(TagType::PATH, "foo")).unwrap();
        assert_eq!(sql, "path ILIKE '%foo%'");
        // Tagging Check
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        let values = f.tagger().tag_file(&path).unwrap();
        if let TagValue::Text(s) = &values[0] {
            assert!(s.ends_with("test.txt"));
        } else { panic!("Wrong type"); }
    }

    #[test]
    fn test_filename_function() {
        let f = FilenameFunction::new();
        // SQL Check
        let sql = f.to_sql(&ttag(TagType::FILENAME, "report")).unwrap();
        assert!(sql.contains("filename ILIKE '%report%' "));
        assert!(sql.contains("directory = FALSE"));
        
        // Tagging Check
        let dir = tempdir().unwrap();
        let path = dir.path().join("my_file.txt");
        let values = f.tagger().tag_file(&path).unwrap();
        if let TagValue::Text(s) = &values[0] {
            assert_eq!(s, "my_file.txt");
        } else { panic!("Wrong type"); }
    }

    #[test]
    fn test_extension_function() {
        let f = ExtensionFunction::new();
        // SQL Check
        let sql = f.to_sql(&ttag(TagType::EXTENSION, "rs")).unwrap();
        assert_eq!(sql, "(extension = 'rs' AND directory = FALSE)");

        // Tagging Check
        let dir = tempdir().unwrap();
        let path = dir.path().join("main.rs");
        let values = f.tagger().tag_file(&path).unwrap();
        if let TagValue::Text(s) = &values[0] {
            assert_eq!(s, "rs");
        } else { panic!("Wrong type"); }
    }

    #[test]
    fn test_directory_function() {
        let f = DirectoryFunction::new();
        // SQL Check
        let sql = f.to_sql(&ttag(TagType::DIRECTORY, "src")).unwrap();
        assert!(sql.contains("filename ILIKE '%src%' "));
        assert!(sql.contains("directory = TRUE"));

        // Tagging Check
        let dir = tempdir().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        let values = f.tagger().tag_file(&sub).unwrap();
        if let TagValue::Boolean(b) = values[0] {
            assert!(b);
        } else { panic!("Wrong type"); }
    }

    #[test]
    fn test_parent_dir_function() {
        let f = ParentDirFunction::new();
        let sql = f.to_sql(&ttag(TagType::PARENT_DIR, "src")).unwrap();
        assert_eq!(sql, "(parentdir ILIKE '%/src' OR parentdir = 'src')");
    }

    #[test]
    fn test_user_tags_fallback() {
        let f = UserTagsFunction::new();
        let sql = f.to_sql(&ttag("project", "alpha")).unwrap();
        assert_eq!(sql, "element_at(tags, 'project') ILIKE '%alpha%'");
    }
    
    #[test]
    fn test_size_str_format() {
        // Internal logic test via public interface
        let f = SizeStrFunction::new();
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.bin");
        
        // Create 1KB file
        let f_obj = File::create(&path).unwrap();
        f_obj.set_len(1024).unwrap();

        let values = f.tagger().tag_file(&path).unwrap();
        if let TagValue::Text(s) = &values[0] {
            assert_eq!(s, "1.0 KB");
        } else { panic!("Wrong type"); }
    }
}
