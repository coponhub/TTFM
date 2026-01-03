use crate::taggers::{TagValue, TargetTable, ColumnDef};
use crate::FunctionRegistry;
use crate::functions::{ScanEntry, ScanRole};
use crate::db::{Tbl, Col, DuckDbFunc as DBFunc};
use anyhow::Result;
use duckdb::{Connection, ToSql};
use sea_query::{
    Query, Expr, Alias, Condition, JoinType, PostgresQueryBuilder, 
    Func, Table, ColumnDef as SeaColumnDef, Iden, SelectStatement
};
use std::path::{Path, PathBuf};
use rayon::prelude::*;

// ========================================================
// 1. Storage Manager (IndexStore)
// ========================================================

struct IndexStore<'a> {
    conn: &'a Connection,
    db_dir: PathBuf,
}

impl<'a> IndexStore<'a> {
    fn new(conn: &'a Connection, db_dir: PathBuf) -> Self {
        Self { conn, db_dir }
    }

    fn file_entities_path(&self) -> PathBuf { self.db_dir.join("file_entities.parquet") }
    fn locations_path(&self) -> PathBuf { self.db_dir.join("locations.parquet") }
    fn file_tags_path(&self) -> PathBuf { self.db_dir.join("file_tags.parquet") }
    fn item_entities_path(&self) -> PathBuf { self.db_dir.join("item_entities.parquet") }
    fn item_tags_path(&self) -> PathBuf { self.db_dir.join("item_tags.parquet") }
    fn temp_scan_path(&self) -> PathBuf { self.db_dir.join("current_scan.parquet") }

    fn save_parquet(&self, query: SelectStatement, path: &Path) -> Result<()> {
        let sql = query.to_string(PostgresQueryBuilder);
        let path_str = path.to_string_lossy();
        let tmp_path = format!("{}.tmp", path_str);
        let copy_sql = format!(
            "COPY ({}) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')",
            sql, tmp_path
        );
        self.conn.execute(&copy_sql, [])?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    fn write_parquet(&self, table_name: &str, path: &Path) -> Result<()> {
        let query = Query::select()
            .expr(Expr::cust("*"))
            .from(Alias::new(table_name))
            .to_owned();
        self.save_parquet(query, path)
    }

    fn merge_and_save(
        &self,
        path: &Path,
        temp_table: impl Iden + 'static,
        filter: Option<Condition>,
    ) -> Result<()> {
        let query = if path.exists() {
            let path_str = path.to_string_lossy().to_string();
            let mut base = QueryHelper::parquet_query(&path_str);
            if let Some(cond) = filter {
                base.cond_where(cond);
            }
            base.union(
                sea_query::UnionType::All,
                Query::select()
                    .expr(Expr::cust("*"))
                    .from(temp_table)
                    .to_owned(),
            )
            .to_owned()
        } else {
            Query::select()
                .expr(Expr::cust("*"))
                .from(temp_table)
                .to_owned()
        };
        self.save_parquet(query, path)
    }

    fn create_or_replace_view(&self, name: &str, select: SelectStatement) -> Result<()> {
        let query_sql = select.to_string(PostgresQueryBuilder);
        let sql = format!("CREATE OR REPLACE VIEW {} AS {}", name, query_sql);
        self.conn.execute(&sql, [])?;
        Ok(())
    }
}

// ========================================================
// 2. Query Builder (QueryHelper)
// ========================================================

struct QueryHelper;

impl QueryHelper {
    fn parquet_query(path: &str) -> SelectStatement {
        Query::select()
            .expr(Expr::cust("*"))
            .from_function(
                Func::cust(DBFunc::ReadParquet).arg(Expr::val(path)),
                Tbl::OriginAlias,
            )
            .to_owned()
    }

    fn identity_condition(left: Tbl, right: Tbl) -> Condition {
        let mut cond = Condition::all();
        for cd in ScanEntry::schema() {
            if matches!(cd.role, ScanRole::ScanId) {
                let col = Alias::new(cd.name);
                cond = cond.add(
                    Expr::col((left, col.clone())).eq(Expr::col((right, col.clone()))),
                );
            }
        }
        cond
    }

    fn integrity_condition(left: Tbl, right: Tbl) -> Condition {
        let mut cond = Condition::all();
        for cd in ScanEntry::schema() {
            if matches!(cd.role, ScanRole::Integrity) {
                let col = Alias::new(cd.name);
                cond = cond.add(
                    Expr::col((left, col.clone())).eq(Expr::col((right, col.clone()))),
                );
            }
        }
        cond
    }

    fn build_to_tag_query(scan_path: &str, entities_path: &str) -> SelectStatement {
        let sub_exists = Query::select()
            .expr(Expr::val(1))
            .from_subquery(Self::parquet_query(entities_path), Tbl::EntAlias)
            .cond_where(Self::identity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .cond_where(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .to_owned();

        let columns = ScanEntry::schema()
            .iter()
            .map(|c| (Tbl::ScanAlias, Alias::new(c.name)))
            .collect::<Vec<_>>();

        Query::select()
            .columns(columns)
            .from_subquery(Self::parquet_query(scan_path), Tbl::ScanAlias)
            .and_where(Expr::exists(sub_exists).not())
            .to_owned()
    }

    fn build_moved_query(
        scan_path: &str,
        entities_path: &str,
        loc_path: &str,
    ) -> SelectStatement {
        let col_path = Alias::new(ScanEntry::schema()[0].name);
        Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .column((Tbl::ScanAlias, col_path.clone()))
            .from_subquery(Self::parquet_query(entities_path), Tbl::EntAlias)
            .join_subquery(
                JoinType::InnerJoin,
                Self::parquet_query(scan_path),
                Tbl::ScanAlias,
                Self::identity_condition(Tbl::EntAlias, Tbl::ScanAlias),
            )
            .join_subquery(
                JoinType::InnerJoin,
                Self::parquet_query(loc_path),
                Tbl::LocAlias,
                Expr::col((Tbl::EntAlias, Col::Id))
                    .eq(Expr::col((Tbl::LocAlias, Col::EntityId))),
            )
            .and_where(
                Expr::col((Tbl::LocAlias, Col::Path)).ne(Expr::col((Tbl::ScanAlias, col_path))),
            )
            .cond_where(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .to_owned()
    }

    fn build_deleted_query(scan_path: &str, entities_path: &str) -> SelectStatement {
        let col_path = Alias::new(ScanEntry::schema()[0].name);
        let join_cond = Condition::all()
            .add(Self::identity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .add(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias));

        Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .from_subquery(Self::parquet_query(entities_path), Tbl::EntAlias)
            .join_subquery(
                JoinType::LeftJoin,
                Self::parquet_query(scan_path),
                Tbl::ScanAlias,
                join_cond,
            )
            .and_where(Expr::col((Tbl::ScanAlias, col_path)).is_null())
            .to_owned()
    }

    fn build_unchanged_query(
        scan_path: &str,
        entities_path: &str,
        loc_path: &str,
    ) -> SelectStatement {
        let col_path = Alias::new(ScanEntry::schema()[0].name);
        Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .from_subquery(Self::parquet_query(entities_path), Tbl::EntAlias)
            .join_subquery(
                JoinType::InnerJoin,
                Self::parquet_query(scan_path),
                Tbl::ScanAlias,
                Self::identity_condition(Tbl::EntAlias, Tbl::ScanAlias),
            )
            .join_subquery(
                JoinType::InnerJoin,
                Self::parquet_query(loc_path),
                Tbl::LocAlias,
                Expr::col((Tbl::EntAlias, Col::Id))
                    .eq(Expr::col((Tbl::LocAlias, Col::EntityId))),
            )
            .and_where(
                Expr::col((Tbl::LocAlias, Col::Path)).eq(Expr::col((Tbl::ScanAlias, col_path))),
            )
            .cond_where(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .to_owned()
    }

    fn build_all_tags_view_query(
        all_columns: &[ColumnDef],
        file_tags: &str,
        locs: &str,
        items: &str,
        item_tags: &str,
    ) -> SelectStatement {
        let mut base = Query::select();
        base.column((Tbl::TagAlias, Col::EntityId))
            .expr_as(Expr::val("file"), Alias::new("target_kind"))
            .column((Tbl::TagAlias, Col::TagType))
            .column((Tbl::TagAlias, Col::TagValue))
            .from_subquery(Self::parquet_query(file_tags), Tbl::TagAlias);

        for cd in all_columns
            .iter()
            .filter(|c| c.target_table == TargetTable::Locations)
        {
            let mut sub = Query::select();
            sub.column(Col::EntityId)
                .expr_as(Expr::val("file"), Alias::new("target_kind"))
                .expr_as(Expr::val(cd.name.clone()), Alias::new("tag_type"))
                .expr_as(
                    Expr::col(Alias::new(&cd.name)).cast_as(Alias::new("VARCHAR")),
                    Alias::new("tag_value"),
                )
                .from_subquery(Self::parquet_query(locs), Tbl::LocAlias);
            base.union(sea_query::UnionType::All, sub.to_owned());
        }

        let mut items_type = Query::select();
        items_type
            .column(Col::Id)
            .expr_as(Expr::val("item"), Alias::new("target_kind"))
            .expr_as(Expr::val("itemtype"), Alias::new("tag_type"))
            .column(Col::Kind)
            .from_subquery(Self::parquet_query(items), Tbl::EntAlias);
        base.union(sea_query::UnionType::All, items_type.to_owned());

        let mut items_content = Query::select();
        items_content
            .column(Col::Id)
            .expr_as(Expr::val("item"), Alias::new("target_kind"))
            .expr_as(Expr::val("content"), Alias::new("tag_type"))
            .column(Col::Content)
            .from_subquery(Self::parquet_query(items), Tbl::EntAlias);
        base.union(sea_query::UnionType::All, items_content.to_owned());

        let mut itags = Query::select();
        itags
            .column(Col::ItemId)
            .expr_as(Expr::val("item"), Alias::new("target_kind"))
            .column(Col::TagType)
            .column(Col::TagValue)
            .from_subquery(Self::parquet_query(item_tags), Tbl::TagAlias);
        base.union(sea_query::UnionType::All, itags.to_owned());

        Query::select()
            .expr_as(Expr::col(Alias::new("entity_id")), Alias::new("target_id"))
            .column(Alias::new("target_kind"))
            .expr_as(Expr::col(Alias::new("tag_type")), Alias::new("type"))
            .expr_as(Expr::col(Alias::new("tag_value")), Alias::new("value"))
            .from_subquery(base.to_owned(), Alias::new("union_all_tags"))
            .to_owned()
    }
}

// ========================================================
// 3. Main Indexer
// ========================================================

pub struct Indexer<'a> {
    conn: &'a Connection,
    registry: &'a FunctionRegistry,
    store: IndexStore<'a>,
}

impl<'a> Indexer<'a> {
    pub fn new(
        conn: &'a Connection,
        registry: &'a FunctionRegistry,
        db_dir: PathBuf,
    ) -> Self {
        Self {
            conn,
            registry,
            store: IndexStore::new(conn, db_dir),
        }
    }

    pub fn run<P, F>(
        &self,
        root_path: P,
        on_progress: Option<&F>,
        dry_run: bool,
    ) -> Result<usize>
    where
        P: AsRef<Path>,
        F: Fn(usize) + Sync + Send,
    {
        let count = self.scan_phase(root_path.as_ref(), on_progress, dry_run)?;
        if dry_run {
            return Ok(count);
        }
        let diff = self.diff_phase()?;
        let (results, moved) = self.tagging_phase(diff.to_tag, diff.moved)?;
        self.merge_phase(results, moved, diff.deleted_ids, diff.unchanged_ids)?;
        self.update_system_items()?;
        Ok(count)
    }

    pub fn initialize_tables(&self) -> Result<()> {
        let all_cols = self.registry.get_all_columns();
        self.ensure_empty_parquet_if_missing(
            &self.store.file_entities_path(),
            TargetTable::FileEntities,
            &all_cols,
        )?;
        self.ensure_empty_parquet_if_missing(
            &self.store.locations_path(),
            TargetTable::Locations,
            &all_cols,
        )?;
        self.ensure_empty_parquet_if_missing(
            &self.store.file_tags_path(),
            TargetTable::FileTags,
            &all_cols,
        )?;
        self.ensure_empty_parquet_if_missing(
            &self.store.item_entities_path(),
            TargetTable::ItemEntities,
            &all_cols,
        )?;
        self.ensure_empty_parquet_if_missing(
            &self.store.item_tags_path(),
            TargetTable::ItemTags,
            &all_cols,
        )?;

        let q = QueryHelper::build_all_tags_view_query(
            &all_cols,
            &self.store.file_tags_path().to_string_lossy(),
            &self.store.locations_path().to_string_lossy(),
            &self.store.item_entities_path().to_string_lossy(),
            &self.store.item_tags_path().to_string_lossy(),
        );
        self.store.create_or_replace_view("all_tags", q)?;
        Ok(())
    }

    fn ensure_empty_parquet_if_missing(
        &self,
        path: &Path,
        target: TargetTable,
        columns: &[ColumnDef],
    ) -> Result<()> {
        if path.exists() {
            return Ok(());
        }
        let table_name = format!("temp_init_{:?}", target);
        let create = self.build_table_schema(target, Alias::new(&table_name), columns);
        self.conn
            .execute(&create.to_string(PostgresQueryBuilder), [])?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.store.write_parquet(&table_name, path)?;
        self.conn
            .execute(
                &Table::drop()
                    .table(Alias::new(&table_name))
                    .to_string(PostgresQueryBuilder),
                [],
            )
            .ok();
        Ok(())
    }

    fn scan_phase<F>(
        &self,
        root_path: &Path,
        on_progress: Option<&F>,
        dry_run: bool,
    ) -> Result<usize>
    where
        F: Fn(usize) + Sync + Send,
    {
        if !dry_run {
            let mut create = Table::create();
            create.table(Tbl::TempScan).if_not_exists();
            for cd in ScanEntry::schema() {
                let mut col = SeaColumnDef::new(Alias::new(cd.name));
                if cd.sql_type == "BIGINT" {
                    col.big_integer();
                } else {
                    col.string();
                }
                create.col(&mut col);
            }
            self.conn
                .execute(&create.to_string(PostgresQueryBuilder), [])?;
            self.conn.execute(
                &Query::delete()
                    .from_table(Tbl::TempScan)
                    .to_string(PostgresQueryBuilder),
                [],
            )?;
        }

        let db_dir_canonical = self
            .store
            .db_dir
            .canonicalize()
            .unwrap_or_else(|_| self.store.db_dir.clone());
        let (tx, rx) = std::sync::mpsc::channel::<ScanEntry>();
        let walker = ignore::WalkBuilder::new(root_path)
            .hidden(false)
            .git_ignore(true)
            .threads(rayon::current_num_threads())
            .build_parallel();

        let count = std::thread::scope(|s| {
            s.spawn(move || {
                walker.run(|| {
                    let tx = tx.clone();
                    let db_dir_canonical = db_dir_canonical.clone();
                    Box::new(move |res| {
                        if let Ok(entry) = res {
                            if let Ok(p) = entry.path().canonicalize() {
                                if p.starts_with(&db_dir_canonical) {
                                    return ignore::WalkState::Continue;
                                }
                            }
                            if let Ok(m) = entry.metadata() {
                                if let Ok(se) =
                                    ScanEntry::from_path_metadata(entry.path(), &m)
                                {
                                    let _ = tx.send(se);
                                }
                            }
                        }
                        ignore::WalkState::Continue
                    })
                });
            });

            let mut current_count = 0;
            let mut appender = if !dry_run {
                Some(self.conn.appender("temp_scan")?)
            } else {
                None
            };
            for entry in rx {
                if let Some(ref mut app) = appender {
                    app.append_row(&*entry.as_params())?;
                }
                current_count += 1;
                if let Some(cb) = on_progress {
                    if current_count % 1000 == 0 {
                        cb(current_count);
                    }
                }
            }
            Ok::<usize, anyhow::Error>(current_count)
        })?;

        if !dry_run {
            let query = Query::select()
                .expr(Expr::cust("*"))
                .from(Tbl::TempScan)
                .to_owned();
            self.store.save_parquet(query, &self.store.temp_scan_path())?;
            self.conn
                .execute(
                    &Table::drop()
                        .table(Tbl::TempScan)
                        .to_string(PostgresQueryBuilder),
                    [],
                )
                .ok();
        }
        if let Some(cb) = on_progress {
            cb(count);
        }
        Ok(count)
    }

    fn diff_phase(&self) -> Result<IndexDiff> {
        let scan_path = self.store.temp_scan_path().to_string_lossy().to_string();
        let entities_path = self.store.file_entities_path().to_string_lossy().to_string();
        let locations_path = self.store.locations_path().to_string_lossy().to_string();

        if !self.store.file_entities_path().exists() {
            let col_aliases: Vec<Alias> = ScanEntry::schema()
                .iter()
                .map(|c| Alias::new(c.name))
                .collect();
            let query = Query::select()
                .columns(col_aliases)
                .from_subquery(QueryHelper::parquet_query(&scan_path), Tbl::ScanAlias)
                .to_string(PostgresQueryBuilder);
            let to_tag = self
                .conn
                .prepare(&query)?
                .query_map([], |row| ScanEntry::from_row(row))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            return Ok(IndexDiff {
                to_tag,
                moved: vec![],
                deleted_ids: vec![],
                unchanged_ids: vec![],
            });
        }

        let to_tag_sql = QueryHelper::build_to_tag_query(&scan_path, &entities_path)
            .to_string(PostgresQueryBuilder);
        let to_tag = self
            .conn
            .prepare(&to_tag_sql)?
            .query_map([], |row| ScanEntry::from_row(row))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let moved_sql =
            QueryHelper::build_moved_query(&scan_path, &entities_path, &locations_path)
                .to_string(PostgresQueryBuilder);
        let moved = self
            .conn
            .prepare(&moved_sql)?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let deleted_sql = QueryHelper::build_deleted_query(&scan_path, &entities_path)
            .to_string(PostgresQueryBuilder);
        let deleted_ids = self
            .conn
            .prepare(&deleted_sql)?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let unchanged_sql =
            QueryHelper::build_unchanged_query(&scan_path, &entities_path, &locations_path)
                .to_string(PostgresQueryBuilder);
        let unchanged_ids = self
            .conn
            .prepare(&unchanged_sql)?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(IndexDiff {
            to_tag,
            moved,
            deleted_ids,
            unchanged_ids,
        })
    }

    fn tagging_phase(
        &self,
        to_tag: Vec<ScanEntry>,
        moved: Vec<(i64, String)>,
    ) -> Result<(Vec<TaggingResult>, Vec<DynamicRow>)> {
        let columns = self.registry.get_all_columns();
        let max_id: i64 = if self.store.file_entities_path().exists() {
            let entities_str = self.store.file_entities_path().to_string_lossy().to_string();
            let query = Query::select()
                .expr(
                    Func::cust(DBFunc::Coalesce)
                        .args([Expr::col(Col::Id).max(), Expr::val(0).into()]),
                )
                .from_subquery(QueryHelper::parquet_query(&entities_str), Tbl::EntAlias)
                .to_string(PostgresQueryBuilder);
            self.conn.query_row(&query, [], |r| r.get(0))?
        } else {
            0
        };

        let results = to_tag
            .into_par_iter()
            .enumerate()
            .map(|(i, entry)| {
                let entity_id = max_id + (i as i64) + 1;
                let values = self.registry.process_file(Path::new(&entry.path.value))?;
                let mut er = DynamicRow {
                    id: entity_id,
                    values: Vec::new(),
                };
                let mut lr = DynamicRow {
                    id: entity_id,
                    values: Vec::new(),
                };
                let mut tags = Vec::new();
                for (col_def, val) in columns.iter().zip(values.into_iter()) {
                    match col_def.target_table {
                        TargetTable::FileEntities => er.values.push(val),
                        TargetTable::Locations => lr.values.push(val),
                        TargetTable::FileTags => {
                            if let Some(s) = val.into_string() {
                                if !s.is_empty() {
                                    tags.push(TagRow {
                                        entity_id,
                                        tag_type: col_def.name.clone(),
                                        tag_value: s,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(TaggingResult {
                    entity_row: er,
                    location_row: lr,
                    tags,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let functions = self.registry.all_functions();
        let moved_rows = moved
            .into_iter()
            .map(|(eid, path_str)| {
                let p = Path::new(&path_str);
                let mut values = Vec::new();
                for func in functions {
                    for _ in func.tagger().get_columns() {
                        if func.role() == ScanRole::Location {
                            values.push(
                                func.generate_from_path(p).unwrap_or(TagValue::Null),
                            );
                        }
                    }
                }
                DynamicRow { id: eid, values }
            })
            .collect();
        Ok((results, moved_rows))
    }

    fn merge_phase(
        &self,
        results: Vec<TaggingResult>,
        moved: Vec<DynamicRow>,
        deleted_ids: Vec<i64>,
        unchanged_ids: Vec<i64>,
    ) -> Result<()> {
        let all_cols = self.registry.get_all_columns();
        let sql_ents = self
            .build_table_schema(
                TargetTable::FileEntities,
                Alias::new("temp_file_entities"),
                &all_cols,
            )
            .to_string(PostgresQueryBuilder);
        let sql_locs = self
            .build_table_schema(
                TargetTable::Locations,
                Alias::new("temp_locations"),
                &all_cols,
            )
            .to_string(PostgresQueryBuilder);
        let sql_tags = self
            .build_table_schema(TargetTable::FileTags, Tbl::TempFileTags, &all_cols)
            .to_string(PostgresQueryBuilder);

        self.conn
            .execute_batch(&format!("{};{};{}", sql_ents, sql_locs, sql_tags))?;

        {
            let mut app_ent = self.conn.appender("temp_file_entities")?;
            let mut app_loc = self.conn.appender("temp_locations")?;
            let mut app_tag = self.conn.appender("temp_file_tags")?;
            for res in results {
                let mut er = vec![&res.entity_row.id as &dyn ToSql];
                er.extend(res.entity_row.values.iter().map(|v| v as &dyn ToSql));
                app_ent.append_row(er.as_slice())?;

                let mut lr = vec![&res.location_row.id as &dyn ToSql];
                lr.extend(res.location_row.values.iter().map(|v| v as &dyn ToSql));
                app_loc.append_row(lr.as_slice())?;

                for t in res.tags {
                    app_tag.append_row([
                        &t.entity_id as &dyn ToSql,
                        &t.tag_type,
                        &t.tag_value,
                    ])?;
                }
            }
            for row in moved {
                let mut lr = vec![&row.id as &dyn ToSql];
                lr.extend(row.values.iter().map(|v| v as &dyn ToSql));
                app_loc.append_row(lr.as_slice())?;
            }
        }

        self.store.merge_and_save(
            &self.store.file_entities_path(),
            Alias::new("temp_file_entities"),
            (!deleted_ids.is_empty()).then(|| {
                Condition::all().add(Expr::col(Col::Id).is_not_in(deleted_ids.clone()))
            }),
        )?;
        self.store.merge_and_save(
            &self.store.locations_path(),
            Alias::new("temp_locations"),
            if unchanged_ids.is_empty() {
                Some(Condition::all().add(Expr::val(1).eq(0)))
            } else {
                Some(Condition::all().add(Expr::col(Col::EntityId).is_in(unchanged_ids)))
            },
        )?;
        self.store.merge_and_save(
            &self.store.file_tags_path(),
            Tbl::TempFileTags,
            (!deleted_ids.is_empty()).then(|| {
                Condition::all().add(Expr::col(Col::EntityId).is_not_in(deleted_ids))
            }),
        )?;

        let drop_ents = Table::drop()
            .table(Alias::new("temp_file_entities"))
            .to_string(PostgresQueryBuilder);
        let drop_locs = Table::drop()
            .table(Alias::new("temp_locations"))
            .to_string(PostgresQueryBuilder);
        let drop_tags = Table::drop()
            .table(Tbl::TempFileTags)
            .to_string(PostgresQueryBuilder);
        self.conn
            .execute_batch(&format!("{};{};{}", drop_ents, drop_locs, drop_tags))?;
        std::fs::remove_file(self.store.temp_scan_path()).ok();
        Ok(())
    }

    /// システムItem（type, label, typedtag）を一括登録し、
    /// 'origin: system' タグを付与します。
    pub fn update_system_items(&self) -> Result<()> {
        let items_path = self.store.item_entities_path();
        let item_tags_path = self.store.item_tags_path();
        let items_path_str = items_path.to_string_lossy();
        let item_tags_path_str = item_tags_path.to_string_lossy();

        // 1. 新規登録が必要なシステムItemを特定
        // all_tagsビューから、現在のタグの種類と値を抽出し、未登録のものをリストアップします。
        self.conn.execute_batch(&format!(r#"
            CREATE TEMP TABLE candidates AS
            WITH ut AS (SELECT DISTINCT type, value FROM all_tags)
            SELECT 'type' as kind, type as content FROM ut
            UNION
            SELECT 'label' as kind, value as content FROM ut
            UNION
            SELECT 'typedtag' as kind, type || ':' || value as content FROM ut;
            
            CREATE TEMP TABLE new_items_raw AS
            SELECT DISTINCT c.kind, c.content
            FROM candidates c
            LEFT JOIN read_parquet('{}') e 
              ON c.kind = e.kind AND c.content = e.content
            WHERE e.id IS NULL;
        "#, items_path_str))?;

        let new_count: i64 = self.conn.query_row("SELECT COUNT(*) FROM new_items_raw", [], |r| r.get(0))?;
        if new_count == 0 {
            self.conn.execute_batch("DROP TABLE candidates; DROP TABLE new_items_raw;")?;
            return Ok(());
        }

        // 2. IDの割り当てと保存
        // 既存の最小ID（負の値）を取得し、そこからさらにデクリメントして新しいIDを生成します。
        let min_id: i64 = self.conn.query_row(&format!("SELECT COALESCE(MIN(id), 0) FROM read_parquet('{}')", items_path_str), [], |r| r.get(0))?;
        let start_id = if min_id > -1 { -1 } else { min_id - 1 };

        let tmp_items = format!("{}.tmp", items_path_str);
        let tmp_item_tags = format!("{}.tmp", item_tags_path_str);

        self.conn.execute_batch(&format!(r#"
            CREATE TEMP TABLE new_items_with_id AS
            SELECT 
                {} - (row_number() OVER () - 1) as id,
                kind,
                content
            FROM new_items_raw;
            
            -- item_entitiesを更新
            COPY (
                SELECT * FROM read_parquet('{}')
                UNION ALL
                SELECT * FROM new_items_with_id
            ) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd');
            
            -- origin:system タグを付与し、item_tagsを更新
            COPY (
                SELECT * FROM read_parquet('{}')
                UNION ALL
                SELECT 
                    id as item_id,
                    'origin' as tag_type,
                    'system' as tag_value
                FROM new_items_with_id
            ) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd');
        "#, start_id, items_path_str, tmp_items, item_tags_path_str, tmp_item_tags))?;

        // 3. ファイルを原子的に置き換え
        std::fs::rename(&tmp_items, &items_path)?;
        std::fs::rename(&tmp_item_tags, &item_tags_path)?;

        // クリーンアップ
        self.conn.execute_batch("DROP TABLE candidates; DROP TABLE new_items_raw; DROP TABLE new_items_with_id;")?;

        Ok(())
    }

    fn build_table_schema(
        &self,
        target: TargetTable,
        name: impl Iden + 'static,
        columns: &[ColumnDef],
    ) -> sea_query::TableCreateStatement {
        let mut create = Table::create().table(name).to_owned();
        match target {
            TargetTable::FileEntities => {
                create.col(SeaColumnDef::new(Col::Id).big_integer());
                for c in columns
                    .iter()
                    .filter(|c| c.target_table == TargetTable::FileEntities)
                {
                    let mut def = SeaColumnDef::new(Alias::new(&c.name));
                    match c.sql_type {
                        "BIGINT" => def.big_integer(),
                        "BOOLEAN" => def.boolean(),
                        _ => def.string(),
                    };
                    create.col(&mut def);
                }
            }
            TargetTable::Locations => {
                create.col(SeaColumnDef::new(Col::EntityId).big_integer());
                for c in columns
                    .iter()
                    .filter(|c| c.target_table == TargetTable::Locations)
                {
                    let mut def = SeaColumnDef::new(Alias::new(&c.name));
                    match c.sql_type {
                        "BIGINT" => def.big_integer(),
                        "BOOLEAN" => def.boolean(),
                        _ => def.string(),
                    };
                    create.col(&mut def);
                }
            }
            TargetTable::FileTags => {
                create
                    .col(SeaColumnDef::new(Col::EntityId).big_integer())
                    .col(SeaColumnDef::new(Col::TagType).string())
                    .col(SeaColumnDef::new(Col::TagValue).string());
            }
            TargetTable::ItemEntities => {
                create
                    .col(SeaColumnDef::new(Col::Id).big_integer())
                    .col(SeaColumnDef::new(Col::Kind).string())
                    .col(SeaColumnDef::new(Col::Content).string());
            }
            TargetTable::ItemTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::TagType).string())
                    .col(SeaColumnDef::new(Col::TagValue).string());
            }
        }
        create
    }
}

struct IndexDiff {
    pub to_tag: Vec<ScanEntry>,
    pub moved: Vec<(i64, String)>,
    pub deleted_ids: Vec<i64>,
    pub unchanged_ids: Vec<i64>,
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
        
            #[test]
            fn test_system_items_registration() {
                let dir = tempdir().unwrap();
                let root = dir.path();
                let db_dir = root.join(".ttfm/db");
                
                // 拡張子 .txt のファイルを作成
                std::fs::write(root.join("hello.txt"), "hello").unwrap();
                
                let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
                let indexer = Indexer::new(&fm.conn, &fm.registry, db_dir);
                indexer.run(root, None::<&fn(usize)>, false).unwrap();
        
                        // 1. item_entities に extension:txt 関連のItemがあるか確認
        
                        let items_path_buf = fm.item_entities_path();
        
                        let items_path = items_path_buf.to_string_lossy();
        
                        let query = format!(
        
                            "SELECT kind, content FROM read_parquet('{}') WHERE content IN ('extension', 'txt', 'extension:txt')",
        
                            items_path
        
                        );
        
                        let mut stmt = fm.conn.prepare(&query).unwrap();
        
                        let rows: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        
                            .map(|r| r.unwrap()).collect();
        
                        
        
                        assert!(rows.iter().any(|(k, c)| k == "type" && c == "extension"));
        
                        assert!(rows.iter().any(|(k, c)| k == "label" && c == "txt"));
        
                        assert!(rows.iter().any(|(k, c)| k == "typedtag" && c == "extension:txt"));
        
                
        
                        // 2. それらのItemに origin:system タグがあるか確認
        
                        let item_tags_path_buf = fm.item_tags_path();
        
                        let item_tags_path = item_tags_path_buf.to_string_lossy();
        
                        let query_tags = format!(
        
                            r#"
        
                            SELECT COUNT(*) 
        
                            FROM read_parquet('{}') it
        
                            JOIN read_parquet('{}') ie ON it.item_id = ie.id
        
                            WHERE ie.content = 'extension:txt' AND it.tag_type = 'origin' AND it.tag_value = 'system'
        
                            "#,
        
                            item_tags_path, items_path
        
                        );
        
                
                let count: i64 = fm.conn.query_row(&query_tags, [], |r| r.get(0)).unwrap();
                assert_eq!(count, 1, "origin:system tag should be attached to system items");
            }
        }
        