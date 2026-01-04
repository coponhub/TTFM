use crate::taggers::{TagValue, TargetTable, ColumnDef};
use crate::FunctionRegistry;
use crate::functions::{ScanEntry, ScanRole};
use crate::db::{Tbl, Col, DuckDbFunc, SystemRank};
use anyhow::Result;
use duckdb::{Connection, ToSql};
use sea_query::{
    Query, Expr, Alias, Condition, JoinType, PostgresQueryBuilder, 
    Func, Table, ColumnDef as SeaColumnDef, Iden, SelectStatement,
    CaseStatement
};
use std::path::{Path, PathBuf};
use rayon::prelude::*;

// ========================================================
// 1. Storage Manager (IndexStore)
// ========================================================

pub(crate) struct IndexStore<'a> {
    conn: &'a Connection,
    db_dir: PathBuf,
}

impl<'a> IndexStore<'a> {
    pub(crate) fn new(conn: &'a Connection, db_dir: PathBuf) -> Self {
        Self { conn, db_dir }
    }

    pub(crate) fn file_entities_path(&self) -> PathBuf { self.db_dir.join("entities.parquet") }
    pub(crate) fn locations_path(&self) -> PathBuf { self.db_dir.join("locations.parquet") }
    pub(crate) fn base_tags_path(&self) -> PathBuf { self.db_dir.join("base_tags.parquet") }
    pub(crate) fn item_entities_path(&self) -> PathBuf { self.db_dir.join("items.parquet") }
    pub(crate) fn system_tags_path(&self) -> PathBuf { self.db_dir.join("system_tags.parquet") }
    pub(crate) fn user_tags_path(&self) -> PathBuf { self.db_dir.join("user_tags.parquet") }
    pub(crate) fn temp_scan_path(&self) -> PathBuf { self.db_dir.join("current_scan.parquet") }

    pub(crate) fn save_parquet(&self, query: SelectStatement, path: &Path) -> Result<()> {
        let sql = query.to_string(PostgresQueryBuilder);
        let path_str = path.to_string_lossy();
        let tmp_path = format!("{}.tmp", path_str);
        
        // COPY (SELECT ...) TO 'path' ...
        let copy_sql = format!(
            "COPY ({}) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')",
            sql, tmp_path
        );
        self.conn.execute(&copy_sql, [])?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    fn iden_to_sql(&self, iden: impl Iden + 'static) -> String {
        let sql = Query::select().column(iden).from(Alias::new("x")).to_string(PostgresQueryBuilder);
        sql.split_whitespace().nth(1).unwrap_or("").to_string()
    }

    pub(crate) fn write_parquet(&self, table_name: impl Iden + 'static, path: &Path) -> Result<()> {
        let query = Query::select()
            .expr(Expr::cust("*"))
            .from(table_name)
            .to_owned();
        self.save_parquet(query, path)
    }

    pub(crate) fn create_table_as(&self, table_name: impl Iden + 'static, query: SelectStatement) -> Result<()> {
        let quoted_name = self.iden_to_sql(table_name);

        let sql = format!(
            "CREATE TABLE {} AS {}",
            quoted_name,
            query.to_string(PostgresQueryBuilder)
        );
        self.conn.execute(&sql, [])?;
        Ok(())
    }

    pub(crate) fn create_temp_table_as(&self, table_name: impl Iden + 'static, query: SelectStatement) -> Result<()> {
        let quoted_name = self.iden_to_sql(table_name);

        let sql = format!(
            "CREATE TEMP TABLE {} AS {}",
            quoted_name,
            query.to_string(PostgresQueryBuilder)
        );
        self.conn.execute(&sql, [])?;
        Ok(())
    }

    pub(crate) fn drop_table(&self, table_name: impl Iden + 'static) -> Result<()> {
        let sql = Table::drop().table(table_name).to_string(PostgresQueryBuilder);
        self.conn.execute(&sql, [])?;
        Ok(())
    }

    pub(crate) fn merge_and_save(
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

    pub(crate) fn create_or_replace_view(&self, name: impl Iden + 'static, select: SelectStatement) -> Result<()> {
        let query_sql = select.to_string(PostgresQueryBuilder);
        let quoted_name = self.iden_to_sql(name);

        let sql = format!(
            "CREATE OR REPLACE VIEW {} AS {}", 
            quoted_name, 
            query_sql
        );
        self.conn.execute(&sql, [])?;
        Ok(())
    }
}

// ========================================================
// 2. Query Builder (QueryHelper)
// ========================================================

pub(crate) struct QueryHelper;

impl QueryHelper {
    pub(crate) fn parquet_query(path: &str) -> SelectStatement {
        Query::select()
            .expr(Expr::cust("*"))
            .from_function(
                Func::cust(DuckDbFunc::ReadParquet).arg(Expr::val(path)),
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
            .column((Tbl::EntAlias, Col::ItemId))
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
                Expr::col((Tbl::EntAlias, Col::ItemId))
                    .eq(Expr::col((Tbl::LocAlias, Col::ItemId))),
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
            .column((Tbl::EntAlias, Col::ItemId))
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
            .column((Tbl::EntAlias, Col::ItemId))
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
                Expr::col((Tbl::EntAlias, Col::ItemId))
                    .eq(Expr::col((Tbl::LocAlias, Col::ItemId))),
            )
            .and_where(
                Expr::col((Tbl::LocAlias, Col::Path)).eq(Expr::col((Tbl::ScanAlias, col_path))),
            )
            .cond_where(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .to_owned()
    }

    fn build_all_tags_view_query(
        all_columns: &[ColumnDef],
        ents: &str,
        base_tags: &str,
        locs: &str,
        items: &str,
        system_tags: &str,
        user_tags: &str,
    ) -> SelectStatement {
        // --- 1. Unified Master Info (ID, Rank, Name, ItemKind) ---
        
        // Base info from files
        let mut file_master = Query::select();
        file_master
            .column((Tbl::EntAlias, Col::ItemId))
            .column((Tbl::EntAlias, Col::Rank))
            .expr_as(Expr::val("file"), Col::ItemKind)
            .expr_as(Expr::col((Tbl::LocAlias, Col::Filename)), Col::Name)
            .from_subquery(Self::parquet_query(ents), Tbl::EntAlias)
            .join_subquery(
                JoinType::InnerJoin,
                Self::parquet_query(locs),
                Tbl::LocAlias,
                Expr::col((Tbl::EntAlias, Col::ItemId)).eq(Expr::col((Tbl::LocAlias, Col::ItemId)))
            );

        // Base info from other items
        let mut item_master = Query::select();
        item_master
            .column(Col::ItemId)
            .column(Col::Rank)
            .column(Col::ItemKind)
            .expr_as(Expr::col(Col::Content), Col::Name)
            .from_subquery(Self::parquet_query(items), Alias::new("i_all"));

        let mut all_master_base = file_master;
        all_master_base.union(sea_query::UnionType::All, item_master.to_owned());

        // Name override from user tags
        let mut user_names = Query::select();
        user_names
            .column(Col::ItemId)
            .expr_as(Expr::col(Col::Label), Col::Name)
            .from_subquery(Self::parquet_query(user_tags), Tbl::TagAlias)
            .and_where(Expr::col(Col::Type).eq("name"));

        let mut final_master = Query::select();
        final_master
            .expr_as(Expr::col((Alias::new("m"), Col::ItemId)), Col::ItemId)
            .column((Alias::new("m"), Col::Rank))
            .column((Alias::new("m"), Col::ItemKind))
            .expr_as(
                Func::cust(DuckDbFunc::Coalesce).args([
                    Expr::col((Alias::new("u"), Col::Name)).into(),
                    Expr::col((Alias::new("m"), Col::Name)).into(),
                ]),
                Col::Name
            )
            .from_subquery(all_master_base, Alias::new("m"))
            .join_subquery(
                JoinType::LeftJoin,
                user_names,
                Alias::new("u"),
                Expr::col((Alias::new("m"), Col::ItemId)).eq(Expr::col((Alias::new("u"), Col::ItemId)))
            );

        // --- 2. Unified Tag Sources (item_id, origin, type, label) ---
        
        let mut base_q = Query::select();
        
        // A. base_tags
        base_q.column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(Self::parquet_query(base_tags), Tbl::TagAlias);

        // B. locations
        for cd in all_columns
            .iter()
            .filter(|c| c.target_table == TargetTable::Locations)
        {
            let mut sub = Query::select();
            sub.column(Col::ItemId)
                .expr_as(Expr::val("system"), Col::Origin)
                .expr_as(Expr::val(cd.name.clone()), Col::Type)
                .expr_as(
                    Expr::col((Tbl::LocAlias, Alias::new(&cd.name)))
                        .cast_as(Alias::new("VARCHAR")),
                    Col::Label,
                )
                .from_subquery(Self::parquet_query(locs), Tbl::LocAlias);
            base_q.union(sea_query::UnionType::All, sub.to_owned());
        }

        // C. item_entities (Generic/Self Tags)
        // 1. content tag (for all items)
        let mut items_content = Query::select();
        items_content
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .expr_as(Expr::val("content"), Col::Type)
            .expr_as(Expr::col(Col::Content), Col::Label)
            .from_subquery(Self::parquet_query(items), Alias::new("it2"));
        base_q.union(sea_query::UnionType::All, items_content.to_owned());

        // 2. type: type tag (for item_kind = 'type')
        // All type definitions belong to the 'type' category.
        let mut items_type_tag = Query::select();
        items_type_tag
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .expr_as(Expr::val("type"), Col::Type)
            .expr_as(Expr::val("type"), Col::Label)
            .from_subquery(Self::parquet_query(items), Alias::new("it3"))
            .and_where(Expr::col(Col::ItemKind).eq("type"));
        base_q.union(sea_query::UnionType::All, items_type_tag.to_owned());

        // 3. Virtual tags for typedtags (e.g., content 'extension:rs' -> type:extension and extension:rs)
        // type: [prefix]
        let mut items_tt_type = Query::select();
        items_tt_type
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .expr_as(Expr::val("type"), Col::Type)
            .expr_as(
                Expr::cust_with_exprs("split_part($1, ':', 1)", [Expr::col(Col::Content).into()]),
                Col::Label
            )
            .from_subquery(Self::parquet_query(items), Alias::new("it4"))
            .and_where(Expr::col(Col::ItemKind).eq("typedtag"));
        base_q.union(sea_query::UnionType::All, items_tt_type.to_owned());

        // [prefix]: [suffix]
        let mut items_tt_val = Query::select();
        items_tt_val
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .expr_as(
                Expr::cust_with_exprs("split_part($1, ':', 1)", [Expr::col(Col::Content).into()]),
                Col::Type
            )
            .expr_as(
                Expr::cust_with_exprs("split_part($1, ':', 2)", [Expr::col(Col::Content).into()]),
                Col::Label
            )
            .from_subquery(Self::parquet_query(items), Alias::new("it5"))
            .and_where(Expr::col(Col::ItemKind).eq("typedtag"));
        base_q.union(sea_query::UnionType::All, items_tt_val.to_owned());

        // D. system_tags
        let mut stags = Query::select();
        stags
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(Self::parquet_query(system_tags), Tbl::TagAlias);
        base_q.union(sea_query::UnionType::All, stags.to_owned());

        // E. user_tags
        let mut utags = Query::select();
        utags
            .column(Col::ItemId)
            .expr_as(Expr::val("user"), Col::Origin)
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(Self::parquet_query(user_tags), Tbl::TagAlias);
        base_q.union(sea_query::UnionType::All, utags.to_owned());

        // --- 3. Final Assembly (Assemble Tags with Master Info) ---

        Query::select()
            .column((Tbl::TagAlias, Col::ItemId))
            .column((Alias::new("m"), Col::ItemKind))
            .column((Alias::new("m"), Col::Rank))
            .column((Tbl::TagAlias, Col::Origin))
            .column((Tbl::TagAlias, Col::Type))
            .column((Tbl::TagAlias, Col::Label))
            .column((Alias::new("m"), Col::Name))
            .from_subquery(base_q, Tbl::TagAlias)
            .join_subquery(
                JoinType::InnerJoin,
                final_master,
                Alias::new("m"),
                Expr::col((Tbl::TagAlias, Col::ItemId)).eq(Expr::col((Alias::new("m"), Col::ItemId)))
            )
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
        
        // ビューを最新の Parquet ファイルに基づいて再構築
        self.initialize_tables()?;

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
            &self.store.base_tags_path(),
            TargetTable::BaseTags,
            &all_cols,
        )?;
        self.ensure_empty_parquet_if_missing(
            &self.store.item_entities_path(),
            TargetTable::ItemEntities,
            &all_cols,
        )?;
        self.ensure_empty_parquet_if_missing(
            &self.store.system_tags_path(),
            TargetTable::SystemTags,
            &all_cols,
        )?;
        self.ensure_empty_parquet_if_missing(
            &self.store.user_tags_path(),
            TargetTable::UserTags,
            &all_cols,
        )?;

        let q = QueryHelper::build_all_tags_view_query(
            &all_cols,
            &self.store.file_entities_path().to_string_lossy(),
            &self.store.base_tags_path().to_string_lossy(),
            &self.store.locations_path().to_string_lossy(),
            &self.store.item_entities_path().to_string_lossy(),
            &self.store.system_tags_path().to_string_lossy(),
            &self.store.user_tags_path().to_string_lossy(),
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
        self.store.write_parquet(Alias::new(&table_name), path)?;
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
                    Func::cust(DuckDbFunc::Coalesce)
                        .args([Expr::col(Col::ItemId).max(), Expr::val(0).into()]),
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
                let item_id = max_id + (i as i64) + 1;
                let values = self.registry.process_file(Path::new(&entry.path.value))?;
                let mut er = DynamicRow {
                    id: item_id,
                    values: vec![TagValue::BigInt(0)],
                };
                let mut lr = DynamicRow {
                    id: item_id,
                    values: Vec::new(),
                };
                let mut tags = Vec::new();
                for (col_def, val) in columns.iter().zip(values.into_iter()) {
                    match col_def.target_table {
                        TargetTable::FileEntities => er.values.push(val),
                        TargetTable::Locations => lr.values.push(val),
                        TargetTable::BaseTags => {
                            if let Some(s) = val.into_string() {
                                if !s.is_empty() {
                                    tags.push(TagRow {
                                        item_id,
                                        tag_type: col_def.name.clone(),
                                        label: s,
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
        let has_updates = !results.is_empty() || !moved.is_empty();
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
            .build_table_schema(TargetTable::BaseTags, Tbl::TempBaseTags, &all_cols)
            .to_string(PostgresQueryBuilder);

        self.conn
            .execute_batch(&format!("{};{};{}", sql_ents, sql_locs, sql_tags))?;

        {
            let mut app_ent = self.conn.appender("temp_file_entities")?;
            let mut app_loc = self.conn.appender("temp_locations")?;
            let mut app_tag = self.conn.appender("temp_base_tags")?;
            for res in results {
                let mut er = vec![&res.entity_row.id as &dyn ToSql];
                er.extend(res.entity_row.values.iter().map(|v| v as &dyn ToSql));
                app_ent.append_row(er.as_slice())?;

                let mut lr = vec![&res.location_row.id as &dyn ToSql];
                lr.extend(res.location_row.values.iter().map(|v| v as &dyn ToSql));
                app_loc.append_row(lr.as_slice())?;

                for t in res.tags {
                    app_tag.append_row([
                        &t.item_id as &dyn ToSql,
                        &t.tag_type,
                        &t.label,
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
                Condition::all().add(Expr::col(Col::ItemId).is_not_in(deleted_ids.clone()))
            }),
        )?;
        self.store.merge_and_save(
            &self.store.locations_path(),
            Alias::new("temp_locations"),
            if unchanged_ids.is_empty() {
                Some(Condition::all().add(Expr::val(1).eq(0)))
            } else {
                Some(Condition::all().add(Expr::col(Col::ItemId).is_in(unchanged_ids)))
            },
        )?;
        self.store.merge_and_save(
            &self.store.base_tags_path(),
            Tbl::TempBaseTags,
            (!deleted_ids.is_empty()).then(|| {
                Condition::all().add(Expr::col(Col::ItemId).is_not_in(deleted_ids))
            }),
        )?;

        // 更新がある場合のみシステムItemを登録
        if has_updates {
            self.update_system_items()?;
        }

        let drop_ents = Table::drop()
            .table(Alias::new("temp_file_entities"))
            .to_string(PostgresQueryBuilder);
        let drop_locs = Table::drop()
            .table(Alias::new("temp_locations"))
            .to_string(PostgresQueryBuilder);
        let drop_tags = Table::drop()
            .table(Tbl::TempBaseTags)
            .to_string(PostgresQueryBuilder);
        self.conn
            .execute_batch(&format!("{};{};{}", drop_ents, drop_locs, drop_tags))?;
        std::fs::remove_file(self.store.temp_scan_path()).ok();
        Ok(())
    }

    /// 今回の更新で発生したタグ情報（テンポラリテーブル）を元に、
    /// システムItemを一括登録します。
    pub fn update_system_items(&self) -> Result<()> {
        let items_path = self.store.item_entities_path();
        let system_tags_path = self.store.system_tags_path();
        let items_str_buf = items_path.to_string_lossy();
        let stags_str_buf = system_tags_path.to_string_lossy();
        let items_str = items_str_buf.as_ref();
        let stags_str = stags_str_buf.as_ref();

        // 1. 今回の更新分（temp_base_tags, temp_locations）からタグ情報を抽出
        let mut source_q = Query::select();
        source_q
            .expr_as(Expr::col(Col::Type), Col::Type)
            .expr_as(Expr::col(Col::Label), Col::Label)
            .from(Tbl::TempBaseTags);

        let all_cols = self.registry.get_all_columns();
        for col in all_cols.iter().filter(|c| {
            c.target_table == crate::taggers::TargetTable::Locations
        }) {
            let mut sub = Query::select();
            sub.expr_as(Expr::val(col.name.clone()), Alias::new("type"))
                .expr_as(
                    Expr::col(Alias::new(&col.name))
                        .cast_as(Alias::new("VARCHAR")),
                    Alias::new("label"),
                )
                .from(Alias::new("temp_locations"));
            source_q.union(sea_query::UnionType::Distinct, sub.to_owned());
        }

        // 2. 登録候補 (item_kind, content) の生成
        let mut cand_q = Query::select();
        cand_q
            .expr_as(Expr::val("type"), Col::ItemKind)
            .expr_as(Expr::col(Col::Type), Col::Content)
            .from_subquery(source_q.to_owned(), Alias::new("st"));

        let mut label_q = Query::select();
        label_q
            .expr_as(Expr::val("label"), Col::ItemKind)
            .expr_as(Expr::col(Col::Label), Col::Content)
            .from_subquery(source_q.to_owned(), Alias::new("st"));
        cand_q.union(sea_query::UnionType::Distinct, label_q.to_owned());

        let mut tt_q = Query::select();
        tt_q.expr_as(Expr::val("typedtag"), Col::ItemKind)
            .expr_as(
                Expr::cust_with_exprs("$1 || ':' || $2", [
                    Expr::col(Col::Type).into(),
                    Expr::col(Col::Label).into(),
                ]),
                Col::Content,
            )
            .from_subquery(source_q.to_owned(), Alias::new("st"));
        cand_q.union(sea_query::UnionType::Distinct, tt_q.to_owned());

        // 3. 未登録のものを特定
        let mut new_items_q = Query::select();
        new_items_q
            .column((Alias::new("c"), Col::ItemKind))
            .column((Alias::new("c"), Col::Content))
            .distinct()
            .from_subquery(cand_q, Alias::new("c"))
            .join_subquery(
                JoinType::LeftJoin,
                QueryHelper::parquet_query(items_str),
                Tbl::EntAlias,
                Condition::all()
                    .add(Expr::col((Alias::new("c"), Col::ItemKind))
                        .eq(Expr::col((Tbl::EntAlias, Col::ItemKind))))
                    .add(Expr::col((Alias::new("c"), Col::Content))
                        .eq(Expr::col((Tbl::EntAlias, Col::Content)))),
            )
            .and_where(Expr::col((Tbl::EntAlias, Col::ItemId)).is_null());

        self.store.create_temp_table_as(Alias::new("new_items_raw"), new_items_q)?;

        let query_count = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from(Alias::new("new_items_raw"))
            .to_string(PostgresQueryBuilder);
        let new_count: i64 = self.conn.query_row(
            &query_count, [], |r| r.get(0)
        )?;
        if new_count == 0 {
            self.store.drop_table(Alias::new("new_items_raw"))?;
            return Ok(());
        }

        // 4. IDの割り当てと保存
        let query_min = Query::select()
            .expr(
                Func::cust(DuckDbFunc::Coalesce)
                    .args([Expr::col(Col::ItemId).min(), Expr::val(0).into()]),
            )
            .from_subquery(QueryHelper::parquet_query(items_str), Tbl::EntAlias)
            .to_string(PostgresQueryBuilder);
        let min_id: i64 = self.conn.query_row(&query_min, [], |r| r.get(0))?;
        let start_id = if min_id > -1 { -1 } else { min_id - 1 };

        let tmp_items = format!("{}.tmp", items_str);
        let tmp_stags = format!("{}.tmp", stags_str);

        let inner_case = CaseStatement::new()
            .case(Expr::col(Col::Content).eq("name"), i64::from(SystemRank::Name))
            .case(Expr::col(Col::Content).eq("type_from_ext"), i64::from(SystemRank::TypeFromExt))
            .case(Expr::col(Col::Content).eq("size_str"), i64::from(SystemRank::SizeStr))
            .case(Expr::col(Col::Content).eq("modified_str"), i64::from(SystemRank::ModifiedStr))
            .case(Expr::col(Col::Content).eq("parentdir"), i64::from(SystemRank::ParentDir))
            .case(Expr::col(Col::Content).eq("kind"), i64::from(SystemRank::ItemKind))
            .case(Expr::col(Col::Content).eq("content"), i64::from(SystemRank::Content))
            .case(Expr::col(Col::Content).eq("filename"), i64::from(SystemRank::Filename))
            .finally(i64::from(SystemRank::Other));

        let rank_case = CaseStatement::new()
            .case(Expr::col(Col::ItemKind).eq("type"), inner_case)
            .finally(0);

        let new_items_id_q = Query::select()
            .expr_as(
                Expr::cust_with_exprs(
                    "$1 - (row_number() OVER () - 1)",
                    [Expr::val(start_id).into()]
                ),
                Col::ItemId,
            )
            .expr_as(rank_case, Col::Rank)
            .column(Col::ItemKind)
            .column(Col::Content)
            .from(Alias::new("new_items_raw"))
            .to_owned();

        self.store.create_temp_table_as(Alias::new("new_items_with_id"), new_items_id_q)?;

        // item_entities 更新
        let mut update_items_q = QueryHelper::parquet_query(items_str);
        update_items_q.union(
            sea_query::UnionType::All,
            Query::select()
                .expr(Expr::cust("*"))
                .from(Alias::new("new_items_with_id"))
                .to_owned()
        );
        self.store.save_parquet(update_items_q, Path::new(&tmp_items))?;

        // system_tags 更新
        let mut update_tags_q = QueryHelper::parquet_query(stags_str);
        let mut new_tags_q = Query::select();
        new_tags_q
            .column(Col::ItemId)
            .expr_as(Expr::val("origin"), Col::Type)
            .expr_as(Expr::val("system"), Col::Label)
            .from(Alias::new("new_items_with_id"));
        update_tags_q.union(sea_query::UnionType::All, new_tags_q.to_owned());
        self.store.save_parquet(update_tags_q, Path::new(&tmp_stags))?;

        std::fs::rename(&tmp_items, &items_path)?;
        std::fs::rename(&tmp_stags, &system_tags_path)?;

        self.store.drop_table(Alias::new("new_items_raw"))?;
        self.store.drop_table(Alias::new("new_items_with_id"))?;

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
                create.col(SeaColumnDef::new(Col::ItemId).big_integer());
                create.col(SeaColumnDef::new(Col::Rank).big_integer());
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
                create.col(SeaColumnDef::new(Col::ItemId).big_integer());
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
            TargetTable::BaseTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Type).string())
                    .col(SeaColumnDef::new(Col::Label).string());
            }
            TargetTable::ItemEntities => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Rank).big_integer())
                    .col(SeaColumnDef::new(Col::ItemKind).string())
                    .col(SeaColumnDef::new(Col::Content).string());
            }
            TargetTable::SystemTags | TargetTable::UserTags => {
                create
                    .col(SeaColumnDef::new(Col::ItemId).big_integer())
                    .col(SeaColumnDef::new(Col::Type).string())
                    .col(SeaColumnDef::new(Col::Label).string());
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
    pub item_id: i64,
    pub tag_type: String,
    pub label: String,
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
        assert!(db_dir.join("entities.parquet").exists());
        let query_count = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from(Alias::new("all_tags"))
            .to_string(PostgresQueryBuilder);
        let count: i64 = conn.query_row(&query_count, [], |r| r.get(0)).unwrap();
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
        let query = Query::select()
            .columns([Col::ItemKind, Col::Content])
            .from_subquery(QueryHelper::parquet_query(&items_path), Tbl::EntAlias)
            .and_where(Expr::col(Col::Content).is_in(["extension", "txt", "extension:txt"]))
            .to_string(PostgresQueryBuilder);

        let mut stmt = fm.conn.prepare(&query).unwrap();
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert!(rows.iter().any(|(k, c)| k == "type" && c == "extension"));
        assert!(rows.iter().any(|(k, c)| k == "label" && c == "txt"));
        assert!(rows.iter().any(|(k, c)| k == "typedtag" && c == "extension:txt"));

        // 2. それらのItemに origin:system タグがあるか確認
        let system_tags_path_buf = fm.system_tags_path();
        let system_tags_path = system_tags_path_buf.to_string_lossy();
        let query_tags = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from_subquery(QueryHelper::parquet_query(&system_tags_path), Tbl::TagAlias)
            .join_subquery(
                JoinType::InnerJoin,
                QueryHelper::parquet_query(&items_path),
                Tbl::EntAlias,
                Expr::col((Tbl::TagAlias, Col::ItemId)).eq(Expr::col((Tbl::EntAlias, Col::ItemId))),
            )
            .and_where(Expr::col((Tbl::EntAlias, Col::Content)).eq("extension:txt"))
            .and_where(Expr::col((Tbl::TagAlias, Col::Type)).eq("origin"))
            .and_where(Expr::col((Tbl::TagAlias, Col::Label)).eq("system"))
            .to_string(PostgresQueryBuilder);

        let count: i64 = fm.conn
            .query_row(&query_tags, [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "origin:system tag should be attached to system items"
        );
    }

    #[test]
    fn test_or_negation_complex_behavior() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let db_dir = root.join(".ttfm/db");

        // 1. ファイル準備 (.rs と .txt)
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("readme.txt"), "hello").unwrap();

        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        let indexer = Indexer::new(&fm.conn, &fm.registry, db_dir);
        indexer.run(root, None::<&fn(usize)>, false).unwrap();

        // 2. クエリ実行: item_kind:type | -extension:rs
        let query = "item_kind:type | -extension:rs";
        let results = fm.search(query).unwrap();

        let mut found_type_item = false;
        let mut found_txt_file = false;
        let mut found_rs_file = false;

        for r in results {
            if r.item_kind != "file" && r.item_kind == "type" {
                found_type_item = true;
            }
            if r.item_kind == "file" {
                if r.tags.iter().any(|(t, v)| t == "extension" && v == "txt") {
                    found_txt_file = true;
                }
                if r.tags.iter().any(|(t, v)| t == "extension" && v == "rs") {
                    found_rs_file = true;
                }
            }
        }

        assert!(found_type_item, "Should find system items");
        assert!(found_txt_file, "Should find readme.txt");
        assert!(!found_rs_file, "Should NOT find main.rs");
    }

    #[test]
    fn test_glob_search_behavior() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let db_dir = root.join(".ttfm/db");

        std::fs::write(root.join("project_alpha.pdf"), "").unwrap();
        std::fs::write(root.join("project_beta.txt"), "").unwrap();

        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        let indexer = Indexer::new(&fm.conn, &fm.registry, db_dir);
        indexer.run(root, None::<&fn(usize)>, false).unwrap();

        // 1. ワイルドカードによる部分一致
        let results = fm.search("filename:*alpha*").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].primary_value().unwrap().contains("alpha"));

        // 2. 複数のワイルドカード
        let results = fm.search("filename:project*").unwrap();
        assert_eq!(results.len(), 2);

        // 3. ワイルドカードなし (完全一致として動作)
        let results = fm.search("filename:project").unwrap();
        assert_eq!(results.len(), 0);

        let results = fm.search("filename:project_alpha.pdf").unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_all_tags_view_consistency() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

        // Noteを作成してタグを付ける
        let note_id = fm.add_item("note", "Consistency Test Memo").unwrap();
        fm.tag_item(&note_id.to_string(), "testtag:true").unwrap();

        // all_tags ビューを直接クエリして不整合をチェック
        // 同じIDなのに異なるNameまたは異なるRankを持つグループがあるか探す
        let sql = "
            SELECT item_id 
            FROM all_tags 
            GROUP BY item_id 
            HAVING COUNT(DISTINCT name) > 1 OR COUNT(DISTINCT rank) > 1
        ";
        
        let mut stmt = fm.conn.prepare(sql).unwrap();
        let inconsistent_ids: Vec<i64> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert!(
            inconsistent_ids.is_empty(), 
            "Inconsistency found in all_tags view for IDs: {:?}. Each item must have exactly one unique Name and Rank across all its tag rows.", 
            inconsistent_ids
        );
    }
}
                        