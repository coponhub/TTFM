use anyhow::{Context, Result};
use duckdb::{params, Connection};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;
use chrono::{DateTime, Local};

// クエリモジュールを読み込み
mod query;
use query::{QueryParser, get_schema_columns};

// Parquetファイル名（デフォルト）
const DEFAULT_INDEX_FILE: &str = "file_index.parquet";

pub struct FileManager {
    conn: Connection,
    index_path: std::path::PathBuf,
}

#[derive(Debug)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub kind: String,
    pub size: String,
    pub modified: String,
}

impl FileManager {
    /// FileManagerを初期化 (デフォルトのインデックスパスを使用)
    pub fn new() -> Result<Self> {
        Self::new_with_index_path(DEFAULT_INDEX_FILE)
    }

    /// インメモリDBを使用し、インデックスパスを指定して初期化（テスト用）
    pub fn new_with_index_path<P: AsRef<Path>>(index_path: P) -> Result<Self> {
        let conn = Connection::open_in_memory()
            .context("Failed to open in-memory database connection")?;
        Ok(Self { 
            conn,
            index_path: index_path.as_ref().to_path_buf(),
        })
    }

    // テスト用コンストラクタ（互換性のため）
    pub fn new_in_memory() -> Result<Self> {
        Self::new()
    }

    /// 指定されたパスを再帰的にスキャンしてインデックスを作成し、Parquetファイルに保存する
    /// - `on_progress`: 進捗状況を通知するコールバック
    /// - `dry_run`: 書き込みを行わずスキャンのみ行う（ベンチマーク用）
    pub fn index_directory<P: AsRef<Path>, F>(&self, root_path: P, on_progress: Option<&F>, dry_run: bool) -> Result<usize> 
    where
        F: Fn(usize),
    {
        let root_path = root_path.as_ref();
        
        if !dry_run {
            // インデックス構築用の一時テーブルを作成
            // query.rs の定義からスキーマを動的生成
            let columns_sql = get_schema_columns().iter()
                .map(|col| format!("{} {}", col.name, col.sql_type))
                .collect::<Vec<_>>()
                .join(", ");
            
            let create_sql = format!("CREATE TABLE IF NOT EXISTS temp_files ({})", columns_sql);

            self.conn.execute(&create_sql, [])
                .context("Failed to create temporary table")?;
            
            // 既存データをクリア
            self.conn.execute("DELETE FROM temp_files", [])?;
        }

        let mut count = 0;
        
        // Appenderでメモリ内テーブルに高速挿入
        {
            let mut appender = if !dry_run {
                Some(self.conn.appender("temp_files")?)
            } else {
                None
            };
    
            for entry in WalkDir::new(root_path) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("Warning: Failed to access file: {}", e);
                        continue;
                    }
                };
                let path = entry.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                let is_dir = path.is_dir();
                
                // メタデータ拡張
                let parent_path = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let extension = path.extension().map(|e| e.to_string_lossy().to_string().to_lowercase());

                let kind = if is_dir {
                    "Folder".to_string()
                } else {
                    extension.as_ref()
                        .map(|ext| format!("{} File", ext.to_uppercase()))
                        .unwrap_or_else(|| "File".to_string())
                };
    
                let metadata = std::fs::metadata(path).ok();
                
                // Size
                let size_bytes = if is_dir { 0 } else { metadata.as_ref().map(|m| m.len()).unwrap_or(0) };
                let size_str = if is_dir {
                    "-".to_string()
                } else {
                    format_size(size_bytes)
                };

                // Time
                let modified_ts = metadata.as_ref()
                    .and_then(|m| m.modified().ok())
                    .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
                    .unwrap_or(0);
                
                let modified_str = metadata.as_ref()
                    .and_then(|m| m.modified().ok())
                    .map(format_time)
                    .unwrap_or_default();
    
                if let Some(ref mut app) = appender {
                    // 12カラム (tagsを追加)
                    app.append_row(params![
                        path.to_string_lossy().to_string(),
                        parent_path,
                        name,
                        stem,
                        extension,
                        is_dir,
                        size_bytes as i64,
                        modified_ts,
                        kind,
                        size_str,
                        modified_str,
                        None::<String> // tags (NULL)
                    ])?;
                }
                
                count += 1;
                
                if let Some(cb) = on_progress {
                    if count % 100 == 0 {
                        cb(count);
                    }
                }
            }
        } // appender flush

        // Final callback
        if let Some(cb) = on_progress {
            cb(count);
        }

        if !dry_run {
            // Parquetファイルへの書き出し
            if self.index_path.exists() {
                std::fs::remove_file(&self.index_path).ok();
            }
            
            // DuckDBのCOPYコマンドでファイルパスを指定し、圧縮形式をzstdに設定
            let path_str = self.index_path.to_string_lossy();
            self.conn.execute(&format!("COPY temp_files TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", path_str), [])
                .context("Failed to export to Parquet")?;
            
            // 後始末
            self.conn.execute("DROP TABLE temp_files", []).ok();
        }
        
        Ok(count)
    }

    /// ファイル検索 (Parquetファイルから直接検索)
    pub fn search(&self, query: &str) -> Result<Vec<FileEntry>> {
        if !self.index_path.exists() {
             return Err(anyhow::anyhow!("Index not found. Please run 'index' command first."));
        }

        // Parquetファイルをテーブルとして扱う
        let table_name = format!("'{}'", self.index_path.to_string_lossy());

        let sql_where = if query.trim().is_empty() {
            String::new()
        } else {
            match QueryParser::parse(query) {
                Ok(node) => format!("WHERE {}", node.to_sql()),
                Err(_) => format!("WHERE filename ILIKE '%{}%'", query.replace("'", "''")) 
            }
        };

        let sql = format!(
            "SELECT path, filename, directory, kind, size_str, modified_str FROM {} {} ORDER BY directory DESC, path ASC LIMIT 100",
            table_name, sql_where
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let file_iter = stmt.query_map([], |row| {
            Ok(FileEntry {
                path: row.get(0)?,
                name: row.get(1)?,
                is_dir: row.get(2)?,
                kind: row.get(3)?,
                size: row.get(4)?,
                modified: row.get(5)?,
            })
        })?;

        let mut entries = Vec::new();
        for file in file_iter {
            entries.push(file?);
        }

        Ok(entries)
    }
    
    /// インデックスのクリア (Parquetファイルを削除)
    pub fn clear_index(&self) -> Result<()> {
        if self.index_path.exists() {
            std::fs::remove_file(&self.index_path).context("Failed to remove index file")?;
        }
        Ok(())
    }
}

// ユーティリティ
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

fn format_time(time: SystemTime) -> String {
    let datetime: DateTime<Local> = time.into();
    datetime.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500.0 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn test_file_manager_search_logic() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        
        // Use a parquet file inside the temp dir to avoid pollution
        let index_path = root.join("test_index.parquet");

        // 1. Create Files
        File::create(root.join("report_alpha.pdf")).unwrap();
        File::create(root.join("report_beta.pdf")).unwrap();
        File::create(root.join("draft_alpha.txt")).unwrap();
        File::create(root.join("image_1.jpg")).unwrap();
        File::create(root.join("image_2.png")).unwrap();

        // 2. Create Folders
        std::fs::create_dir(root.join("work_docs")).unwrap();
        std::fs::create_dir(root.join("private_pics")).unwrap();
        
        // Initialize with custom index path
        let fm = FileManager::new_with_index_path(&index_path).unwrap();
        fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

        // Test cases
        assert_eq!(fm.search("report").unwrap().len(), 2);
        
        let res = fm.search("work").unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "work_docs");
        assert!(res[0].is_dir);

        assert_eq!(fm.search("report & alpha").unwrap().len(), 1);
        assert_eq!(fm.search("report | work").unwrap().len(), 3);
        
        let res = fm.search("report & -alpha").unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "report_beta.pdf");

        assert_eq!(fm.search("(alpha | beta) & report").unwrap().len(), 2);
        assert_eq!(fm.search("(report | image) & -(alpha | png)").unwrap().len(), 2);

        // Typed Tag Tests
        // 1. filename:
        // "report" matches filename.
        assert_eq!(fm.search("filename:report").unwrap().len(), 2);
        
        // 2. ext:
        // ext:pdf -> 2 files
        assert_eq!(fm.search("ext:pdf").unwrap().len(), 2);
        // ext:txt -> 1 file
        assert_eq!(fm.search("ext:txt").unwrap().len(), 1);

        // 3. parent:
        // Create a file inside work_docs
        let doc_path = root.join("work_docs/inner_doc.md");
        File::create(&doc_path).unwrap();
        fm.index_directory(root, None::<&fn(usize)>, false).unwrap(); // re-index

        // parent:work_docs -> matches inner_doc.md
        let res = fm.search("parent:work_docs").unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "inner_doc.md");

        // Clear Test
        fm.clear_index().unwrap();
        assert!(!index_path.exists());
        assert!(fm.search("").is_err()); // Index deleted
    }
}