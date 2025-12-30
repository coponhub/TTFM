//! # TTFM (Typed Tag File Manager) Core Library
//!
//! このライブラリは、Typed Tag（型付きタグ）を用いたファイル管理システムのコア機能を提供します。
//! DuckDBをバックエンドに使用し、Parquet形式でのインデックス保存と高速な検索を実現します。

use anyhow::{Context, Result};
use duckdb::{Connection, ToSql};
use std::path::Path;
use walkdir::WalkDir;
use rayon::prelude::*;

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
        
        if !dry_run {
            // 3つのテーブルを作成
            self.conn.execute_batch("
                CREATE TABLE IF NOT EXISTS temp_entities (id BIGINT, size BIGINT, mtime BIGINT);
                CREATE TABLE IF NOT EXISTS temp_locations (entity_id BIGINT, path VARCHAR, filename VARCHAR, parentdir VARCHAR, extension VARCHAR);
                CREATE TABLE IF NOT EXISTS temp_tags (entity_id BIGINT, tag_type VARCHAR, tag_value VARCHAR);
                DELETE FROM temp_entities;
                DELETE FROM temp_locations;
                DELETE FROM temp_tags;
            ")?;
        }

        // ファイルリストを収集
        // データベースディレクトリ自体はインデックス対象から除外する
        let db_dir_canonical = self.db_dir.canonicalize().unwrap_or_else(|_| self.db_dir.clone());
        
        let entries: Vec<_> = WalkDir::new(root_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                // DBディレクトリ配下のファイルは除外
                if let Ok(path) = e.path().canonicalize() {
                     !path.starts_with(&db_dir_canonical)
                } else {
                    true
                }
            })
            .collect();

        // カラム定義を取得（並列処理の外で一度だけ取得）
        let columns: Vec<ColumnDef> = self.registry.get_all_columns();
        
        // registry だけを切り出して並列処理に渡す
        let registry = &self.registry;

        // 並列処理: スキャン -> タグ付け -> 行データ変換
        // Entity ID はここで連番を振るのが難しい（並列だから）。
        // したがって、一旦 `enumerate` でインデックスをつけて、それを ID とする。
        let results: Vec<Result<(crate::indexing::EntityRow, crate::indexing::LocationRow, Vec<crate::indexing::TagRow>)>> = entries
            .par_iter()
            .enumerate()
            .map(|(id, entry)| {
                // IDは1から始める
                let entity_id = (id + 1) as i64;
                
                // タグ付け実行
                let values = registry.process_file(entry.path())?;
                
                // カラム定義と値をペアにする
                let data: Vec<(ColumnDef, TagValue)> = columns.iter()
                    .zip(values.into_iter())
                    .map(|(c, v)| (c.clone(), v))
                    .collect();
                
                // 3つの構造体に変換
                Ok(crate::indexing::convert_to_rows(entity_id, &data))
            })
            .collect();

        let mut count = 0;
        
        if !dry_run {
            let mut app_entities = self.conn.appender("temp_entities")?;
            let mut app_locations = self.conn.appender("temp_locations")?;
            let mut app_tags = self.conn.appender("temp_tags")?;

            for row_result in results {
                let (entity, loc, tags) = row_result?;
                
                // Entities
                app_entities.append_row(&[
                    &entity.id as &dyn ToSql,
                    &entity.size,
                    &entity.mtime
                ])?;

                // Locations
                app_locations.append_row(&[
                    &loc.entity_id as &dyn ToSql,
                    &loc.path,
                    &loc.filename,
                    &loc.parentdir,
                    &loc.extension
                ])?;

                // Tags (複数行)
                for tag in tags {
                    app_tags.append_row(&[
                        &tag.entity_id as &dyn ToSql,
                        &tag.tag_type,
                        &tag.tag_value
                    ])?;
                }
                
                count += 1;
                if let Some(cb) = on_progress {
                    if count % 100 == 0 { cb(count); }
                }
            }
        } else {
            count = results.len();
        }

        if let Some(cb) = on_progress { cb(count); }

        if !dry_run {
            // 古いファイルを削除（念のため）
            if self.entities_path().exists() { std::fs::remove_file(self.entities_path()).ok(); }
            if self.locations_path().exists() { std::fs::remove_file(self.locations_path()).ok(); }
            if self.tags_path().exists() { std::fs::remove_file(self.tags_path()).ok(); }
            
            // エクスポート
            let entities_str = self.entities_path().to_string_lossy().to_string();
            let locations_str = self.locations_path().to_string_lossy().to_string();
            let tags_str = self.tags_path().to_string_lossy().to_string();

            self.conn.execute(&format!("COPY temp_entities TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", entities_str), [])
                .context("Failed to export entities")?;
            self.conn.execute(&format!("COPY temp_locations TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", locations_str), [])
                .context("Failed to export locations")?;
            self.conn.execute(&format!("COPY temp_tags TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", tags_str), [])
                .context("Failed to export tags")?;
            
            // 一時テーブル削除
            self.conn.execute_batch("DROP TABLE temp_entities; DROP TABLE temp_locations; DROP TABLE temp_tags;").ok();
        }
        
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
