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

/// インデックスのデフォルト保存先ファイル名。
const DEFAULT_INDEX_FILE: &str = "file_index.parquet";

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
    pub fn generate_sql(&self, node: &QueryNode) -> String {
        match node {
            QueryNode::And(left, right) => format!("({} AND {})", self.generate_sql(left), self.generate_sql(right)),
            QueryNode::Or(left, right) => format!("({} OR {})", self.generate_sql(left), self.generate_sql(right)),
            QueryNode::Not(child) => format!("NOT ({})", self.generate_sql(child)),
            QueryNode::Term(tag) => format!("filename ILIKE '%{}%'", tag.0.replace("'", "''")),
            QueryNode::TypedTag(tt) => self.tag_to_sql(tt),
        }
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
    /// インデックス（Parquet）の出力先
    index_path: std::path::PathBuf,
    /// 利用可能な機能のレジストリ
    registry: FunctionRegistry,
}

impl FileManager {
    /// デフォルト設定で `FileManager` を作成します。
    ///
    /// # Examples
    ///
    /// ```
    /// use ttfm::FileManager;
    /// let fm = FileManager::new().unwrap();
    /// ```
    pub fn new() -> Result<Self> {
        Self::new_with_index_path(DEFAULT_INDEX_FILE)
    }

    /// 指定されたインデックス保存先で `FileManager` を作成します。
    pub fn new_with_index_path<P: AsRef<Path>>(index_path: P) -> Result<Self> {
        let conn = Connection::open_in_memory()
            .context("Failed to open in-memory database connection")?;
        
        Ok(Self { 
            conn,
            index_path: index_path.as_ref().to_path_buf(),
            registry: FunctionRegistry::with_standard(),
        })
    }

    /// テストなどの用途向けにインメモリでのみ動作する `FileManager` を作成します。
    pub fn new_in_memory() -> Result<Self> {
        Self::new()
    }

    /// 指定されたディレクトリを再帰的にスキャンし、インデックスを作成します。
    ///
    /// # Arguments
    ///
    /// * `root_path` - スキャンを開始するルートディレクトリ
    /// * `on_progress` - 進捗状況を受け取るコールバック
    /// * `dry_run` - trueの場合、書き込みを行いません
    ///
    /// # Examples
    ///
    /// ```
    /// use ttfm::FileManager;
    /// let fm = FileManager::new_with_index_path("temp.parquet").unwrap();
    /// fm.index_directory(".", Some(&|count| println!("Progress: {}", count)), false).unwrap();
    /// ```
    pub fn index_directory<P: AsRef<Path>, F>(&self, root_path: P, on_progress: Option<&F>, dry_run: bool) -> Result<usize> 
    where
        F: Fn(usize) + Sync + Send,
    {
        let root_path = root_path.as_ref();
        
        if !dry_run {
            let columns_sql = self.registry.get_all_columns().iter()
                .map(|col| format!("{} {}", col.name, col.sql_type))
                .collect::<Vec<_>>()
                .join(", ");
            
            let create_sql = format!("CREATE TABLE IF NOT EXISTS temp_files ({})", columns_sql);

            self.conn.execute(&create_sql, [])
                .context("Failed to create temporary table")?;
            
            self.conn.execute("DELETE FROM temp_files", [])?;
        }

        // ファイルリストを先に収集
        let entries: Vec<_> = WalkDir::new(root_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .collect();

        // registry だけを切り出して並列処理に渡す (Connectionを含まないためSync)
        let registry = &self.registry;

        // 並列にプラグイン処理を実行
        let results: Vec<Result<Vec<TagValue>>> = entries.par_iter()
            .map(|entry| registry.process_file(entry.path()))
            .collect();

        let mut count = 0;
        
        if !dry_run {
            let mut appender = self.conn.appender("temp_files")?;
            for row_result in results {
                let row_values = row_result?;
                let params: Vec<Box<dyn ToSql>> = row_values.iter()
                    .map(|v| v.to_sql_param())
                    .collect();
                
                let params_ref: Vec<&dyn ToSql> = params.iter()
                    .map(|b| b.as_ref())
                    .collect();

                appender.append_row(params_ref.as_slice())?;
                
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
            if self.index_path.exists() {
                std::fs::remove_file(&self.index_path).ok();
            }
            
            let path_str = self.index_path.to_string_lossy();
            self.conn.execute(&format!("COPY temp_files TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", path_str), [])
                .context("Failed to export to Parquet")?;
            
            self.conn.execute("DROP TABLE temp_files", []).ok();
        }
        
        Ok(count)
    }

    /// クエリ文字列を使用してインデックスを検索し、一致したファイルのパス一覧を返します。
    /// インデックスファイル（Parquet）が読み込まれて検索が実行されます。
    ///
    /// # Arguments
    ///
    /// * `query` - 検索クエリ。空文字列の場合は全件表示（上限100件）になります。
    ///
    /// # Returns
    ///
    /// 一致したファイルの絶対/相対パスのベクタ。
    ///
    /// # Examples
    ///
    /// ```
    /// use ttfm::FileManager;
    /// let fm = FileManager::new_with_index_path("example_index.parquet").unwrap();
    /// // インデックス作成済みの前提
    /// let results = fm.search("extension:rs").unwrap();
    /// ```
    pub fn search(&self, query: &str) -> Result<Vec<String>> {
        if !self.index_path.exists() {
             return Err(anyhow::anyhow!("Index not found. Please run 'index' command first."));
        }

        let table_name = format!("'{}'", self.index_path.to_string_lossy());

        let sql_where = if query.trim().is_empty() {
            String::new()
        } else {
            match QueryParser::parse(query) {
                Ok(node) => format!("WHERE {}", self.registry.generate_sql(&node)),
                Err(_) => format!("WHERE filename ILIKE '%{}%'", query.replace("'", "''")) 
            }
        };

        let sql = format!(
            "SELECT path FROM {} {} ORDER BY directory DESC, path ASC LIMIT 100",
            table_name, sql_where
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let paths = stmt.query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;

        Ok(paths)
    }
    
    /// インデックスファイル（Parquet）を削除します。
    pub fn clear_index(&self) -> Result<()> {
        if self.index_path.exists() {
            std::fs::remove_file(&self.index_path).context("Failed to remove index file")?;
        }
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

        assert_eq!(fm.search("report").unwrap().len(), 1);
        assert_eq!(fm.search("extension:pdf").unwrap().len(), 1);
        
        fm.clear_index().unwrap();
    }
}
