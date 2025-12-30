//! # TTFM (Typed Tag File Manager) Core Library
//!
//! このライブラリは、Typed Tag（型付きタグ）を用いたファイル管理システムのコア機能を提供します。
//! DuckDBをバックエンドに使用し、Parquet形式でのインデックス保存と高速な検索を実現します。

use anyhow::{Context, Result};
use duckdb::{Connection, ToSql};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use rayon::prelude::*;
use std::time::UNIX_EPOCH;
use file_id::get_file_id;

pub mod types;
pub mod query;
pub mod plugins;
pub mod config;
mod taggers;
mod functions;
mod indexing;

pub use query::{QueryParser, QueryNode};
pub use taggers::{ColumnDef, TagValue, Tagger};
use functions::{
    TagFunction,
    PathFunction,
    ParentDirFunction,
    FilenameFunction,
    StemFunction,
    ExtensionFunction,
    DirectoryFunction,
    SizeBytesFunction,
    ModifiedTsFunction,
    KindFunction,
    SizeStrFunction,
    ModifiedStrFunction,
};
pub use types::{TagType, TypedTag}; 

/// ファイルの一意識別子を取得し、文字列として返します。
fn get_inode_string(path: &Path) -> String {
    match get_file_id(path) {
        Ok(id) => format!("{:?}", id),
        Err(_) => path.to_string_lossy().to_string(), // フォールバックとしてパスを使用
    }
}

/// インデックス（データベース）のデフォルト保存先ディレクトリ。
const DEFAULT_DB_DIR: &str = ".ttfm/db";

/// 全ての `TagFunction` を管理し、インデックス作成と検索の仲介を行うレジストリ。
pub struct FunctionRegistry {
    /// 登録されている機能のリスト
    functions: Vec<Box<dyn TagFunction>>,
}

impl FunctionRegistry {
    /// 空のレジストリを作成します。
    pub fn new() -> Self {
        Self { functions: Vec::new() }
    }

    /// 新しい機能（`TagFunction`）をレジストリに追加します。
    pub fn register(&mut self, func: Box<dyn TagFunction>) {
        self.functions.push(func);
    }

    /// 標準的な機能をすべて登録したレジストリを返します。
    /// これにはファイル名、拡張子、サイズ、更新日時、ユーザータグなどが含まれます。
    pub fn with_standard() -> Self {
        let mut reg = Self::new();
        // 登録順序が重要（カラム順序になるため）
        reg.register(Box::new(PathFunction::new()));
        reg.register(Box::new(ParentDirFunction::new()));
        reg.register(Box::new(FilenameFunction::new()));
        reg.register(Box::new(StemFunction::new()));
        reg.register(Box::new(ExtensionFunction::new()));
        reg.register(Box::new(DirectoryFunction::new()));
        reg.register(Box::new(SizeBytesFunction::new()));
        reg.register(Box::new(ModifiedTsFunction::new()));
        reg.register(Box::new(KindFunction::new()));
        reg.register(Box::new(SizeStrFunction::new()));
        reg.register(Box::new(ModifiedStrFunction::new()));
        reg
    }

    // --- Indexing Support ---

    /// 登録されている全機能からデータベースのカラム定義を取得します。
    pub fn get_all_columns(&self) -> Vec<ColumnDef> {
        let mut cols = Vec::new();
        for func in &self.functions {
            cols.extend(func.tagger().get_columns());
        }
        cols
    }

    /// 指定されたファイルパスに対してタグ付けを実行し、1行分のデータを返します。
    pub fn process_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let mut row = Vec::new();
        for func in &self.functions {
            let values = func.tagger().tag_file(path)?;
            row.extend(values);
        }
        Ok(row)
    }

    // --- Search Support ---

    /// クエリツリーを辿って、DuckDBで使用可能なSQL WHERE条件式を生成します。
    pub fn generate_sql(&self, node: &QueryNode, tags_path: &str) -> String {
        let sql = match node {
            QueryNode::And(left, right) => format!("({} AND {})", self.generate_sql(left, tags_path), self.generate_sql(right, tags_path)),
            QueryNode::Or(left, right) => format!("({} OR {})", self.generate_sql(left, tags_path), self.generate_sql(right, tags_path)),
            QueryNode::Not(child) => format!("NOT ({})", self.generate_sql(child, tags_path)),
            QueryNode::TypedTag(tt) => self.tag_to_sql(tt),
        };
        // プレースホルダを実際のパスに置換（各Function実装がこれを使う）
        sql.replace("__TAGS_TABLE__", &format!("'{}'", tags_path))
    }

    /// `TypedTag` を具体的なSQL条件に変換します。
    /// 各機能に問い合わせ、対応するものがない場合は常に偽となる条件を返します。
    fn tag_to_sql(&self, tag: &TypedTag) -> String {
        // 各Functionに問い合わせる
        for func in &self.functions {
            if let Some(sql) = func.to_sql(tag) {
                return sql;
            }
        }
        // マッチする機能がない場合は常に偽
        "1=0".to_string()
    }
}

/// ファイル管理システムのメインインターフェース。
pub struct FileManager {
    /// DuckDB接続
    conn: Connection,
    /// データベースディレクトリのパス
    db_dir: std::path::PathBuf,
    /// 利用可能な機能のレジストリ
    registry: FunctionRegistry,
}

impl FileManager {
    /// デフォルト設定で `FileManager` を作成します。
    pub fn new() -> Result<Self> {
        Self::new_with_db_dir(DEFAULT_DB_DIR)
    }

    /// 指定されたデータベースディレクトリで `FileManager` を作成します。
    pub fn new_with_db_dir<P: AsRef<Path>>(db_dir: P) -> Result<Self> {
        let db_dir = db_dir.as_ref().to_path_buf();
        
        // データベースディレクトリを作成（存在しない場合）
        if !db_dir.exists() {
            std::fs::create_dir_all(&db_dir)
                .context(format!("Failed to create database directory: {:?}", db_dir))?;
        }

        let conn = Connection::open_in_memory()
            .context("Failed to open in-memory database connection")?;
        
        Ok(Self { 
            conn,
            db_dir,
            registry: FunctionRegistry::with_standard(),
        })
    }
    
    // 互換性のためのエイリアス
    pub fn new_with_index_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::new_with_db_dir(path)
    }

    fn entities_path(&self) -> std::path::PathBuf { self.db_dir.join("entities.parquet") }
    fn locations_path(&self) -> std::path::PathBuf { self.db_dir.join("locations.parquet") }
    fn tags_path(&self) -> std::path::PathBuf { self.db_dir.join("tags.parquet") }
    fn temp_scan_path(&self) -> std::path::PathBuf { self.db_dir.join("current_scan.parquet") }

    /// テストなどの用途向けにインメモリでのみ動作する `FileManager` を作成します。
    pub fn new_in_memory() -> Result<Self> {
        Self::new()
    }

    /// 指定されたディレクトリを再帰的にスキャンし、インデックスを作成します。
    pub fn index_directory<P: AsRef<Path>, F>(&self, root_path: P, on_progress: Option<&F>, dry_run: bool) -> Result<usize> 
    where
        F: Fn(usize) + Sync + Send,
    {
        let root_path = root_path.as_ref();
        
        // --- 1. Scan Phase ---
        if !dry_run {
            self.conn.execute_batch("
                CREATE TABLE IF NOT EXISTS temp_scan (path VARCHAR, inode VARCHAR, size BIGINT, mtime BIGINT);
                DELETE FROM temp_scan;
            ")?;
        }

        let db_dir_canonical = self.db_dir.canonicalize().unwrap_or_else(|_| self.db_dir.clone());
        let (tx, rx) = std::sync::mpsc::channel::<(String, String, i64, i64)>();
        let mut count = 0;

        // 並列スキャンの実行
        let walker = ignore::WalkBuilder::new(root_path)
            .hidden(false) // 隠しファイルも対象にする
            .git_ignore(true) // .gitignore は尊重する（設定次第で変更可能）
            .threads(rayon::current_num_threads()) // Rayonと同じスレッド数を使用
            .build_parallel();

        let scan_thread = std::thread::spawn(move || {
            walker.run(|| {
                let tx = tx.clone();
                let db_dir_canonical = db_dir_canonical.clone();
                Box::new(move |result| {
                    if let Ok(entry) = result {
                        // DBディレクトリ配下のファイルは除外
                        if let Ok(path) = entry.path().canonicalize() {
                            if path.starts_with(&db_dir_canonical) { return ignore::WalkState::Continue; }
                        }

                        let path_str = entry.path().to_string_lossy().to_string();
                        let inode = get_inode_string(entry.path());
                        let metadata = match entry.metadata() {
                            Ok(m) => m,
                            Err(_) => return ignore::WalkState::Continue,
                        };
                        let size = if entry.path().is_dir() { 0 } else { metadata.len() as i64 };
                        let mtime = metadata.modified()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
                            .unwrap_or(0);

                        let _ = tx.send((path_str, inode, size, mtime));
                    }
                    ignore::WalkState::Continue
                })
            });
        });

        // メインスレッドで受信して Appender に書き込む
        {
            let mut appender = if !dry_run { Some(self.conn.appender("temp_scan")?) } else { None };

            for (path_str, inode, size, mtime) in rx {
                if let Some(ref mut app) = appender {
                    app.append_row(&[
                        &path_str as &dyn ToSql,
                        &inode,
                        &size,
                        &mtime
                    ])?;
                }

                count += 1;
                if let Some(cb) = on_progress {
                    if count % 1000 == 0 { cb(count); }
                }
            }
        }

        scan_thread.join().map_err(|_| anyhow::anyhow!("Scan thread panicked"))?;

        if !dry_run {
            // スキャン結果を Parquet に出力
            let scan_path = self.temp_scan_path().to_string_lossy().to_string();
            self.conn.execute(&format!("COPY temp_scan TO '{}' (FORMAT 'parquet')", scan_path), [])
                .context("Failed to export current_scan.parquet")?;
            self.conn.execute("DROP TABLE temp_scan", [])?;
        }

        if let Some(cb) = on_progress { cb(count); }

        if dry_run {
            return Ok(count);
        }

        // --- 2. Diff Phase ---
        // 既存データの有無を確認
        let has_existing = self.entities_path().exists() && self.locations_path().exists();
        let entities_str = self.entities_path().to_string_lossy().to_string();
        let locations_str = self.locations_path().to_string_lossy().to_string();
        let scan_path_str = self.temp_scan_path().to_string_lossy().to_string();

        // Parquetファイル参照用のSQLフラグメント
        let old_entities_sql = format!("read_parquet('{}')", entities_str);
        let old_locations_sql = format!("read_parquet('{}')", locations_str);
        let current_scan_sql = format!("read_parquet('{}')", scan_path_str);

        let max_id: i64;
        let to_tag: Vec<crate::indexing::ScanEntry>;
        let mut moved: Vec<(i64, String)> = Vec::new(); // entity_id, new_path
        let mut deleted_ids: Vec<i64> = Vec::new();
        let mut unchanged_ids: Vec<i64> = Vec::new();

        if has_existing {
            // 1. 最大IDを取得
            max_id = self.conn.query_row(&format!("SELECT COALESCE(MAX(id), 0) FROM {}", old_entities_sql), [], |r| r.get(0))?;

            // 2. To Process (新規 または 内容変更) を抽出
            let mut stmt = self.conn.prepare(
                &format!("SELECT s.path, s.inode, s.size, s.mtime 
                 FROM {} s
                 WHERE NOT EXISTS (
                    SELECT 1 FROM {} e 
                    WHERE e.inode = s.inode AND e.mtime = s.mtime AND e.size = s.size
                 )", current_scan_sql, old_entities_sql)
            )?;
            to_tag = stmt.query_map([], |row| Ok(crate::indexing::ScanEntry {
                path: row.get(0)?,
                inode: row.get(1)?,
                size: row.get(2)?,
                mtime: row.get(3)?,
            }))?.collect::<std::result::Result<Vec<_>, _>>()?;

            // 3. Moved (Inode/Metadata一致 だが Path不一致) を抽出
            let mut stmt = self.conn.prepare(
                &format!("SELECT e.id, s.path 
                 FROM {} e
                 JOIN {} s ON e.inode = s.inode
                 JOIN {} l ON e.id = l.entity_id
                 WHERE l.path != s.path AND s.mtime = e.mtime AND s.size = e.size",
                 old_entities_sql, current_scan_sql, old_locations_sql)
            )?;
            moved = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            // 4. Deleted (既存にあるがスキャン結果にない、または再タグ付け対象になった古いエントリ)
            let mut stmt = self.conn.prepare(
                &format!("SELECT id FROM {} 
                 EXCEPT 
                 SELECT e.id FROM {} e 
                 JOIN {} s ON e.inode = s.inode 
                 WHERE e.mtime = s.mtime AND e.size = s.size",
                 old_entities_sql, old_entities_sql, current_scan_sql)
            )?;
            deleted_ids = stmt.query_map([], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            // 5. Unchanged
            let mut stmt = self.conn.prepare(
                &format!("SELECT e.id FROM {} e
                 JOIN {} s ON e.inode = s.inode
                 JOIN {} l ON e.id = l.entity_id
                 WHERE l.path = s.path AND s.mtime = e.mtime AND s.size = e.size",
                 old_entities_sql, current_scan_sql, old_locations_sql)
            )?;
            unchanged_ids = stmt.query_map([], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;

        } else {
            max_id = 0;
            // 初回インデックス時は全件を to_tag へ
            let mut stmt = self.conn.prepare(&format!("SELECT path, inode, size, mtime FROM {}", current_scan_sql))?;
            to_tag = stmt.query_map([], |row| Ok(crate::indexing::ScanEntry {
                path: row.get(0)?,
                inode: row.get(1)?,
                size: row.get(2)?,
                mtime: row.get(3)?,
            }))?.collect::<std::result::Result<Vec<_>, _>>()?;
        }

        // --- 3. Tagging Phase ---
        self.conn.execute_batch("
            CREATE TABLE IF NOT EXISTS temp_entities (id BIGINT, inode VARCHAR, size BIGINT, mtime BIGINT);
            CREATE TABLE IF NOT EXISTS temp_locations (entity_id BIGINT, path VARCHAR, filename VARCHAR, parentdir VARCHAR, extension VARCHAR);
            CREATE TABLE IF NOT EXISTS temp_tags (entity_id BIGINT, tag_type VARCHAR, tag_value VARCHAR);
            DELETE FROM temp_entities;
            DELETE FROM temp_locations;
            DELETE FROM temp_tags;
        ")?;

        let columns: Vec<ColumnDef> = self.registry.get_all_columns();
        let registry = &self.registry;

        let tagging_results: Vec<Result<(crate::indexing::EntityRow, crate::indexing::LocationRow, Vec<crate::indexing::TagRow>)>> = to_tag
            .par_iter()
            .enumerate()
            .map(|(i, entry)| {
                let entity_id = max_id + (i as i64) + 1;
                let path = Path::new(&entry.path);
                let values = registry.process_file(path)?;
                let data: Vec<(ColumnDef, TagValue)> = columns.iter()
                    .zip(values.into_iter())
                    .map(|(c, v)| (c.clone(), v))
                    .collect();
                Ok(crate::indexing::convert_to_rows(entity_id, entry.inode.clone(), &data))
            })
            .collect();

        // 移動分の Location 生成
        let moved_locations: Vec<crate::indexing::LocationRow> = moved.into_iter().map(|(eid, path_str)| {
            let p = Path::new(&path_str);
            let filename = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let parentdir = p.parent().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let extension = p.extension().map(|e| e.to_string_lossy().to_string().to_lowercase()).unwrap_or_default();
            
            crate::indexing::LocationRow {
                entity_id: eid,
                path: path_str,
                filename,
                parentdir,
                extension,
            }
        }).collect();

        // --- 4. Merge/Finalize Phase ---
        {
            let mut app_entities = self.conn.appender("temp_entities")?;
            let mut app_locations = self.conn.appender("temp_locations")?;
            let mut app_tags = self.conn.appender("temp_tags")?;

            for res in tagging_results {
                let (entity, loc, tags) = res?;
                app_entities.append_row(&[&entity.id as &dyn ToSql, &entity.inode, &entity.size, &entity.mtime])?;
                app_locations.append_row(&[&loc.entity_id as &dyn ToSql, &loc.path, &loc.filename, &loc.parentdir, &loc.extension])?;
                for t in tags {
                    app_tags.append_row(&[&t.entity_id as &dyn ToSql, &t.tag_type, &t.tag_value])?;
                }
            }
            
            for loc in moved_locations {
                app_locations.append_row(&[&loc.entity_id as &dyn ToSql, &loc.path, &loc.filename, &loc.parentdir, &loc.extension])?;
            }
        }

        // Parquetファイルへの最終マージ書き出し
        let tags_str = self.tags_path().to_string_lossy().to_string();

        if has_existing {
            // 既存データのうち、維持するもの（Unchanged + Moved）と 新規分を結合
            // 削除・更新された ID は除外
            let filter = if deleted_ids.is_empty() { "1=1".to_string() } else { 
                format!("id NOT IN ({})", deleted_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",")) 
            };
            
            let entities_tmp = format!("{}.tmp", entities_str);
            let locations_tmp = format!("{}.tmp", locations_str);
            let tags_tmp = format!("{}.tmp", tags_str);

            // Entities: (Old - Deleted) + New
            self.conn.execute(&format!("COPY (
                SELECT * FROM {} WHERE {}
                UNION ALL SELECT * FROM temp_entities
            ) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", old_entities_sql, filter, entities_tmp), [])?;

            // Locations: (Old - Deleted - Moved) + New + UpdatedMoved
            let keep_filter = if unchanged_ids.is_empty() { "1=0".to_string() } else {
                format!("entity_id IN ({})", unchanged_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(","))
            };
            
            self.conn.execute(&format!("COPY (
                SELECT * FROM {} WHERE {}
                UNION ALL SELECT * FROM temp_locations
            ) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", old_locations_sql, keep_filter, locations_tmp), [])?;

            // Tags: (Old - Deleted) + New
            let tags_filter = if deleted_ids.is_empty() { "1=1".to_string() } else { 
                format!("entity_id NOT IN ({})", deleted_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",")) 
            };
            let old_tags_sql = format!("read_parquet('{}')", tags_str);
            self.conn.execute(&format!("COPY (
                SELECT * FROM {} WHERE {}
                UNION ALL SELECT * FROM temp_tags
            ) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", old_tags_sql, tags_filter, tags_tmp), [])?;

            // リネームして確定
            std::fs::rename(&entities_tmp, &entities_str)?;
            std::fs::rename(&locations_tmp, &locations_str)?;
            std::fs::rename(&tags_tmp, &tags_str)?;

        } else {
            // 初回保存
            self.conn.execute(&format!("COPY temp_entities TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", entities_str), [])?;
            self.conn.execute(&format!("COPY temp_locations TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", locations_str), [])?;
            self.conn.execute(&format!("COPY temp_tags TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", tags_str), [])?;
        }
        
        // クリーンアップ
        self.conn.execute_batch("DROP TABLE temp_entities; DROP TABLE temp_locations; DROP TABLE temp_tags;").ok();
        std::fs::remove_file(self.temp_scan_path()).ok();
        
        Ok(count)
    }

    /// クエリ文字列を使用してインデックスを検索し、一致したファイルのパス一覧を返します。
    pub fn search(&self, query: &str) -> Result<Vec<String>> {
        if !self.entities_path().exists() || !self.locations_path().exists() || !self.tags_path().exists() {
             return Err(anyhow::anyhow!("Index not found or incomplete. Please run 'index' command first."));
        }

        // Parquetファイルを直接参照するためのパス文字列
        let entities_path = format!("'{}'", self.entities_path().to_string_lossy());
        let locations_path = format!("'{}'", self.locations_path().to_string_lossy());
        // tagsテーブルは各TagFunctionが個別に参照するため、ここではメインのJOINには含めないか、
        // あるいは `search_base` VIEWを作っておく。

        // メインの検索対象は Entities + Locations
        // DuckDBではParquetファイルをテーブルとして直接扱える
        let base_sql = format!(
            "SELECT l.path, l.parentdir AS directory 
             FROM {} e 
             JOIN {} l ON e.id = l.entity_id",
            entities_path, locations_path
        );

        // クエリパースとWHERE句の生成
        let sql_where = if query.trim().is_empty() {
            String::new()
        } else {
            let node = QueryParser::parse(query)?;
            // ここで生成されるSQLは、エイリアス e, l を使う前提、
            // または tags テーブルへのサブクエリを含む必要がある。
            format!("WHERE {}", self.registry.generate_sql(&node, &self.tags_path().to_string_lossy()))
        };

        let sql = format!(
            "{} {} ORDER BY l.parentdir DESC, l.path ASC LIMIT 100",
            base_sql, sql_where
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let paths = stmt.query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;

        Ok(paths)
    }

    /// インデックスファイル（Parquet）を削除します。
    pub fn clear_index(&self) -> Result<()> {
        if self.entities_path().exists() { std::fs::remove_file(self.entities_path()).ok(); }
        if self.locations_path().exists() { std::fs::remove_file(self.locations_path()).ok(); }
        if self.tags_path().exists() { std::fs::remove_file(self.tags_path()).ok(); }
        Ok(())
    }

    /// 指定されたディレクトリからWasmプラグインをロードし、レジストリに登録します。
    /// ".wasm" 拡張子を持つファイルを対象とします。
    pub fn load_plugins<P: AsRef<Path>>(&mut self, dir: P, status: &std::collections::HashMap<String, bool>) -> Result<()> {
        let dir = dir.as_ref();
        if !dir.exists() || !dir.is_dir() {
            return Ok(()); // ディレクトリがない場合は何もしない
        }

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wasm") {
                match crate::plugins::WasmPlugin::new(&path) {
                    Ok(plugin) => {
                        let adapter = plugin.into_adapter()?;
                        
                        // 個別設定のチェック
                        let is_enabled = *status.get(&adapter.name).unwrap_or(&true);
                        println!("DEBUG: Plugin name='{}', is_enabled={}, config_value={:?}", 
                                 adapter.name, is_enabled, status.get(&adapter.name));
                        if is_enabled {
                            println!("Loaded plugin: {} from {:?}", adapter.name, path);
                            self.registry.register(Box::new(adapter));
                        } else {
                            println!("Plugin {} is disabled via config. Skipping.", adapter.name);
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: Failed to load plugin {:?}: {}", path, e);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_get_inode_string() {
        let dir = tempdir().unwrap();
        let path1 = dir.path().join("file1.txt");
        let path2 = dir.path().join("file2.txt");
        File::create(&path1).unwrap();
        File::create(&path2).unwrap();

        let inode1 = get_inode_string(&path1);
        let inode2 = get_inode_string(&path2);

        assert!(!inode1.is_empty());
        assert!(!inode2.is_empty());
        assert_ne!(inode1, inode2, "Different files should have different inodes");

        // 同一ファイルの再取得
        assert_eq!(inode1, get_inode_string(&path1), "Same file should have same inode");
    }

    #[test]
    fn test_scan_phase() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let db_dir = root.join(".ttfm/db");
        
        let file_path = root.join("test.txt");
        std::fs::write(&file_path, "hello world").unwrap(); // 11 bytes
        
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        // 1. スキャン実行（index_directoryの一部として、あるいは内部的に確認）
        // 現状は index_directory を呼ぶことで current_scan.parquet が作られ、最後に削除される
        // テストのために、一時ファイルを削除しないモードか、あるいは内部関数をテストしたいところ
        
        fm.index_directory(root, None::<&fn(usize)>, false).unwrap();
        
        // 2. 結果の検証
        // インデックス作成が成功していれば、内部的に正しくスキャンされているはず
        let results = fm.search("extension:txt").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("test.txt"));
    }

    #[test]
    fn test_file_manager_search_logic() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let index_path = root.join("test_index.parquet");

        File::create(root.join("report_alpha.pdf")).unwrap();
        File::create(root.join("image_1.jpg")).unwrap();
        std::fs::create_dir(root.join("work_docs")).unwrap();
        
        let fm = FileManager::new_with_index_path(&index_path).unwrap();
        fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

        // 修正: Type指定なしの検索はエラーになるため、明示的に filename: を使用するか、
        // エラーになることを確認する。ここでは filename: を使ってヒットすることを確認する。
        assert_eq!(fm.search("filename:report").unwrap().len(), 1);
        assert_eq!(fm.search("extension:pdf").unwrap().len(), 1);
        
        // 修正: Type指定なしの検索がエラーになることを確認
        assert!(fm.search("report").is_err());

        fm.clear_index().unwrap();
    }
}
