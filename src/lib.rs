use anyhow::{Context, Result};
use duckdb::{Connection, ToSql};
use std::path::Path;
use walkdir::WalkDir;

mod types;
mod query;
mod taggers;
mod functions;

use query::{QueryParser, QueryNode};
use taggers::{ColumnDef, TagValue};
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
    UserTagsFunction,
};
use types::TypedTag; 

// Parquetファイル名
const DEFAULT_INDEX_FILE: &str = "file_index.parquet";

/// 全てのTagFunctionを管理し、インデックス作成と検索のハブとなる
pub struct FunctionRegistry {
    functions: Vec<Box<dyn TagFunction>>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self { functions: Vec::new() }
    }

    pub fn register(&mut self, func: Box<dyn TagFunction>) {
        self.functions.push(func);
    }

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
        reg.register(Box::new(UserTagsFunction::new()));
        reg
    }

    // --- Indexing Support ---

    pub fn get_all_columns(&self) -> Vec<ColumnDef> {
        let mut cols = Vec::new();
        for func in &self.functions {
            cols.extend(func.tagger().get_columns());
        }
        cols
    }

    pub fn process_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let mut row = Vec::new();
        for func in &self.functions {
            let values = func.tagger().tag_file(path)?;
            row.extend(values);
        }
        Ok(row)
    }

    // --- Search Support ---

    pub fn generate_sql(&self, node: &QueryNode) -> String {
        match node {
            QueryNode::And(left, right) => format!("({} AND {})", self.generate_sql(left), self.generate_sql(right)),
            QueryNode::Or(left, right) => format!("({} OR {})", self.generate_sql(left), self.generate_sql(right)),
            QueryNode::Not(child) => format!("NOT ({})", self.generate_sql(child)),
            QueryNode::Term(tag) => format!("filename ILIKE '%{}%'", tag.0.replace("'", "''")),
            QueryNode::TypedTag(tt) => self.tag_to_sql(tt),
        }
    }

    fn tag_to_sql(&self, tag: &TypedTag) -> String {
        // 各Functionに問い合わせる
        for func in &self.functions {
            if let Some(sql) = func.to_sql(tag) {
                // println!("Tag [{}:{}] handled by Function -> SQL: {}", tag.tagtype.0, tag.tag.0, sql);
                return sql;
            }
        }
        // フォールバック
        format!("element_at({}, '{}') ILIKE '%{}%'", UserTagsFunction::NAME, Self::escape(&tag.tagtype.0), Self::escape(&tag.tag.0))
    }
    
    fn escape(s: &str) -> String {
        s.replace("'", "''")
    }
}

pub struct FileManager {
    conn: Connection,
    index_path: std::path::PathBuf,
    registry: FunctionRegistry,
}

impl FileManager {
    pub fn new() -> Result<Self> {
        Self::new_with_index_path(DEFAULT_INDEX_FILE)
    }

    pub fn new_with_index_path<P: AsRef<Path>>(index_path: P) -> Result<Self> {
        let conn = Connection::open_in_memory()
            .context("Failed to open in-memory database connection")?;
        
        Ok(Self { 
            conn,
            index_path: index_path.as_ref().to_path_buf(),
            registry: FunctionRegistry::with_standard(),
        })
    }

    pub fn new_in_memory() -> Result<Self> {
        Self::new()
    }

    pub fn index_directory<P: AsRef<Path>, F>(&self, root_path: P, on_progress: Option<&F>, dry_run: bool) -> Result<usize> 
    where
        F: Fn(usize),
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

        let mut count = 0;
        
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
                
                let row_values = self.registry.process_file(entry.path())?;

                if let Some(ref mut app) = appender {
                    let params: Vec<Box<dyn ToSql>> = row_values.iter()
                        .map(|v| v.to_sql_param())
                        .collect();
                    
                    let params_ref: Vec<&dyn ToSql> = params.iter()
                        .map(|b| b.as_ref())
                        .collect();

                    app.append_row(params_ref.as_slice())?;
                }
                
                count += 1;
                if let Some(cb) = on_progress {
                    if count % 100 == 0 { cb(count); }
                }
            }
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
    
    pub fn clear_index(&self) -> Result<()> {
        if self.index_path.exists() {
            std::fs::remove_file(&self.index_path).context("Failed to remove index file")?;
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
