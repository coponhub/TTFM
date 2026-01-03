use crate::taggers::{TagValue, TargetTable};
use crate::FunctionRegistry;
use crate::functions::{
    ScanEntry, ScanRole,
};
use crate::db::{Tbl, Col, DuckDbFunc as DBFunc};
use anyhow::{Result, Context};
use duckdb::{Connection, ToSql};
use sea_query::{Query, Expr, Alias, Condition, JoinType, PostgresQueryBuilder, Func, Table, ColumnDef as SeaColumnDef};
use std::path::{Path, PathBuf};
use rayon::prelude::*;

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

    fn file_entities_path(&self) -> PathBuf { self.db_dir.join("file_entities.parquet") }
    fn locations_path(&self) -> PathBuf { self.db_dir.join("locations.parquet") }
    fn file_tags_path(&self) -> PathBuf { self.db_dir.join("file_tags.parquet") }
    fn item_entities_path(&self) -> PathBuf { self.db_dir.join("item_entities.parquet") }
    fn item_tags_path(&self) -> PathBuf { self.db_dir.join("item_tags.parquet") }
    fn temp_scan_path(&self) -> PathBuf { self.db_dir.join("current_scan.parquet") }

    /// Helper to execute "COPY (query) TO 'path' (FORMAT 'parquet', COMPRESSION 'zstd')"
    /// Performs an atomic write by using a temporary file.
    fn copy_to_parquet(&self, query: sea_query::SelectStatement, path: &str) -> Result<()> {
        let sql = query.to_string(PostgresQueryBuilder);
        let tmp_path = format!("{}.tmp", path);
        
        // Always write to a temporary file first
        let copy_sql = format!("COPY ({}) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", sql, tmp_path);
        self.conn.execute(&copy_sql, []).with_context(|| format!("Failed to export parquet to {}", tmp_path))?;
        
        // Atomically rename to the target path
        std::fs::rename(&tmp_path, path).with_context(|| format!("Failed to rename {} to {}", tmp_path, path))?;
        Ok(())
    }

    /// Helper to create a subquery: (SELECT * FROM read_parquet('path'))
    fn parquet_query(&self, path: &str) -> sea_query::SelectStatement {
         Query::select()
            .expr(Expr::cust("*"))
            .from_function(
                Func::cust(DBFunc::ReadParquet).arg(Expr::val(path)),
                Tbl::OriginAlias
            )
            .to_owned()
    }

    pub fn initialize_tables(&self) -> Result<()> {
        // 1. Ensure Parquet files exist (create empty ones if missing)
        self.ensure_empty_parquet_if_missing(&self.file_entities_path(), TargetTable::FileEntities)?;
        self.ensure_empty_parquet_if_missing(&self.locations_path(), TargetTable::Locations)?;
        self.ensure_empty_parquet_if_missing(&self.file_tags_path(), TargetTable::FileTags)?;
        self.ensure_empty_parquet_if_missing(&self.item_entities_path(), TargetTable::ItemEntities)?;
        self.ensure_empty_parquet_if_missing(&self.item_tags_path(), TargetTable::ItemTags)?;

        // 2. Create Unified View
        // Build dynamic SQL for locations columns
        let columns = self.registry.get_all_columns();
        let loc_cols: Vec<_> = columns.iter().filter(|c| c.target_table == TargetTable::Locations).collect();
        let mut loc_union_sqls = Vec::new();
        for col in loc_cols {
            loc_union_sqls.push(format!(
                "SELECT entity_id AS target_id, 'file' AS target_kind, '{}' AS type, CAST({} AS VARCHAR) AS value FROM read_parquet('{}')",
                col.name, col.name, self.locations_path().to_string_lossy()
            ));
        }
        let loc_sql = loc_union_sqls.join(" UNION ALL ");

        let view_sql = format!(r#"
            CREATE OR REPLACE VIEW all_tags AS
            -- Tags from file_tags
            SELECT 
                t.entity_id AS target_id, 
                'file' AS target_kind, 
                t.tag_type AS type, 
                t.tag_value AS value 
            FROM read_parquet('{}') t
            UNION ALL
            -- Attributes from locations
            {}
            UNION ALL
            -- Item Entity Kind/Content
            SELECT
                id AS target_id,
                'item' AS target_kind,
                'itemtype' AS type,
                kind AS value
            FROM read_parquet('{}')
            UNION ALL
            SELECT
                id AS target_id,
                'item' AS target_kind,
                'content' AS type,
                content AS value
            FROM read_parquet('{}')
            UNION ALL
            -- Tags from item_tags
            SELECT 
                it.item_id AS target_id, 
                'item' AS target_kind, 
                it.tag_type AS type, 
                it.tag_value AS value 
            FROM read_parquet('{}') it
        "#, 
        self.file_tags_path().to_string_lossy(),
        loc_sql,
        self.item_entities_path().to_string_lossy(),
        self.item_entities_path().to_string_lossy(),
        self.item_tags_path().to_string_lossy()
        );
        
        self.conn.execute(&view_sql, []).context("Failed to create unified view 'all_tags'")?;

        Ok(())
    }

    fn ensure_empty_parquet_if_missing(&self, path: &Path, target: TargetTable) -> Result<()> {
        if path.exists() { return Ok(()); }

        let table_name = format!("temp_init_{:?}", target);
        let mut create = Table::create().table(Alias::new(&table_name)).to_owned();

        match target {
            TargetTable::FileEntities => {
                 let columns = self.registry.get_all_columns();
                 let cols: Vec<_> = columns.iter().filter(|c| c.target_table == TargetTable::FileEntities).collect();
                 
                 create.col(SeaColumnDef::new(Col::Id).big_integer()); // Base ID
                 for c in cols {
                     let mut def = SeaColumnDef::new(Alias::new(&c.name));
                     match c.sql_type {
                         "BIGINT" => def.big_integer(),
                         "BOOLEAN" => def.boolean(),
                         _ => def.string(),
                     };
                     create.col(&mut def);
                 }
            },
            TargetTable::Locations => {
                let columns = self.registry.get_all_columns();
                let cols: Vec<_> = columns.iter().filter(|c| c.target_table == TargetTable::Locations).collect();
                create.col(SeaColumnDef::new(Col::EntityId).big_integer());
                for c in cols {
                     let mut def = SeaColumnDef::new(Alias::new(&c.name));
                     match c.sql_type { "BIGINT" => def.big_integer(), "BOOLEAN" => def.boolean(), _ => def.string() };
                     create.col(&mut def);
                }
            },
            TargetTable::FileTags => {
                create.col(SeaColumnDef::new(Col::EntityId).big_integer())
                      .col(SeaColumnDef::new(Col::TagType).string())
                      .col(SeaColumnDef::new(Col::TagValue).string());
            },
            TargetTable::ItemEntities => {
                create.col(SeaColumnDef::new(Col::Id).big_integer())
                      .col(SeaColumnDef::new(Col::Kind).string())
                      .col(SeaColumnDef::new(Col::Content).string());
            },
            TargetTable::ItemTags => {
                create.col(SeaColumnDef::new(Col::ItemId).big_integer())
                      .col(SeaColumnDef::new(Col::TagType).string())
                      .col(SeaColumnDef::new(Col::TagValue).string());
            },
        }

        let create_sql = create.to_string(PostgresQueryBuilder);
        self.conn.execute(&create_sql, []).context(format!("Failed to create init table {}", table_name))?;
        
        let path_str = path.to_string_lossy();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context(format!("Failed to create directory {:?}", parent))?;
        }
        
        let copy_sql = format!("COPY {} TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", table_name, path_str);
        self.conn.execute(&copy_sql, []).context(format!("Failed to write empty parquet {}", path_str))?;

        let drop_sql = Table::drop().table(Alias::new(&table_name)).to_string(PostgresQueryBuilder);
        self.conn.execute(&drop_sql, []).ok();

        Ok(())
    }

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
            let mut create_table = Table::create();
            create_table.table(Tbl::TempScan).if_not_exists();
            
            for col_def in ScanEntry::schema() {
                let mut col = SeaColumnDef::new(Alias::new(col_def.name));
                match col_def.sql_type {
                    "BIGINT" => col.big_integer(),
                    _ => col.string(),
                };
                create_table.col(&mut col);
            }
            
            let create_sql = create_table.to_string(PostgresQueryBuilder);
            self.conn.execute(&create_sql, []).context("Failed to create temp_scan table")?;
            
            // Clear table
            let delete_sql = sea_query::Query::delete().from_table(Tbl::TempScan).to_string(PostgresQueryBuilder);
            self.conn.execute(&delete_sql, []).context("Failed to clear temp_scan table")?;
        }

        let db_dir_canonical = self.db_dir.canonicalize().unwrap_or_else(|_| self.db_dir.clone());
        let (tx, rx) = std::sync::mpsc::channel::<ScanEntry>();
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

                        let metadata = match entry.metadata() {
                            Ok(m) => m,
                            Err(_) => return ignore::WalkState::Continue,
                        };

                        match ScanEntry::from_path_metadata(entry.path(), &metadata) {
                            Ok(scan_entry) => {
                                let _ = tx.send(scan_entry);
                            }
                            Err(e) => {
                                eprintln!("Error: Failed to process {:?}: {}", entry.path(), e);
                            }
                        }
                    }
                    ignore::WalkState::Continue
                })
            });
        });

        {
            let mut appender = if !dry_run { Some(self.conn.appender("temp_scan")?) } else { None };
            for entry in rx {
                if let Some(ref mut app) = appender {
                    let params = entry.as_params();
                    app.append_row(&*params)?;
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
            // COPY (SELECT * FROM temp_scan) TO ...
            let query = Query::select().expr(Expr::cust("*")).from(Tbl::TempScan).to_owned();
            self.copy_to_parquet(query, &scan_path)?;
            
            let drop_sql = Table::drop().table(Tbl::TempScan).to_string(PostgresQueryBuilder);
            self.conn.execute(&drop_sql, []).context("Failed to drop temp_scan table")?;
        }

        if let Some(cb) = on_progress { cb(count); }
        Ok(count)
    }

    fn diff_phase(&self) -> Result<IndexDiff> {
        let has_existing = self.file_entities_path().exists() && self.locations_path().exists();
        let entities_str = self.file_entities_path().to_string_lossy().to_string();
        let locations_str = self.locations_path().to_string_lossy().to_string();
        let scan_path_str = self.temp_scan_path().to_string_lossy().to_string();

        let schema = ScanEntry::schema();
        let col_aliases: Vec<Alias> = schema.iter().map(|c| Alias::new(c.name)).collect();
        
        let identity_match = || {
            let mut cond = Condition::all();
            for (i, col_def) in schema.iter().enumerate() {
                if matches!(col_def.role, ScanRole::ScanId) {
                    let col = col_aliases[i].clone();
                    cond = cond.add(Expr::col((Tbl::EntAlias, col.clone())).eq(Expr::col((Tbl::ScanAlias, col.clone()))));
                }
            }
            cond
        };
        let integrity_match = || {
            let mut cond = Condition::all();
            for (i, col_def) in schema.iter().enumerate() {
                if matches!(col_def.role, ScanRole::Integrity) {
                    let col = col_aliases[i].clone();
                    cond = cond.add(Expr::col((Tbl::EntAlias, col.clone())).eq(Expr::col((Tbl::ScanAlias, col.clone()))));
                }
            }
            cond
        };

        if !has_existing {
            let query = Query::select()
                .columns(col_aliases.clone())
                .from_subquery(self.parquet_query(&scan_path_str), Tbl::ScanAlias)
                .to_string(PostgresQueryBuilder);

            let mut stmt = self.conn.prepare(&query).context("Failed to prepare initial scan query")?;
            let to_tag = stmt.query_map([], |row| {
                ScanEntry::from_row(row)
            })?.collect::<std::result::Result<Vec<_>, _>>()?;
            
            return Ok(IndexDiff { to_tag, moved: vec![], deleted_ids: vec![], unchanged_ids: vec![] });
        }

        // 1. to_tag: WHERE NOT EXISTS (SELECT 1 FROM entities e WHERE identity AND integrity)
        let subquery_exists = Query::select()
            .expr(Expr::val(1))
            .from_subquery(self.parquet_query(&entities_str), Tbl::EntAlias)
            .cond_where(identity_match())
            .cond_where(integrity_match())
            .to_owned();

        let query_to_tag = Query::select()
            .columns(col_aliases.iter().map(|a| (Tbl::ScanAlias, a.clone())).collect::<Vec<_>>())
            .from_subquery(self.parquet_query(&scan_path_str), Tbl::ScanAlias)
            .and_where(Expr::exists(subquery_exists).not())
            .to_string(PostgresQueryBuilder);

        let to_tag = self.conn.prepare(&query_to_tag)
            .context("Failed to prepare diff query (to_tag)")?
            .query_map([], |row| {
                ScanEntry::from_row(row)
            })?.collect::<std::result::Result<Vec<_>, _>>()?;

        // 2. moved: Identity match AND Integrity match AND Path mismatch
        let col_path = col_aliases[0].clone();
        
        let query_moved = Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .column((Tbl::ScanAlias, col_path.clone()))
            .from_subquery(self.parquet_query(&entities_str), Tbl::EntAlias)
            .join_subquery(
                JoinType::InnerJoin,
                self.parquet_query(&scan_path_str),
                Tbl::ScanAlias,
                identity_match()
            )
            .join_subquery(
                JoinType::InnerJoin,
                self.parquet_query(&locations_str),
                Tbl::LocAlias,
                Expr::col((Tbl::EntAlias, Col::Id)).eq(Expr::col((Tbl::LocAlias, Col::EntityId)))
            )
            .and_where(Expr::col((Tbl::LocAlias, Col::Path)).ne(Expr::col((Tbl::ScanAlias, col_path.clone()))))
            .cond_where(integrity_match())
            .to_string(PostgresQueryBuilder);

        let moved = self.conn.prepare(&query_moved)
            .context("Failed to prepare diff query (moved)")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;

        // 3. deleted_ids: Identity match missing in Scan
        let query_deleted = Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .from_subquery(self.parquet_query(&entities_str), Tbl::EntAlias)
            .join_subquery(
                JoinType::LeftJoin,
                self.parquet_query(&scan_path_str),
                Tbl::ScanAlias,
                Condition::all().add(identity_match()).add(integrity_match())
            )
            .and_where(Expr::col((Tbl::ScanAlias, col_path.clone())).is_null())
            .to_string(PostgresQueryBuilder);

        let deleted_ids = self.conn.prepare(&query_deleted)
            .context("Failed to prepare diff query (deleted_ids)")?
            .query_map([], |row| row.get(0))?.collect::<std::result::Result<Vec<_>, _>>()?;

        // 4. unchanged_ids: Identity match AND Integrity match AND Path match
        let query_unchanged = Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .from_subquery(self.parquet_query(&entities_str), Tbl::EntAlias)
            .join_subquery(
                JoinType::InnerJoin,
                self.parquet_query(&scan_path_str),
                Tbl::ScanAlias,
                identity_match()
            )
            .join_subquery(
                JoinType::InnerJoin,
                self.parquet_query(&locations_str),
                Tbl::LocAlias,
                Expr::col((Tbl::EntAlias, Col::Id)).eq(Expr::col((Tbl::LocAlias, Col::EntityId)))
            )
            .and_where(Expr::col((Tbl::LocAlias, Col::Path)).eq(Expr::col((Tbl::ScanAlias, col_path.clone()))))
            .cond_where(integrity_match())
            .to_string(PostgresQueryBuilder);

        let unchanged_ids = self.conn.prepare(&query_unchanged)
            .context("Failed to prepare diff query (unchanged_ids)")?
            .query_map([], |row| row.get(0))?.collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(IndexDiff { to_tag, moved, deleted_ids, unchanged_ids })
    }

    fn tagging_phase(&self, to_tag: Vec<ScanEntry>, moved: Vec<(i64, String)>) -> Result<(Vec<TaggingResult>, Vec<DynamicRow>)> {
        let columns = self.registry.get_all_columns();
        let file_entities_path = self.file_entities_path();
                    let max_id: i64 = if file_entities_path.exists() {
                    let entities_str = file_entities_path.to_string_lossy().to_string();
                    let query = Query::select()
                        .expr(Func::cust(DBFunc::Coalesce).args([Expr::col(Col::Id).max(), Expr::val(0).into()]))
                        .from_subquery(self.parquet_query(&entities_str), Tbl::EntAlias)
                        .to_string(PostgresQueryBuilder);
                    self.conn.query_row(&query, [], |r| r.get(0))?
        } else {
            0
        };

        let tagging_results = to_tag.into_par_iter().enumerate().map(|(i, entry)| {
            let entity_id = max_id + (i as i64) + 1;
            let path = Path::new(&entry.path.value);
            let values = self.registry.process_file(path)?;
            
            let mut entity_row = DynamicRow { id: entity_id, values: Vec::new() };
            
            let mut location_row = DynamicRow { id: entity_id, values: Vec::new() };
            let mut tags = Vec::new();

            for (col_def, val) in columns.iter().zip(values.into_iter()) {
                match col_def.target_table {
                    TargetTable::FileEntities => entity_row.values.push(val),
                    TargetTable::Locations => location_row.values.push(val),
                    TargetTable::FileTags => {
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
                    _ => {}
                }
            }
            Ok(TaggingResult { entity_row, location_row, tags })
        }).collect::<Result<Vec<_>>>()?;

        let functions = self.registry.all_functions();
        let moved_locations = moved.into_iter().map(|(eid, path_str)| {
            let p = Path::new(&path_str);
            let mut values = Vec::new();
            
            for func in functions {
                let cols = func.tagger().get_columns();
                for _ in cols {
                    if func.role() == ScanRole::Location {
                        let val = func.generate_from_path(p).unwrap_or(TagValue::Null);
                        values.push(val);
                    }
                }
            }
            DynamicRow { id: eid, values }
        }).collect();

        Ok((tagging_results, moved_locations))
    }

    fn merge_phase(&self, tagging_results: Vec<TaggingResult>, moved_locations: Vec<DynamicRow>, deleted_ids: Vec<i64>, unchanged_ids: Vec<i64>) -> Result<()> {
        let columns = self.registry.get_all_columns();
        let entity_cols: Vec<_> = columns.iter().filter(|c| c.target_table == TargetTable::FileEntities).collect();
        let location_cols: Vec<_> = columns.iter().filter(|c| c.target_table == TargetTable::Locations).collect();

        // Helper to create temp table definition
        let create_temp_table = |name: &str, base_cols: Vec<(Alias, &str)>, dyn_cols: &Vec<&crate::taggers::ColumnDef>| -> String {
            let mut create = Table::create()
                .table(Alias::new(name))
                .to_owned();
            
            for (col_alias, col_type) in base_cols {
                let mut def = SeaColumnDef::new(col_alias);
                match col_type {
                    "BIGINT" => def.big_integer(),
                    _ => def.string(),
                };
                create.col(&mut def);
            }

            for col in dyn_cols {
                let mut def = SeaColumnDef::new(Alias::new(&col.name));
                match col.sql_type {
                    "BIGINT" => def.big_integer(),
                    "BOOLEAN" => def.boolean(),
                    _ => def.string(),
                };
                create.col(&mut def);
            }
            create.to_string(PostgresQueryBuilder)
        };

        let sql_ents = create_temp_table("temp_file_entities", vec![(Alias::new("id"), "BIGINT")], &entity_cols);
        let sql_locs = create_temp_table("temp_locations", vec![(Alias::new("entity_id"), "BIGINT")], &location_cols);
        
        let sql_tags = Table::create().table(Tbl::TempFileTags)
            .col(SeaColumnDef::new(Col::EntityId).big_integer())
            .col(SeaColumnDef::new(Col::TagType).string())
            .col(SeaColumnDef::new(Col::TagValue).string())
            .to_string(PostgresQueryBuilder);

        self.conn.execute_batch(&format!("{};{};{}", sql_ents, sql_locs, sql_tags))?;

        {
            let mut app_ent = self.conn.appender("temp_file_entities")?;
            let mut app_loc = self.conn.appender("temp_locations")?;
            let mut app_tag = self.conn.appender("temp_file_tags")?;

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
                    app_tag.append_row([&t.entity_id as &dyn ToSql, &t.tag_type, &t.tag_value])?;
                }
            }

            for loc in moved_locations {
                let mut loc_refs: Vec<&dyn ToSql> = Vec::with_capacity(loc.values.len() + 1);
                loc_refs.push(&loc.id);
                for val in &loc.values { loc_refs.push(val); }
                app_loc.append_row(loc_refs.as_slice())?;
            }
        }

        let entities_str = self.file_entities_path().to_string_lossy().to_string();
        let locations_str = self.locations_path().to_string_lossy().to_string();
        let tags_str = self.file_tags_path().to_string_lossy().to_string();

        let merge_and_write = |parquet_path: &str, temp_table: &str, filter_cond: Option<Condition>| -> Result<()> {
            let query = if Path::new(parquet_path).exists() {
                let mut base_query = Query::select();
                base_query.expr(Expr::cust("*")).from_subquery(self.parquet_query(parquet_path), Tbl::OldAlias);
                
                if let Some(cond) = filter_cond {
                    base_query.cond_where(cond);
                }

                base_query.union(
                    sea_query::UnionType::All,
                    Query::select().expr(Expr::cust("*")).from(Alias::new(temp_table)).to_owned()
                ).to_owned()
            } else {
                Query::select().expr(Expr::cust("*")).from(Alias::new(temp_table)).to_owned()
            };

            self.copy_to_parquet(query, parquet_path)
        };

        let deleted_filter = if deleted_ids.is_empty() { None } else {
             Some(Condition::all().add(Expr::col(Col::Id).is_not_in(deleted_ids.clone())))
        };
        let unchanged_filter = if unchanged_ids.is_empty() { 
            Some(Condition::all().add(Expr::val(1).eq(0))) 
        } else {
             Some(Condition::all().add(Expr::col(Col::EntityId).is_in(unchanged_ids)))
        };
        let tags_filter = if deleted_ids.is_empty() { None } else {
            Some(Condition::all().add(Expr::col(Col::EntityId).is_not_in(deleted_ids)))
        };

        merge_and_write(&entities_str, "temp_file_entities", deleted_filter)?;
        merge_and_write(&locations_str, "temp_locations", unchanged_filter)?;
        merge_and_write(&tags_str, "temp_file_tags", tags_filter)?;

        let drop_sql = Table::drop().table(Tbl::TempFileEntities)
            .table(Tbl::TempLocations)
            .table(Tbl::TempFileTags)
            .to_string(PostgresQueryBuilder);
        self.conn.execute(&drop_sql, []).ok();
        
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
    fn test_initialize_tables() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let conn = Connection::open_in_memory().unwrap();
        let registry = FunctionRegistry::with_standard();
        let indexer = Indexer::new(&conn, &registry, db_dir.clone());

        indexer.initialize_tables().unwrap();

        assert!(db_dir.join("file_entities.parquet").exists());
        assert!(db_dir.join("locations.parquet").exists());
        assert!(db_dir.join("file_tags.parquet").exists());
        assert!(db_dir.join("item_entities.parquet").exists());
        assert!(db_dir.join("item_tags.parquet").exists());

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM all_tags", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
    }

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
        assert!(results[0].primary_value().unwrap().contains("test.rs"));
    }
}