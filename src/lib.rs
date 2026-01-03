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
pub use types::{SearchResult, TagType, TypedTag};
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
    TypeFromExtFunction,
    SizeStrFunction,
    ModifiedStrFunction,
};
use functions::escape;

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
        reg.register(Box::new(TypeFromExtFunction::new()));
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
            QueryNode::TypedTag(tt) => self.tag_to_sql(tt, tags_path),
        };
        // プレースホルダを実際のパスに置換（各Function実装がこれを使う）
        sql.replace("__TAGS_TABLE__", &format!("'{}'", tags_path))
    }

    /// `all_tags` ビューに対する検索SQL条件を生成します。
    pub fn generate_sql_for_view(&self, node: &QueryNode, view_name: &str) -> String {
        match node {
            QueryNode::And(left, right) => format!("({}) AND ({})", self.generate_sql_for_view(left, view_name), self.generate_sql_for_view(right, view_name)),
            QueryNode::Or(left, right) => format!("({}) OR ({})", self.generate_sql_for_view(left, view_name), self.generate_sql_for_view(right, view_name)),
            QueryNode::Not(child) => {
                format!("target_id NOT IN (SELECT DISTINCT target_id FROM {} WHERE {})", view_name, self.generate_sql_for_view(child, view_name))
            }
            QueryNode::TypedTag(tt) => {
                // View 'all_tags' では全てのタグが type, value として統合されているため、
                // タグの種類によらず一律この形式で検索可能。
                format!("(type = '{}' AND value ILIKE '%{}%')", escape(&tt.tagtype.0), escape(&tt.tag.0))
            }
        }
    }

    /// `TypedTag` を具体的なSQL条件に変換します。
    fn tag_to_sql(&self, tag: &TypedTag, tags_view: &str) -> String {
        // 各Functionに問い合わせる
        for func in &self.functions {
            if let Some(sql) = func.to_sql(tag) {
                // `to_sql` が __TAGS_TABLE__ プレースホルダを使う前提
                return sql.replace("__TAGS_TABLE__", tags_view);
            }
        }
        // マッチする機能がない場合は、汎用のタグテーブルを検索
        // View 'all_tags' schema: target_id, target_kind, type, value
        format!(
            "EXISTS (SELECT 1 FROM {} t WHERE t.target_id = e.id AND t.type = '{}' AND t.value ILIKE '%{}%')",
            tags_view, escape(&tag.tagtype.0), escape(&tag.tag.0)
        )
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
        
        let registry = FunctionRegistry::with_standard();
        
        // Initialize tables and views
        let indexer = crate::indexing::Indexer::new(&conn, &registry, db_dir.clone());
        indexer.initialize_tables().context("Failed to initialize database tables")?;

        Ok(Self { 
            conn,
            db_dir,
            registry,
        })
    }
    
    // 互換性のためのエイリアス
    pub fn new_with_index_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::new_with_db_dir(path)
    }

    fn file_entities_path(&self) -> std::path::PathBuf { self.db_dir.join("file_entities.parquet") }
    fn locations_path(&self) -> std::path::PathBuf { self.db_dir.join("locations.parquet") }
    fn file_tags_path(&self) -> std::path::PathBuf { self.db_dir.join("file_tags.parquet") }
    fn item_entities_path(&self) -> std::path::PathBuf { self.db_dir.join("item_entities.parquet") }
    fn item_tags_path(&self) -> std::path::PathBuf { self.db_dir.join("item_tags.parquet") }

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

    /// クエリ文字列を使用してインデックスを検索し、結果のリストを返します。
    pub fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        if !self.file_entities_path().exists() {
             return Err(anyhow::anyhow!("Index not found or incomplete. Please run 'index' command first."));
        }

        // 1. 検索条件にマッチするIDを抽出するサブクエリ
        // generate_sql は __TAGS_TABLE__ のようなプレースホルダを使うが、
        // to_sql の実装によっては `e.` や `l.` を使うものもあるため、
        // 検索クエリの組み立てには注意が必要。
        // ここでは、`all_tags` からIDを絞り込む方針で統一する。
        let where_clause = if query.trim().is_empty() {
            "1=1".to_string()
        } else {
            let node = QueryParser::parse(query)?;
            // `e` alias を使わずに、`all_tags` のみで完結するSQLを生成する必要がある。
            // そのためには、to_sql の実装を見直す必要がある。
            // ここでは `generate_sql` のフォールバックロジックが `all_tags` を参照することを期待する。
            self.registry.generate_sql_for_view(&node, "all_tags")
        };

        let id_query = format!(
            "SELECT DISTINCT target_id FROM all_tags WHERE {}", 
            where_clause
        );

        // 2. マッチしたIDの全タグを取得して集約
        let sql = format!(r#"
            SELECT 
                t.target_id,
                t.target_kind,
                list(t.type) as types,
                list(t.value) as values
            FROM all_tags t
            WHERE t.target_id IN ({})
            GROUP BY t.target_id, t.target_kind
            LIMIT 100
        "#, id_query);

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            
            use duckdb::types::Value;
            let types_val: Value = row.get(2)?;
            let values_val: Value = row.get(3)?;

            fn value_to_string(v: &Value) -> String {
                match v {
                    Value::Text(s) => s.clone(),
                    Value::BigInt(i) => i.to_string(),
                    Value::Boolean(b) => b.to_string(),
                    _ => format!("{:?}", v),
                }
            }

            let types: Vec<String> = if let Value::List(items) = types_val {
                items.iter().map(value_to_string).collect()
            } else { vec![] };
            
            let values: Vec<String> = if let Value::List(items) = values_val {
                items.iter().map(value_to_string).collect()
            } else { vec![] };

            let tags = types.into_iter().zip(values.into_iter()).collect();
            
            Ok(SearchResult { id, kind, tags })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }

        Ok(results)
    }

    /// インデックスファイル（Parquet）を削除します。
    pub fn clear_index(&self) -> Result<()> {
        if self.file_entities_path().exists() { std::fs::remove_file(self.file_entities_path()).ok(); }
        if self.locations_path().exists() { std::fs::remove_file(self.locations_path()).ok(); }
        if self.file_tags_path().exists() { std::fs::remove_file(self.file_tags_path()).ok(); }
        if self.item_entities_path().exists() { std::fs::remove_file(self.item_entities_path()).ok(); }
        if self.item_tags_path().exists() { std::fs::remove_file(self.item_tags_path()).ok(); }
        Ok(())
    }

    /// 新しいアイテム（Type, Label, Note等）をデータベースに追加します。
    pub fn add_item(&self, kind: &str, content: &str) -> Result<i64> {
        let path = self.item_entities_path();
        if !path.exists() {
             return Err(anyhow::anyhow!("Item entities table not found. Please run index first or re-initialize."));
        }
        
        // 1. Get current min ID
        let query = format!(
            "SELECT MIN(id) FROM read_parquet('{}')", 
            path.to_string_lossy()
        );
        let min_id: i64 = self.conn.query_row(&query, [], |r| r.get(0)).unwrap_or(0);
        let new_id = if min_id > -1 { -1 } else { min_id - 1 };

        // 2. Append new row via Temp Table & COPY
        let path_str = path.to_string_lossy();
        let tmp_path = format!("{}.tmp", path_str);
        
        self.conn.execute_batch(&format!(r#"
            CREATE TABLE temp_add_item AS SELECT * FROM read_parquet('{}');
            INSERT INTO temp_add_item (id, kind, content) VALUES ({}, '{}', '{}');
            COPY temp_add_item TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd');
            DROP TABLE temp_add_item;
        "#, path_str, new_id, kind, escape(content), tmp_path))?;

        std::fs::rename(&tmp_path, path).context("Failed to update item_entities.parquet")?;

        Ok(new_id)
    }

    /// アイテム（ファイルまたは Item Entity）にタグを付与します。
    pub fn tag_item(&self, target: &str, tag_str: &str) -> Result<()> {
        let (key, value) = tag_str.split_once(':')
            .context("Tag must be in 'key:value' format")?;

        // 1. タグ自体の Item Entity が存在することを確認（なければ作成）
        self.get_or_create_item("type", key)?;
        self.get_or_create_item("label", value)?;
        self.get_or_create_item("typedtag", tag_str)?;

        // 2. ターゲットの ID を特定
        let target_id = if let Ok(id) = target.parse::<i64>() {
            id
        } else {
            // パスとして扱い、file_entities から ID を取得
            let query = format!(
                "SELECT entity_id FROM read_parquet('{}') WHERE path = '{}'",
                self.locations_path().to_string_lossy(), escape(target)
            );
            self.conn.query_row(&query, [], |r| r.get(0))
                .context(format!("File not found: {}", target))?
        };

        // 3. 適切なテーブルにタグを保存
        if target_id >= 0 {
            // File Entity へのタグ付け
            self.append_tag_to_parquet(self.file_tags_path(), "temp_add_file_tag", "entity_id", target_id, key, value)?;
        } else {
            // Item Entity へのタグ付け
            self.append_tag_to_parquet(self.item_tags_path(), "temp_add_item_tag", "item_id", target_id, key, value)?;
        }

        Ok(())
    }

    pub fn get_or_create_item(&self, kind: &str, content: &str) -> Result<i64> {
        let path = self.item_entities_path();
        let query = format!(
            "SELECT id FROM read_parquet('{}') WHERE kind = '{}' AND content = '{}'",
            path.to_string_lossy(), kind, escape(content)
        );
        
        if let Ok(id) = self.conn.query_row(&query, [], |r| r.get(0)) {
            Ok(id)
        } else {
            self.add_item(kind, content)
        }
    }

    fn append_tag_to_parquet(&self, path: std::path::PathBuf, temp_table: &str, id_col: &str, id: i64, key: &str, value: &str) -> Result<()> {
        let path_str = path.to_string_lossy();
        let tmp_path = format!("{}.tmp", path_str);

        self.conn.execute_batch(&format!(r#"
            CREATE TABLE {} AS SELECT * FROM read_parquet('{}');
            INSERT INTO {} ({}, tag_type, tag_value) VALUES ({}, '{}', '{}');
            COPY {} TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd');
            DROP TABLE {};
        "#, temp_table, path_str, temp_table, id_col, id, escape(key), escape(value), temp_table, tmp_path, temp_table))?;

        std::fs::rename(&tmp_path, path).context("Failed to update parquet")?;
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
        fm.index_directory(root, None::<&fn(usize)>, false).unwrap();
        
        // 2. 結果の検証
        let results = fm.search("extension:txt").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].primary_value().unwrap().contains("test.txt"));
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

        assert_eq!(fm.search("filename:report").unwrap().len(), 1);
        assert_eq!(fm.search("extension:pdf").unwrap().len(), 1);
        
        assert!(fm.search("report").is_err());

        fm.clear_index().unwrap();
    }

    #[test]
    fn test_add_item_and_get_or_create() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

        let id = fm.add_item("type", "location").unwrap();
        assert_eq!(id, -1);

        let id2 = fm.get_or_create_item("type", "location").unwrap();
        assert_eq!(id, id2);

        let id3 = fm.get_or_create_item("label", "tokyo").unwrap();
        assert_eq!(id3, -2);
    }

    #[test]
    fn test_tag_item_entity() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

        let note_id = fm.add_item("note", "This is a test note").unwrap();
        fm.tag_item(&note_id.to_string(), "status:done").unwrap();

        assert_eq!(fm.get_or_create_item("type", "status").unwrap(), -2);
        assert_eq!(fm.get_or_create_item("label", "done").unwrap(), -3);
        assert_eq!(fm.get_or_create_item("typedtag", "status:done").unwrap(), -4);

        let query = format!("SELECT tag_value FROM read_parquet('{}') WHERE item_id = {}", 
            fm.item_tags_path().to_string_lossy(), note_id);
        let tag_value: String = fm.conn.query_row(&query, [], |r| r.get(0)).unwrap();
        assert_eq!(tag_value, "done");
    }

    #[test]
    fn test_tag_file_entity() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

        let file_path = dir.path().join("test_file.txt");
        std::fs::write(&file_path, "test content").unwrap();
        fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();

        let results = fm.search("extension:txt").unwrap();
        let registered_path = results[0].primary_value().unwrap();
        fm.tag_item(registered_path, "manual:true").unwrap();

        let query = format!("SELECT tag_value FROM read_parquet('{}') WHERE tag_type = 'manual'", 
            fm.file_tags_path().to_string_lossy());
        let tag_value: String = fm.conn.query_row(&query, [], |r| r.get(0)).unwrap();
        assert_eq!(tag_value, "true");
    }
}