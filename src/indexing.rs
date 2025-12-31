use crate::taggers::{TagValue, TargetTable};
use crate::FunctionRegistry;
use crate::functions::{
    SizeBytesFunction, ModifiedTsFunction, PathFunction,
    FilenameFunction, ParentDirFunction, ExtensionFunction,
};
use anyhow::{Result, Context};
use duckdb::{Connection, ToSql};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use rayon::prelude::*;

#[derive(Debug, PartialEq)]
pub struct ScanEntry {
    pub path: String,
    pub inode: String,
    pub size: i64,
    pub mtime: i64,
}

struct IndexDiff {
    pub to_tag: Vec<ScanEntry>,
    pub moved: Vec<(i64, String)>,
    pub deleted_ids: Vec<i64>,
    pub unchanged_ids: Vec<i64>,
}

pub struct Indexer<'a> {
    conn: &'a Connection,
    registry: &'a FunctionRegistry,
    db_dir: PathBuf,
}

impl<'a> Indexer<'a> {
    pub fn new(conn: &'a Connection, registry: &'a FunctionRegistry, db_dir: PathBuf) -> Self {
        Self { conn, registry, db_dir }
    }

    fn entities_path(&self) -> PathBuf { self.db_dir.join("entities.parquet") }
    fn locations_path(&self) -> PathBuf { self.db_dir.join("locations.parquet") }
    fn tags_path(&self) -> PathBuf { self.db_dir.join("tags.parquet") }
    fn temp_scan_path(&self) -> PathBuf { self.db_dir.join("current_scan.parquet") }

    pub fn run<P, F>(&self, root_path: P, on_progress: Option<&F>, dry_run: bool) -> Result<usize>
    where
        P: AsRef<Path>,
        F: Fn(usize) + Sync + Send,
    {
        let root_path = root_path.as_ref();
        
        let count = self.scan_phase(root_path, on_progress, dry_run)?;
        if dry_run { return Ok(count); }

        let diff = self.diff_phase()?;
        let (tagging_results, moved_locations) = self.tagging_phase(diff.to_tag, diff.moved)?;
        self.merge_phase(tagging_results, moved_locations, diff.deleted_ids, diff.unchanged_ids)?;

        Ok(count)
    }

    fn scan_phase<F>(&self, root_path: &Path, on_progress: Option<&F>, dry_run: bool) -> Result<usize>
    where
        F: Fn(usize) + Sync + Send,
    {
        if !dry_run {
            self.conn.execute_batch("
                CREATE TABLE IF NOT EXISTS temp_scan (path VARCHAR, inode VARCHAR, size BIGINT, mtime BIGINT);
                DELETE FROM temp_scan;
            ").context("Failed to prepare temp_scan table")?;
        }

        let db_dir_canonical = self.db_dir.canonicalize().unwrap_or_else(|_| self.db_dir.clone());
        let (tx, rx) = std::sync::mpsc::channel::<(String, String, i64, i64)>();
        let mut count = 0;

        let walker = ignore::WalkBuilder::new(root_path)
            .hidden(false)
            .git_ignore(true)
            .threads(rayon::current_num_threads())
            .build_parallel();

        let scan_thread = std::thread::spawn(move || {
            walker.run(|| {
                let tx = tx.clone();
                let db_dir_canonical = db_dir_canonical.clone();
                Box::new(move |result| {
                    if let Ok(entry) = result {
                        if let Ok(path) = entry.path().canonicalize() {
                            if path.starts_with(&db_dir_canonical) { return ignore::WalkState::Continue; }
                        }

                        let path_str = entry.path().to_string_lossy().to_string();
                        let inode = crate::get_inode_string(entry.path());
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

        {
            let mut appender = if !dry_run { Some(self.conn.appender("temp_scan")?) } else { None };
            for (path_str, inode, size, mtime) in rx {
                if let Some(ref mut app) = appender {
                    app.append_row(&[&path_str as &dyn ToSql, &inode, &size, &mtime])?;
                }
                count += 1;
                if let Some(cb) = on_progress {
                    if count % 1000 == 0 { cb(count); }
                }
            }
        }

        scan_thread.join().map_err(|e| anyhow::anyhow!("Scan thread panicked: {:?}", e))?;

        if !dry_run {
            let scan_path = self.temp_scan_path().to_string_lossy().to_string();
            self.conn.execute(&format!("COPY temp_scan TO '{}' (FORMAT 'parquet')", scan_path), [])
                .with_context(|| format!("Failed to export temp_scan to {}", scan_path))?;
            self.conn.execute("DROP TABLE temp_scan", [])
                .context("Failed to drop temp_scan table")?;
        }

        if let Some(cb) = on_progress { cb(count); }
        Ok(count)
    }

    fn diff_phase(&self) -> Result<IndexDiff> {
        let has_existing = self.entities_path().exists() && self.locations_path().exists();
        let entities_str = self.entities_path().to_string_lossy().to_string();
        let locations_str = self.locations_path().to_string_lossy().to_string();
        let scan_path_str = self.temp_scan_path().to_string_lossy().to_string();
        let current_scan_sql = format!("read_parquet('{}')", scan_path_str);

        if !has_existing {
            let mut stmt = self.conn.prepare(&format!("SELECT path, inode, size, mtime FROM {}", current_scan_sql))
                .context("Failed to prepare initial scan query")?;
            let to_tag = stmt.query_map([], |row| Ok(ScanEntry {
                path: row.get(0)?, inode: row.get(1)?, size: row.get(2)?, mtime: row.get(3)?,
            }))?.collect::<std::result::Result<Vec<_>, _>>()?;
            
            return Ok(IndexDiff { to_tag, moved: vec![], deleted_ids: vec![], unchanged_ids: vec![] });
        }

        let old_entities_sql = format!("read_parquet('{}')", entities_str);
        let old_locations_sql = format!("read_parquet('{}')", locations_str);

        let size_col = SizeBytesFunction::NAME;
        let mtime_col = ModifiedTsFunction::NAME;

        let to_tag = self.conn.prepare(&format!(
            "SELECT s.path, s.inode, s.size, s.mtime FROM {} s 
             WHERE NOT EXISTS (SELECT 1 FROM {} e WHERE e.inode = s.inode AND e.{} = s.mtime AND e.{} = s.size)", 
             current_scan_sql, old_entities_sql, mtime_col, size_col
        )).context("Failed to prepare diff query (to_tag)")?
        .query_map([], |row| Ok(ScanEntry {
            path: row.get(0)?, inode: row.get(1)?, size: row.get(2)?, mtime: row.get(3)?,
        }))?.collect::<std::result::Result<Vec<_>, _>>()?;

        let moved = self.conn.prepare(&format!(
            "SELECT e.id, s.path FROM {} e JOIN {} s ON e.inode = s.inode JOIN {} l ON e.id = l.entity_id 
             WHERE l.path != s.path AND s.mtime = e.{} AND s.size = e.{}",
             old_entities_sql, current_scan_sql, old_locations_sql, mtime_col, size_col
        )).context("Failed to prepare diff query (moved)")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;

        let deleted_ids = self.conn.prepare(&format!(
            "SELECT id FROM {} EXCEPT SELECT e.id FROM {} e JOIN {} s ON e.inode = s.inode WHERE e.{} = s.mtime AND e.{} = s.size",
            old_entities_sql, old_entities_sql, current_scan_sql, mtime_col, size_col
        )).context("Failed to prepare diff query (deleted_ids)")?
        .query_map([], |row| row.get(0))?.collect::<std::result::Result<Vec<_>, _>>()?;

        let unchanged_ids = self.conn.prepare(&format!(
            "SELECT e.id FROM {} e JOIN {} s ON e.inode = s.inode JOIN {} l ON e.id = l.entity_id 
             WHERE l.path = s.path AND s.mtime = e.{} AND s.size = e.{}",
             old_entities_sql, current_scan_sql, old_locations_sql, mtime_col, size_col
        )).context("Failed to prepare diff query (unchanged_ids)")?
        .query_map([], |row| row.get(0))?.collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(IndexDiff { to_tag, moved, deleted_ids, unchanged_ids })
    }

    fn tagging_phase(&self, to_tag: Vec<ScanEntry>, moved: Vec<(i64, String)>) -> Result<(Vec<TaggingResult>, Vec<DynamicRow>)> {
        let columns = self.registry.get_all_columns();
        let entities_path = self.entities_path();
        let max_id: i64 = if entities_path.exists() {
            let entities_str = entities_path.to_string_lossy().to_string();
            self.conn.query_row(&format!("SELECT COALESCE(MAX(id), 0) FROM read_parquet('{}')", entities_str), [], |r| r.get(0))?
        } else {
            0
        };

        let tagging_results = to_tag.into_par_iter().enumerate().map(|(i, entry)| {
            let entity_id = max_id + (i as i64) + 1;
            let path = Path::new(&entry.path);
            let values = self.registry.process_file(path)?;
            
            let mut entity_row = DynamicRow { id: entity_id, values: Vec::new() };
            entity_row.values.push(TagValue::Text(entry.inode));
            
            let mut location_row = DynamicRow { id: entity_id, values: Vec::new() };
            let mut tags = Vec::new();

            for (col_def, val) in columns.iter().zip(values.into_iter()) {
                match col_def.target_table {
                    TargetTable::Entities => entity_row.values.push(val),
                    TargetTable::Locations => location_row.values.push(val),
                    TargetTable::Tags => {
                        let val_str = match val {
                            TagValue::Text(s) => s,
                            TagValue::BigInt(i) => i.to_string(),
                            TagValue::Boolean(b) => b.to_string(),
                            _ => String::new(),
                        };
                        if !val_str.is_empty() {
                            tags.push(TagRow { entity_id, tag_type: col_def.name.clone(), tag_value: val_str });
                        }
                    }
                }
            }
            Ok(TaggingResult { entity_row, location_row, tags })
        }).collect::<Result<Vec<_>>>()?;

        let moved_locations = moved.into_iter().map(|(eid, path_str)| {
            let p = Path::new(&path_str);
            let mut values = Vec::new();
            for col in &columns {
                if col.target_table == TargetTable::Locations {
                    let val = if col.name == PathFunction::NAME {
                        TagValue::Text(path_str.clone())
                    } else if col.name == FilenameFunction::NAME {
                        TagValue::Text(p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
                    } else if col.name == ParentDirFunction::NAME {
                        TagValue::Text(p.parent().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
                    } else if col.name == ExtensionFunction::NAME {
                        TagValue::Text(p.extension().map(|e| e.to_string_lossy().to_string().to_lowercase()).unwrap_or_default())
                    } else {
                        TagValue::Null
                    };
                    values.push(val);
                }
            }
            DynamicRow { id: eid, values }
        }).collect();

        Ok((tagging_results, moved_locations))
    }

    fn merge_phase(&self, tagging_results: Vec<TaggingResult>, moved_locations: Vec<DynamicRow>, deleted_ids: Vec<i64>, unchanged_ids: Vec<i64>) -> Result<()> {
        let columns = self.registry.get_all_columns();
        let entity_cols: Vec<_> = columns.iter().filter(|c| c.target_table == TargetTable::Entities).collect();
        let location_cols: Vec<_> = columns.iter().filter(|c| c.target_table == TargetTable::Locations).collect();

        let mut ent_sql = "CREATE TABLE temp_entities (id BIGINT, inode VARCHAR".to_string();
        for col in &entity_cols { ent_sql.push_str(&format!(", {} {}", col.name, col.sql_type)); }
        ent_sql.push_str(");");

        let mut loc_sql = "CREATE TABLE temp_locations (entity_id BIGINT".to_string();
        for col in &location_cols { loc_sql.push_str(&format!(", {} {}", col.name, col.sql_type)); }
        loc_sql.push_str(");");

        self.conn.execute_batch(&format!("
            {}
            {}
            CREATE TABLE temp_tags (entity_id BIGINT, tag_type VARCHAR, tag_value VARCHAR);
        ", ent_sql, loc_sql))?;

        {
            let mut app_ent = self.conn.appender("temp_entities")?;
            let mut app_loc = self.conn.appender("temp_locations")?;
            let mut app_tag = self.conn.appender("temp_tags")?;

            for res in tagging_results {
                let mut ent_refs: Vec<&dyn ToSql> = Vec::with_capacity(res.entity_row.values.len() + 1);
                ent_refs.push(&res.entity_row.id);
                for val in &res.entity_row.values { ent_refs.push(val); }
                app_ent.append_row(ent_refs.as_slice())?;

                let mut loc_refs: Vec<&dyn ToSql> = Vec::with_capacity(res.location_row.values.len() + 1);
                loc_refs.push(&res.location_row.id);
                for val in &res.location_row.values { loc_refs.push(val); }
                app_loc.append_row(loc_refs.as_slice())?;

                for t in res.tags {
                    app_tag.append_row(&[&t.entity_id as &dyn ToSql, &t.tag_type, &t.tag_value])?;
                }
            }

            for loc in moved_locations {
                let mut loc_refs: Vec<&dyn ToSql> = Vec::with_capacity(loc.values.len() + 1);
                loc_refs.push(&loc.id);
                for val in &loc.values { loc_refs.push(val); }
                app_loc.append_row(loc_refs.as_slice())?;
            }
        }

        let entities_str = self.entities_path().to_string_lossy().to_string();
        let locations_str = self.locations_path().to_string_lossy().to_string();
        let tags_str = self.tags_path().to_string_lossy().to_string();

        if Path::new(&entities_str).exists() {
            let filter = if deleted_ids.is_empty() { "1=1".to_string() } else { 
                format!("id NOT IN ({})", deleted_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",")) 
            };
            let keep_filter = if unchanged_ids.is_empty() { "1=0".to_string() } else {
                format!("entity_id IN ({})", unchanged_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(","))
            };
            let tags_filter = if deleted_ids.is_empty() { "1=1".to_string() } else { 
                format!("entity_id NOT IN ({})", deleted_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",")) 
            };

            self.conn.execute(&format!("COPY (SELECT * FROM read_parquet('{}') WHERE {} UNION ALL SELECT * FROM temp_entities) TO '{}.tmp' (FORMAT 'parquet', COMPRESSION 'zstd')", entities_str, filter, entities_str), [])?;
            self.conn.execute(&format!("COPY (SELECT * FROM read_parquet('{}') WHERE {} UNION ALL SELECT * FROM temp_locations) TO '{}.tmp' (FORMAT 'parquet', COMPRESSION 'zstd')", locations_str, keep_filter, locations_str), [])?;
            self.conn.execute(&format!("COPY (SELECT * FROM read_parquet('{}') WHERE {} UNION ALL SELECT * FROM temp_tags) TO '{}.tmp' (FORMAT 'parquet', COMPRESSION 'zstd')", tags_str, tags_filter, tags_str), [])?;

            std::fs::rename(format!("{}.tmp", entities_str), &entities_str)?;
            std::fs::rename(format!("{}.tmp", locations_str), &locations_str)?;
            std::fs::rename(format!("{}.tmp", tags_str), &tags_str)?;
        } else {
            self.conn.execute(&format!("COPY temp_entities TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", entities_str), [])?;
            self.conn.execute(&format!("COPY temp_locations TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", locations_str), [])?;
            self.conn.execute(&format!("COPY temp_tags TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", tags_str), [])?;
        }

        self.conn.execute_batch("DROP TABLE temp_entities; DROP TABLE temp_locations; DROP TABLE temp_tags;").ok();
        std::fs::remove_file(self.temp_scan_path()).ok();
        Ok(())
    }
}

pub struct TaggingResult {
    pub entity_row: DynamicRow,
    pub location_row: DynamicRow,
    pub tags: Vec<TagRow>,
}

pub struct DynamicRow {
    pub id: i64,
    pub values: Vec<TagValue>,
}

#[derive(Debug, PartialEq)]
pub struct TagRow {
    pub entity_id: i64,
    pub tag_type: String,
    pub tag_value: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use crate::FileManager;

    #[test]
    fn test_indexer_basic_flow() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let db_dir = root.join(".ttfm/db");
        
        std::fs::write(root.join("test.rs"), "fn main() {}").unwrap();
        
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        let indexer = Indexer::new(&fm.conn, &fm.registry, db_dir);
        
        let count = indexer.run(root, None::<&fn(usize)>, false).unwrap();
        assert!(count >= 1);
        
        let results = fm.search("extension:rs").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("test.rs"));
    }
}