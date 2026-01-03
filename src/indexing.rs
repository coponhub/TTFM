use crate::taggers::{TagValue, TargetTable};
use crate::FunctionRegistry;
use crate::functions::{ScanEntry, ScanRole};
use crate::db::{Tbl, Col, DuckDbFunc as DBFunc};
use anyhow::{Result, Context};
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

/// Parquet ファイルの実体操作とパス管理を担当。
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

    /// SELECT 結果を Parquet ファイルへ書き出す。
    fn copy_to_parquet(&self, query: SelectStatement, path: &Path) -> Result<()> {
        let sql = query.to_string(PostgresQueryBuilder);
        let path_str = path.to_string_lossy();
        let tmp_path = format!("{}.tmp", path_str);
        
        let copy_sql = format!(
            "COPY ({}) TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", 
            sql, tmp_path
        );
        self.conn.execute(&copy_sql, [])
            .with_context(|| format!("Failed to export parquet to {}", tmp_path))?;
        
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("Failed to rename {} to {}", tmp_path, path_str))?;
        Ok(())
    }

    /// 既存の Parquet ファイルとテンポラリテーブルをマージして保存。
    fn merge_and_save(
        &self, 
        path: &Path, 
        temp_table: impl Iden + 'static, 
        filter: Option<Condition>
    ) -> Result<()> {
        let query = if path.exists() {
            let path_str = path.to_string_lossy().to_string();
            let mut base = QueryHelper::parquet_query(&path_str);
            if let Some(cond) = filter {
                base.cond_where(cond);
            }
            base.union(
                sea_query::UnionType::All, 
                Query::select().expr(Expr::cust("*")).from(temp_table).to_owned()
            ).to_owned()
        } else {
            Query::select().expr(Expr::cust("*")).from(temp_table).to_owned()
        };
        self.copy_to_parquet(query, path)
    }
}

// ========================================================
// 2. Query Builder (QueryHelper)
// ========================================================

/// sea-query を使った複雑な SQL 構築ロジックを集約。
struct QueryHelper;

impl QueryHelper {
    /// read_parquet 関数を用いた SELECT 文を生成。
    fn parquet_query(path: &str) -> SelectStatement {
         Query::select()
            .expr(Expr::cust("*"))
            .from_function(
                Func::cust(DBFunc::ReadParquet).arg(Expr::val(path)), 
                Tbl::OriginAlias
            )
            .to_owned()
    }

    /// ファイルの同一性（ScanId）判定条件。
    fn identity_condition(left: Tbl, right: Tbl) -> Condition {
        let mut cond = Condition::all();
        for col_def in ScanEntry::schema() {
            if matches!(col_def.role, ScanRole::ScanId) {
                let col = Alias::new(col_def.name);
                cond = cond.add(Expr::col((left, col.clone())).eq(Expr::col((right, col.clone()))));
            }
        }
        cond
    }

    /// ファイルの整合性（Integrity）判定条件。
    fn integrity_condition(left: Tbl, right: Tbl) -> Condition {
        let mut cond = Condition::all();
        for col_def in ScanEntry::schema() {
            if matches!(col_def.role, ScanRole::Integrity) {
                let col = Alias::new(col_def.name);
                cond = cond.add(Expr::col((left, col.clone())).eq(Expr::col((right, col.clone()))));
            }
        }
        cond
    }

    fn build_to_tag_query(scan_path: &str, entities_path: &str) -> SelectStatement {
        let schema = ScanEntry::schema();
        let col_aliases: Vec<Alias> = schema.iter().map(|c| Alias::new(c.name)).collect();

        let subquery_exists = Query::select()
            .expr(Expr::val(1))
            .from_subquery(Self::parquet_query(entities_path), Tbl::EntAlias)
            .cond_where(Self::identity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .cond_where(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .to_owned();

        Query::select()
            .columns(col_aliases.iter().map(|a| (Tbl::ScanAlias, a.clone())).collect::<Vec<_>>())
            .from_subquery(Self::parquet_query(scan_path), Tbl::ScanAlias)
            .and_where(Expr::exists(subquery_exists).not())
            .to_owned()
    }

    fn build_moved_query(scan_path: &str, entities_path: &str, locations_path: &str) -> SelectStatement {
        let schema = ScanEntry::schema();
        let col_path = Alias::new(schema[0].name);

        Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .column((Tbl::ScanAlias, col_path.clone()))
            .from_subquery(Self::parquet_query(entities_path), Tbl::EntAlias)
            .join_subquery(
                JoinType::InnerJoin, 
                Self::parquet_query(scan_path), 
                Tbl::ScanAlias, 
                Self::identity_condition(Tbl::EntAlias, Tbl::ScanAlias)
            )
            .join_subquery(
                JoinType::InnerJoin, 
                Self::parquet_query(locations_path), 
                Tbl::LocAlias, 
                Expr::col((Tbl::EntAlias, Col::Id)).eq(Expr::col((Tbl::LocAlias, Col::EntityId)))
            )
            .and_where(Expr::col((Tbl::LocAlias, Col::Path)).ne(Expr::col((Tbl::ScanAlias, col_path.clone()))))
            .cond_where(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            .to_owned()
    }

    fn build_deleted_query(scan_path: &str, entities_path: &str) -> SelectStatement {
        let col_path = Alias::new(ScanEntry::schema()[0].name);
        Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .from_subquery(Self::parquet_query(entities_path), Tbl::EntAlias)
            .join_subquery(
                JoinType::LeftJoin, 
                Self::parquet_query(scan_path), 
                Tbl::ScanAlias, 
                Condition::all()
                    .add(Self::identity_condition(Tbl::EntAlias, Tbl::ScanAlias))
                    .add(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias))
            )
            .and_where(Expr::col((Tbl::ScanAlias, col_path)).is_null())
            .to_owned()
    }

    fn build_unchanged_query(scan_path: &str, entities_path: &str, locations_path: &str) -> SelectStatement {
        let col_path = Alias::new(ScanEntry::schema()[0].name);
        Query::select()
            .column((Tbl::EntAlias, Col::Id))
            .from_subquery(Self::parquet_query(entities_path), Tbl::EntAlias)
            .join_subquery(
                JoinType::InnerJoin, 
                Self::parquet_query(scan_path), 
                Tbl::ScanAlias, 
                Self::identity_condition(Tbl::EntAlias, Tbl::ScanAlias)
            )
            .join_subquery(
                JoinType::InnerJoin, 
                Self::parquet_query(locations_path), 
                Tbl::LocAlias, 
                Expr::col((Tbl::EntAlias, Col::Id)).eq(Expr::col((Tbl::LocAlias, Col::EntityId)))
            )
            .and_where(Expr::col((Tbl::LocAlias, Col::Path)).eq(Expr::col((Tbl::ScanAlias, col_path))))
            .cond_where(Self::integrity_condition(Tbl::EntAlias, Tbl::ScanAlias))
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
    pub fn new(conn: &'a Connection, registry: &'a FunctionRegistry, db_dir: PathBuf) -> Self {
        Self { 
            conn, 
            registry, 
            store: IndexStore::new(conn, db_dir) 
        }
    }

    /// インデックス作成のメイン実行フロー。
    pub fn run<P, F>(&self, root_path: P, on_progress: Option<&F>, dry_run: bool) -> Result<usize>
    where
        P: AsRef<Path>,
        F: Fn(usize) + Sync + Send,
    {
        let count = self.scan_phase(root_path.as_ref(), on_progress, dry_run)?;
        if dry_run {
            return Ok(count);
        }

        let diff = self.diff_phase()?;
        let (tagging_results, moved_locations) = self.tagging_phase(diff.to_tag, diff.moved)?;
        self.merge_phase(tagging_results, moved_locations, diff.deleted_ids, diff.unchanged_ids)?;

        Ok(count)
    }

    /// 必要な Parquet ファイルとビューを初期化します。
    pub fn initialize_tables(&self) -> Result<()> {
        self.ensure_empty_parquet_if_missing(&self.store.file_entities_path(), TargetTable::FileEntities)?;
        self.ensure_empty_parquet_if_missing(&self.store.locations_path(), TargetTable::Locations)?;
        self.ensure_empty_parquet_if_missing(&self.store.file_tags_path(), TargetTable::FileTags)?;
        self.ensure_empty_parquet_if_missing(&self.store.item_entities_path(), TargetTable::ItemEntities)?;
        self.ensure_empty_parquet_if_missing(&self.store.item_tags_path(), TargetTable::ItemTags)?;

        // 検索用の統合ビュー (all_tags) を構築
        let loc_union_sql = self.registry.get_all_columns().into_iter()
            .filter(|c| c.target_table == TargetTable::Locations)
            .map(|c| format!(
                "SELECT entity_id AS target_id, 'file' AS target_kind, '{}' AS type, CAST({} AS VARCHAR) AS value FROM read_parquet('{}')",
                c.name, c.name, self.store.locations_path().to_string_lossy()
            ))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");

        let view_sql = format!(r#"
            CREATE OR REPLACE VIEW all_tags AS
            -- Tags from file_tags
            SELECT t.entity_id AS target_id, 'file' AS target_kind, t.tag_type AS type, t.tag_value AS value FROM read_parquet('{}') t
            UNION ALL 
            -- Attributes from locations
            {} 
            UNION ALL
            -- Item Entities
            SELECT id AS target_id, 'item' AS target_kind, 'itemtype' AS type, kind AS value FROM read_parquet('{}')
            UNION ALL SELECT id AS target_id, 'item' AS target_kind, 'content' AS type, content AS value FROM read_parquet('{}')
            UNION ALL 
            -- Tags from item_tags
            SELECT it.item_id AS target_id, 'item' AS target_kind, it.tag_type AS type, it.tag_value AS value FROM read_parquet('{}') it
        "#, 
        self.store.file_tags_path().to_string_lossy(), loc_union_sql,
        self.store.item_entities_path().to_string_lossy(), self.store.item_entities_path().to_string_lossy(),
        self.store.item_tags_path().to_string_lossy());
        
        self.conn.execute(&view_sql, [])
            .context("Failed to create unified view 'all_tags'")?;
        Ok(())
    }

    fn ensure_empty_parquet_if_missing(&self, path: &Path, target: TargetTable) -> Result<()> {
        if path.exists() { return Ok(()); }

        let table_name = format!("temp_init_{:?}", target);
        let create = self.build_table_schema(target, Alias::new(&table_name));
        self.conn.execute(&create.to_string(PostgresQueryBuilder), [])
            .context("Failed to create init table")?;
        
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        let copy_sql = format!("COPY {} TO '{}' (FORMAT 'parquet', COMPRESSION 'zstd')", table_name, path.to_string_lossy());
        self.conn.execute(&copy_sql, [])?;
        
        self.conn.execute(&Table::drop().table(Alias::new(&table_name)).to_string(PostgresQueryBuilder), []).ok();
        Ok(())
    }

    /// ディレクトリをスキャンして current_scan.parquet に一時保存します。
    fn scan_phase<F>(&self, root_path: &Path, on_progress: Option<&F>, dry_run: bool) -> Result<usize>
    where
        F: Fn(usize) + Sync + Send,
    {
        if !dry_run {
            let mut create_table = Table::create();
            create_table.table(Tbl::TempScan).if_not_exists();
            for col_def in ScanEntry::schema() {
                let mut col = SeaColumnDef::new(Alias::new(col_def.name));
                if col_def.sql_type == "BIGINT" { col.big_integer(); } else { col.string(); }
                create_table.col(&mut col);
            }
            self.conn.execute(&create_table.to_string(PostgresQueryBuilder), [])
                .context("Failed to create temp_scan table")?;
            self.conn.execute(&Query::delete().from_table(Tbl::TempScan).to_string(PostgresQueryBuilder), [])?;
        }

        let db_dir_canonical = self.store.db_dir.canonicalize().unwrap_or_else(|_| self.store.db_dir.clone());
        let (tx, rx) = std::sync::mpsc::channel::<ScanEntry>();
        let walker = ignore::WalkBuilder::new(root_path)
            .hidden(false)
            .git_ignore(true)
            .threads(rayon::current_num_threads())
            .build_parallel();

        let count = std::thread::scope(|s| {
            // スキャン実行スレッド
            s.spawn(move || {
                walker.run(|| {
                    let tx = tx.clone();
                    let db_dir_canonical = db_dir_canonical.clone();
                    Box::new(move |result| {
                        if let Ok(entry) = result {
                            if let Ok(path) = entry.path().canonicalize() {
                                if path.starts_with(&db_dir_canonical) { return ignore::WalkState::Continue; }
                            }
                            if let Ok(metadata) = entry.metadata() {
                                if let Ok(scan_entry) = ScanEntry::from_path_metadata(entry.path(), &metadata) {
                                    let _ = tx.send(scan_entry);
                                }
                            }
                        }
                        ignore::WalkState::Continue
                    })
                });
            });

            // 受信・DB書き込み
            let mut current_count = 0;
            let mut appender = if !dry_run { Some(self.conn.appender("temp_scan")?) } else { None };
            for entry in rx {
                if let Some(ref mut app) = appender {
                    app.append_row(&*entry.as_params())?;
                }
                current_count += 1;
                if let Some(cb) = on_progress {
                    if current_count % 1000 == 0 { cb(current_count); }
                }
            }
            Ok::<usize, anyhow::Error>(current_count)
        })?;

        if !dry_run {
            self.store.copy_to_parquet(
                Query::select().expr(Expr::cust("*")).from(Tbl::TempScan).to_owned(), 
                &self.store.temp_scan_path()
            )?;
            self.conn.execute(&Table::drop().table(Tbl::TempScan).to_string(PostgresQueryBuilder), []).ok();
        }

        if let Some(cb) = on_progress { cb(count); }
        Ok(count)
    }

    /// 既存のインデックスと current_scan を比較して差分を抽出します。
    fn diff_phase(&self) -> Result<IndexDiff> {
        let scan_path = self.store.temp_scan_path().to_string_lossy().to_string();
        let entities_path = self.store.file_entities_path().to_string_lossy().to_string();
        let locations_path = self.store.locations_path().to_string_lossy().to_string();

        if !self.store.file_entities_path().exists() {
            let col_aliases: Vec<Alias> = ScanEntry::schema().iter().map(|c| Alias::new(c.name)).collect();
            let query = Query::select()
                .columns(col_aliases)
                .from_subquery(QueryHelper::parquet_query(&scan_path), Tbl::ScanAlias)
                .to_string(PostgresQueryBuilder);

            let to_tag = self.conn.prepare(&query)?
                .query_map([], |row| ScanEntry::from_row(row))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            
            return Ok(IndexDiff { to_tag, moved: vec![], deleted_ids: vec![], unchanged_ids: vec![] });
        }

        // 各差分抽出クエリの実行
        let to_tag = self.conn.prepare(&QueryHelper::build_to_tag_query(&scan_path, &entities_path).to_string(PostgresQueryBuilder))?
            .query_map([], |row| ScanEntry::from_row(row))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let moved = self.conn.prepare(&QueryHelper::build_moved_query(&scan_path, &entities_path, &locations_path).to_string(PostgresQueryBuilder))?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let deleted_ids = self.conn.prepare(&QueryHelper::build_deleted_query(&scan_path, &entities_path).to_string(PostgresQueryBuilder))?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let unchanged_ids = self.conn.prepare(&QueryHelper::build_unchanged_query(&scan_path, &entities_path, &locations_path).to_string(PostgresQueryBuilder))?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(IndexDiff { to_tag, moved, deleted_ids, unchanged_ids })
    }

    /// 新規/変更ファイルのタグ付けと、移動したファイルの属性更新を行います。
    fn tagging_phase(&self, to_tag: Vec<ScanEntry>, moved: Vec<(i64, String)>) -> Result<(Vec<TaggingResult>, Vec<DynamicRow>)> {
        let columns = self.registry.get_all_columns();
        let max_id: i64 = if self.store.file_entities_path().exists() {
            let entities_str = self.store.file_entities_path().to_string_lossy().to_string();
            let query = Query::select()
                .expr(Func::cust(DBFunc::Coalesce).args([Expr::col(Col::Id).max(), Expr::val(0).into()]))
                .from_subquery(QueryHelper::parquet_query(&entities_str), Tbl::EntAlias)
                .to_string(PostgresQueryBuilder);
            self.conn.query_row(&query, [], |r| r.get(0))?
        } else { 0 };

        // 1. 新規/変更ファイルの処理（並列実行）
        let results = to_tag.into_par_iter().enumerate().map(|(i, entry)| {
            let entity_id = max_id + (i as i64) + 1;
            let values = self.registry.process_file(Path::new(&entry.path.value))?;
            let mut er = DynamicRow { id: entity_id, values: Vec::new() };
            let mut lr = DynamicRow { id: entity_id, values: Vec::new() };
            let mut tags = Vec::new();

            for (col_def, val) in columns.iter().zip(values.into_iter()) {
                match col_def.target_table {
                    TargetTable::FileEntities => er.values.push(val),
                    TargetTable::Locations => lr.values.push(val),
                    TargetTable::FileTags => {
                        if let Some(s) = val.to_string_lossy() {
                            if !s.is_empty() {
                                tags.push(TagRow { entity_id, tag_type: col_def.name.clone(), tag_value: s });
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(TaggingResult { entity_row: er, location_row: lr, tags })
        }).collect::<Result<Vec<_>>>()?;

        // 2. 移動ファイルの処理
        let functions = self.registry.all_functions();
        let moved_rows = moved.into_iter().map(|(eid, path_str)| {
            let p = Path::new(&path_str);
            let mut values = Vec::new();
            for func in functions {
                for _ in func.tagger().get_columns() {
                    if func.role() == ScanRole::Location {
                        values.push(func.generate_from_path(p).unwrap_or(TagValue::Null));
                    }
                }
            }
            DynamicRow { id: eid, values }
        }).collect();

        Ok((results, moved_rows))
    }

    /// 一時的な結果を最終的な Parquet ファイルに統合します。
    fn merge_phase(&self, results: Vec<TaggingResult>, moved: Vec<DynamicRow>, deleted_ids: Vec<i64>, unchanged_ids: Vec<i64>) -> Result<()> {
        let sql_ents = self.build_table_schema(TargetTable::FileEntities, Alias::new("temp_file_entities")).to_string(PostgresQueryBuilder);
        let sql_locs = self.build_table_schema(TargetTable::Locations, Alias::new("temp_locations")).to_string(PostgresQueryBuilder);
        let sql_tags = self.build_table_schema(TargetTable::FileTags, Tbl::TempFileTags).to_string(PostgresQueryBuilder);
        self.conn.execute_batch(&format!("{};{};{}", sql_ents, sql_locs, sql_tags))?;

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
                    app_tag.append_row([&t.entity_id as &dyn ToSql, &t.tag_type, &t.tag_value])?;
                }
            }
            for row in moved {
                let mut lr = vec![&row.id as &dyn ToSql];
                lr.extend(row.values.iter().map(|v| v as &dyn ToSql));
                app_loc.append_row(lr.as_slice())?;
            }
        }

        // ストレージへの最終マージ
        self.store.merge_and_save(
            &self.store.file_entities_path(), 
            Alias::new("temp_file_entities"), 
            (!deleted_ids.is_empty()).then(|| Condition::all().add(Expr::col(Col::Id).is_not_in(deleted_ids.clone())))
        )?;

        self.store.merge_and_save(
            &self.store.locations_path(), 
            Alias::new("temp_locations"), 
            if unchanged_ids.is_empty() { 
                Some(Condition::all().add(Expr::val(1).eq(0))) 
            } else { 
                Some(Condition::all().add(Expr::col(Col::EntityId).is_in(unchanged_ids))) 
            }
        )?;

        self.store.merge_and_save(
            &self.store.file_tags_path(), 
            Tbl::TempFileTags, 
            (!deleted_ids.is_empty()).then(|| Condition::all().add(Expr::col(Col::EntityId).is_not_in(deleted_ids))) 
        )?;

        self.conn.execute("DROP TABLE temp_file_entities; DROP TABLE temp_locations; DROP TABLE temp_file_tags;", [])?;
        std::fs::remove_file(self.store.temp_scan_path()).ok();
        Ok(())
    }

    /// 指定されたターゲットテーブルのスキーマ（TableCreateStatement）を構築します。
    fn build_table_schema(&self, target: TargetTable, name: impl Iden + 'static) -> sea_query::TableCreateStatement {
        let mut create = Table::create().table(name).to_owned();
        match target {
            TargetTable::FileEntities => {
                create.col(SeaColumnDef::new(Col::Id).big_integer());
                for c in self.registry.get_all_columns().iter().filter(|c| c.target_table == TargetTable::FileEntities) {
                    let mut def = SeaColumnDef::new(Alias::new(&c.name));
                    match c.sql_type { 
                        "BIGINT" => def.big_integer(), 
                        "BOOLEAN" => def.boolean(), 
                        _ => def.string() 
                    };
                    create.col(&mut def);
                }
            },
            TargetTable::Locations => {
                create.col(SeaColumnDef::new(Col::EntityId).big_integer());
                for c in self.registry.get_all_columns().iter().filter(|c| c.target_table == TargetTable::Locations) {
                    let mut def = SeaColumnDef::new(Alias::new(&c.name));
                    match c.sql_type { 
                        "BIGINT" => def.big_integer(), 
                        "BOOLEAN" => def.boolean(), 
                        _ => def.string() 
                    };
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
        create
    }
}

// ========================================================
// Data Structures
// ========================================================

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

// ========================================================
// Tests
// ========================================================

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
}