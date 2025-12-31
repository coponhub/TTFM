//! # TTFM (Typed Tag File Manager) Core Library
//!
//! このライブラリは、Typed Tag（型付きタグ）を用いたファイル管理システムのコア機能を提供します。
//! DuckDBをバックエンドに使用し、Parquet形式でのインデックス保存と高速な検索を実現します。

use anyhow::{Context, Result};
use duckdb::Connection;
use std::path::Path;
use file_id::get_file_id;

pub mod types;
pub mod query;
pub mod plugins;
pub mod config;
pub mod macros;
mod taggers;
mod functions;
pub mod indexing;

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
    InodeFunction,
    KindFunction,
    SizeStrFunction,
    ModifiedStrFunction,
};
pub use types::{TagType, TypedTag}; 

/// ファイルの一意識別子を取得し、文字列として返します。
pub(crate) fn get_inode_string(path: &Path) -> String {
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

impl Default for FunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
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
        reg.register(Box::new(InodeFunction::new()));
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

    /// 全ての登録済み関数への参照を返します。
    pub fn all_functions(&self) -> &[Box<dyn TagFunction>] {
        &self.functions
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
        let indexer = crate::indexing::Indexer::new(&self.conn, &self.registry, self.db_dir.clone());
        indexer.run(root_path, on_progress, dry_run)
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
