use crate::taggers::{TagValue, TargetTable, ColumnDef};
use crate::{FunctionRegistry, TagFunction};
use crate::functions::{ScanEntry, ScanRole};
use crate::db::{Tbl, Col, DuckDbFunc, SqlType};
use crate::rank::SystemRank;
use crate::util::{self, ExecuteSql, ParquetExt, IdenExt, SelectExt, TableCreateExt};
use anyhow::Result;
use duckdb::{Connection, ToSql, Appender};
use sea_query::{
    Query, Expr, Alias, Condition, JoinType, PostgresQueryBuilder, 
    Func, Table, Iden, SelectStatement,
    CaseStatement, IntoIden
};
use std::path::{Path, PathBuf};
use rayon::prelude::*;

// ========================================================
// 0. File Scanner
// ========================================================

struct FileScanner<'a> {
    conn: &'a Connection,
    db_dir: PathBuf,
    dry_run: bool,
    on_progress: Option<&'a (dyn Fn(usize) + Sync + Send)>,
    walker: Option<ignore::WalkParallel>,
    tx: Option<std::sync::mpsc::Sender<ScanEntry>>,
}

impl<'a> FileScanner<'a> {
    fn new(
        conn: &'a Connection,
        root_path: PathBuf,
        db_dir: PathBuf,
        dry_run: bool,
        on_progress: Option<&'a (dyn Fn(usize) + Sync + Send)>,
    ) -> (Self, std::sync::mpsc::Receiver<ScanEntry>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let walker = ignore::WalkBuilder::new(root_path)
            .hidden(false)
            .git_ignore(true)
            .threads(rayon::current_num_threads())
            .build_parallel();

        // 比較を正確にするため、db_dir を正規化しておく
        let db_dir = db_dir.canonicalize().unwrap_or(db_dir);

        (
            Self {
                conn,
                db_dir: db_dir.clone(),
                on_progress,
                dry_run,
                walker: Some(walker),
                tx: Some(tx),
            },
            rx,
        )
    }

    fn prepare_tray(&self) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        Table::create()
            .table(Tbl::Scan)
            .temporary()
            .add_columns(ScanEntry::columns_with_type())
            .execute(self.conn)
    }

    fn scan<'s, 'e>(&mut self, s: &'s std::thread::Scope<'s, 'e>) {
        let walker = self.walker.take().expect("Walker already consumed");
        let tx = self.tx.take().expect("Sender already consumed");
        let db_dir = self.db_dir.clone();

        s.spawn(move || {
            let factory = move || ScanWalker::create(tx.clone(), db_dir.clone());
            walker.run(factory);
        });
    }

    fn write(&self, rx: std::sync::mpsc::Receiver<ScanEntry>) -> Result<usize> {
        let mut current_count = 0;
        let mut appender: Option<Appender<'_>> = if !self.dry_run {
            let table_name = Tbl::Scan.to_string().replace('"', "");
            Some(self.conn.appender(&table_name)?)
        } else {
            None
        };

        for entry in rx {
            if let Some(ref mut app) = appender {
                app.append_row(&*entry.as_params())?;
            }
            current_count += 1;
            if let Some(cb) = self.on_progress {
                if current_count % 1000 == 0 {
                    cb(current_count);
                }
            }
        }

        if let Some(cb) = self.on_progress {
            cb(current_count);
        }

        Ok(current_count)
    }

    fn finalize_table(&self, path: &Path) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        Query::select()
            .expr(Expr::cust("*"))
            .from(Tbl::Scan)
            .save_parquet(self.conn, path)?;

        Table::drop().table(Tbl::Scan).execute(self.conn).ok();
        Ok(())
    }
}

// --- Walker Implementation ---

struct ScanWalker {
    tx: std::sync::mpsc::Sender<ScanEntry>,
    db_dir: PathBuf,
}

impl ScanWalker {
    fn create(
        tx: std::sync::mpsc::Sender<ScanEntry>,
        db_dir: PathBuf,
    ) -> Box<dyn FnMut(Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState + Send> {
        let mut walker = Self { tx, db_dir };
        Box::new(move |res| walker.visit(res))
    }

    fn is_db_dir(&self, path: &Path) -> bool {
        // 対象パスを正規化し、db_dir の配下にあるかチェック
        path.canonicalize()
            .map(|p| p.starts_with(&self.db_dir))
            .unwrap_or(false)
    }

    fn try_create_entry(
        &self,
        res: Result<ignore::DirEntry, ignore::Error>,
    ) -> Option<ScanEntry> {
        res.ok()
            .filter(|e| !self.is_db_dir(e.path()))
            .and_then(|e| {
                let m = e.metadata().ok()?;
                ScanEntry::from_path_metadata(e.path(), &m).ok()
            })
    }

    fn visit(&mut self, res: Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState {
        let Some(entry) = self.try_create_entry(res) else {
            return ignore::WalkState::Continue;
        };

        if self.tx.send(entry).is_err() {
            return ignore::WalkState::Quit;
        }

        ignore::WalkState::Continue
    }
}

// ========================================================
// 1. Diff Auditor
// ========================================================

struct DiffAuditor {
    scan: String,
    ents: String,
    locs: String,
}

impl DiffAuditor {
    fn new(store: &IndexStore<'_>) -> Self {
        Self {
            scan: store.temp_scan_path().to_string_lossy().into_owned(),
            ents: store.file_entities_path().to_string_lossy().into_owned(),
            locs: store.locations_path().to_string_lossy().into_owned(),
        }
    }

    /// 既存のインデックスが存在しない（初回スキャン）かどうかを判定します。
    fn is_initial(&self) -> bool {
        !Path::new(&self.ents).exists()
    }

    /// 初回スキャン用：全ファイルを「処理対象」として取得するクエリ。
    fn query_all(&self) -> SelectStatement {
        Query::select()
            .columns(ScanEntry::column_idens())
            .from_subquery(util::parquet_query(&self.scan), Tbl::Scan)
            .to_owned()
    }

    /// 通常用：新規または内容が変更されたファイルを特定するクエリ。
    fn query_to_tag(&self) -> SelectStatement {
        QueryParts::to_tag(&self.scan, &self.ents)
    }

    /// 通常用：移動（パス変更）されたファイルを特定するクエリ。
    fn query_moved(&self) -> SelectStatement {
        QueryParts::moved(&self.scan, &self.ents, &self.locs)
    }

    /// 通常用：削除されたファイルのIDを特定するクエリ。
    fn query_deleted(&self) -> SelectStatement {
        QueryParts::deleted(&self.scan, &self.ents)
    }

    /// 通常用：変更のないファイルのIDを特定するクエリ。
    fn query_unchanged(&self) -> SelectStatement {
        QueryParts::unchanged(&self.scan, &self.ents, &self.locs)
    }
}

// ========================================================
// 2. Item Triager
// ========================================================

/// トリアージの結果、どのバケツに入れるべきかを表す中間型。
enum TriagePiece {
    Entity(TagValue),
    Location(TagValue),
    Tag(TagRow),
    None,
}

/// トリアージされたデータを一時的に蓄積するアキュムレータ。
struct TriageAccumulator {
    id: i64,
    entities: Vec<TagValue>,
    locations: Vec<TagValue>,
    tags: Vec<TagRow>,
}

impl TriageAccumulator {
    fn new(id: i64) -> Self {
        Self {
            id,
            entities: vec![TagValue::BigInt(0)], // 1列目は Rank(0)
            locations: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// ピースを適切なバケツへ振り分けます。
    fn collect(mut self, piece: TriagePiece) -> Self {
        match piece {
            TriagePiece::Entity(v) => self.entities.push(v),
            TriagePiece::Location(v) => self.locations.push(v),
            TriagePiece::Tag(t) => self.tags.push(t),
            TriagePiece::None => {}
        }
        self
    }

    fn finish(self) -> TaggingResult {
        TaggingResult {
            entity_row: DynamicRow {
                id: self.id,
                values: self.entities,
            },
            location_row: DynamicRow {
                id: self.id,
                values: self.locations,
            },
            tags: self.tags,
        }
    }
}

struct ItemTriager<'a> {
    conn: &'a Connection,
    registry: &'a FunctionRegistry,
    store: &'a IndexStore<'a>,
}

impl<'a> ItemTriager<'a> {
    fn new(
        conn: &'a Connection,
        reg: &'a FunctionRegistry,
        store: &'a IndexStore<'a>,
    ) -> Self {
        Self {
            conn,
            registry: reg,
            store,
        }
    }

    /// 各ファイルから並列で情報を抽出します。
    fn extract_all(&self, entries: Vec<ScanEntry>) -> Result<Vec<Vec<TagValue>>> {
        entries
            .into_par_iter()
            .map(|e| self.registry.process_file(Path::new(&e.path.value)))
            .collect()
    }

    /// 抽出された情報を ID 付与と共にデータベース形式へ選別します。
    fn assemble_records(
        &self,
        all_values: Vec<Vec<TagValue>>,
    ) -> Result<Vec<TaggingResult>> {
        let max_id = self.get_max_id()?;
        let columns = self.registry.get_all_columns();

        all_values
            .into_iter()
            .enumerate()
            .map(|(i, values)| {
                let id = max_id + (i as i64) + 1;
                Ok(self.triage_item(id, values, &columns))
            })
            .collect()
    }

    /// 1アイテム分のトリアージを実行するメインパイプライン。
    fn triage_item(
        &self,
        id: i64,
        values: Vec<TagValue>,
        cols: &[ColumnDef],
    ) -> TaggingResult {
        values
            .into_iter()
            .zip(cols)
            .map(|(v, c)| self.classify(id, v, c))
            .fold(TriageAccumulator::new(id), |acc, p| acc.collect(p))
            .finish()
    }

    /// カラムの TargetTable に基づいて、どのバケツへ振り分けるべきかを決定します。
    fn classify(&self, id: i64, val: TagValue, col: &ColumnDef) -> TriagePiece {
        match col.target_table {
            TargetTable::FileEntities => TriagePiece::Entity(val),
            TargetTable::Locations => TriagePiece::Location(val),
            TargetTable::BaseTags => self.triage_base_tag(id, val, &col.name),
            _ => TriagePiece::None,
        }
    }

    /// タグ値が有効（非空）であればタグとして採用し、そうでなければ無視します。
    fn triage_base_tag(&self, id: i64, val: TagValue, name: &str) -> TriagePiece {
        val.into_string()
            .filter(|s| !s.is_empty())
            .map(|label| {
                TriagePiece::Tag(TagRow {
                    item_id: id,
                    tag_type: name.to_string(),
                    label,
                })
            })
            .unwrap_or(TriagePiece::None)
    }

    fn rebuild_moved_locations(
        &self,
        moved: Vec<(i64, String)>,
    ) -> Result<Vec<DynamicRow>> {
        let functions = self.registry.all_functions();
        moved
            .into_iter()
            .map(|(id, path)| {
                let values =
                    self.rebuild_values_from_path(Path::new(&path), functions);
                Ok(DynamicRow { id, values })
            })
            .collect()
    }

    /// パス情報から場所関連のタグ値を宣言的に再生成します。
    fn rebuild_values_from_path(
        &self,
        path: &Path,
        functions: &[Box<dyn TagFunction>],
    ) -> Vec<TagValue> {
        functions
            .iter()
            .flat_map(|f| {
                f.tagger()
                    .into_iter()
                    .flat_map(|t| t.get_columns())
                    .filter_map(move |col| {
                        (col.target_table == TargetTable::Locations).then(|| {
                            f.generate_from_path(path).unwrap_or(TagValue::Null)
                        })
                    })
            })
            .collect()
    }

    /// 現在のデータベースにおける最大 ItemID を取得します。
    fn get_max_id(&self) -> Result<i64> {
        if !self.store.file_entities_path().exists() {
            return Ok(0);
        }
        let ents_path = self.store.file_entities_path();
        let ents_str = ents_path.to_string_lossy();
        let query = Query::select()
            .expr(Func::cust(DuckDbFunc::Coalesce).args([
                Expr::col(Col::ItemId).max().into(),
                Expr::val(0).into(),
            ]))
            .from_subquery(util::parquet_query(&ents_str), Tbl::FileEntities)
            .to_string(PostgresQueryBuilder);

        self.conn
            .query_row(&query, [], |r| r.get(0))
            .map_err(Into::into)
    }
}

// ========================================================
// 3. Storage Manager (IndexStore)
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

    pub(crate) fn merge_and_save(
        &self,
        path: &Path,
        temp_table: impl Iden + Clone + 'static,
        filter: Option<Condition>,
    ) -> Result<()> {
        let query = if path.exists() {
            let path_str = path.to_string_lossy().to_string();
            let mut base = util::parquet_query(&path_str);
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
        query.save_parquet(self.conn, path)
    }

    pub(crate) fn build_table_schema(
        &self,
        target: TargetTable,
        name: impl Iden + 'static,
        columns: &[ColumnDef],
    ) -> sea_query::TableCreateStatement {
        use sea_query::{ColumnDef as SeaColumnDef, Table};
        let mut create = Table::create().table(name).to_owned();
        match target {
            TargetTable::FileEntities => {
                create.col(SeaColumnDef::new(Col::ItemId).big_integer());
                create.col(SeaColumnDef::new(Col::Rank).big_integer());
                for c in columns
                    .iter()
                    .filter(|c| c.target_table == TargetTable::FileEntities)
                {
                    let iden = Col::from_str(&c.name)
                        .map(|c| c.into_iden())
                        .unwrap_or_else(|| crate::util::alias_from(&c.name));
                    let mut def = SeaColumnDef::new(iden);
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
                    let iden = Col::from_str(&c.name)
                        .map(|c| c.into_iden())
                        .unwrap_or_else(|| crate::util::alias_from(&c.name));
                    let mut def = SeaColumnDef::new(iden);
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

// ========================================================
// 2. Query Builder (QueryParts)
// ========================================================

pub(crate) struct QueryParts;

impl QueryParts {
    fn identity(left: Tbl, right: Tbl) -> Condition {
        let mut cond = Condition::all();
        for cd in ScanEntry::schema() {
            if matches!(cd.role, ScanRole::ScanId) {
                let col = Col::from_str(&cd.name).map(|c| c.into_iden()).unwrap_or_else(|| Alias::new(cd.name).into_iden());
                cond = cond.add(
                    Expr::col((left, col.clone())).eq(Expr::col((right, col.clone()))),
                );
            }
        }
        cond
    }

    fn integrity(left: Tbl, right: Tbl) -> Condition {
        let mut cond = Condition::all();
        for cd in ScanEntry::schema() {
            if matches!(cd.role, ScanRole::Integrity) {
                let col = Col::from_str(&cd.name).map(|c| c.into_iden()).unwrap_or_else(|| Alias::new(cd.name).into_iden());
                cond = cond.add(
                    Expr::col((left, col.clone())).eq(Expr::col((right, col.clone()))),
                );
            }
        }
        cond
    }

    fn to_tag(scan_path: &str, entities_path: &str) -> SelectStatement {
        let sub_exists = Query::select()
            .expr(Expr::val(1))
            .from_subquery(util::parquet_query(entities_path), Tbl::FileEntities)
            .cond_where(Self::identity(Tbl::FileEntities, Tbl::Scan))
            .cond_where(Self::integrity(Tbl::FileEntities, Tbl::Scan))
            .to_owned();

        let columns = ScanEntry::schema()
            .iter()
            .map(|c| {
                let col = Col::from_str(&c.name).map(|c| c.into_iden()).unwrap_or_else(|| Alias::new(c.name).into_iden());
                (Tbl::Scan, col)
            })
            .collect::<Vec<_>>();

        Query::select()
            .columns(columns)
            .from_subquery(util::parquet_query(scan_path), Tbl::Scan)
            .and_where(Expr::exists(sub_exists).not())
            .to_owned()
    }

    fn moved(
        scan_path: &str,
        entities_path: &str,
        loc_path: &str,
    ) -> SelectStatement {
        let path_name = ScanEntry::schema()[0].name;
        let col_path = Col::from_str(path_name).map(|c| c.into_iden()).unwrap_or_else(|| Alias::new(path_name).into_iden());
        Query::select()
            .column((Tbl::FileEntities, Col::ItemId))
            .column((Tbl::Scan, col_path.clone()))
            .from_subquery(util::parquet_query(entities_path), Tbl::FileEntities)
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(scan_path),
                Tbl::Scan,
                Self::identity(Tbl::FileEntities, Tbl::Scan),
            )
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(loc_path),
                Tbl::Locations,
                Expr::col((Tbl::FileEntities, Col::ItemId))
                    .eq(Expr::col((Tbl::Locations, Col::ItemId))),
            )
            .and_where(
                Expr::col((Tbl::Locations, Col::Path)).ne(Expr::col((Tbl::Scan, col_path))),
            )
            .cond_where(Self::integrity(Tbl::FileEntities, Tbl::Scan))
            .to_owned()
    }

    fn deleted(scan_path: &str, entities_path: &str) -> SelectStatement {
        let path_name = ScanEntry::schema()[0].name;
        let col_path = Col::from_str(path_name).map(|c| c.into_iden()).unwrap_or_else(|| Alias::new(path_name).into_iden());
        let join_cond = Condition::all()
            .add(Self::identity(Tbl::FileEntities, Tbl::Scan))
            .add(Self::integrity(Tbl::FileEntities, Tbl::Scan));

        Query::select()
            .column((Tbl::FileEntities, Col::ItemId))
            .from_subquery(util::parquet_query(entities_path), Tbl::FileEntities)
            .join_subquery(
                JoinType::LeftJoin,
                util::parquet_query(scan_path),
                Tbl::Scan,
                join_cond,
            )
            .and_where(Expr::col((Tbl::Scan, col_path)).is_null())
            .to_owned()
    }

    /// 差分データから (type, label) のペアを抽出します。
    fn diff_tags(all_cols: &[ColumnDef]) -> SelectStatement {
        let mut source_q = Query::select();
        source_q
            .expr_as(Expr::col(Col::Type), Col::Type)
            .expr_as(Expr::col(Col::Label), Col::Label)
            .from(Tbl::BaseTagsDiff);

        for col in all_cols.iter().filter(|c| {
            c.target_table == TargetTable::Locations
        }) {
            let mut sub = Query::select();
            let col_iden = Col::from_str(&col.name)
                .map(|c| c.into_iden())
                .unwrap_or_else(|| Alias::new(col.name.clone()).into_iden());
            sub.expr_as(Expr::val(col.name.to_string()), Col::Type)
                .expr_as(
                    Expr::col(col_iden.clone()).cast_as(SqlType::VARCHAR),
                    Col::Label,
                )
                .from(Tbl::LocationsDiff)
                .and_where(Expr::col(col_iden).is_not_null());
            source_q.union(sea_query::UnionType::Distinct, sub.to_owned());
        }
        source_q
    }

    /// タグのペアから、Itemの3つのバリアント (type, label, typedtag) を生成します。
    fn expand_variants(tags: SelectStatement) -> SelectStatement {
        let mut cand_q = Query::select();
        cand_q
            .expr_as(Expr::val("type"), Col::ItemKind)
            .expr_as(Expr::col(Col::Type), Col::Content)
            .column(Col::Type)
            .expr_as(Expr::cust("NULL"), Col::Label)
            .from_subquery(tags.clone(), Tbl::Diff);

        let mut label_q = Query::select();
        label_q
            .expr_as(Expr::val("label"), Col::ItemKind)
            .expr_as(Expr::col(Col::Label), Col::Content)
            .expr_as(Expr::cust("NULL"), Col::Type)
            .column(Col::Label)
            .from_subquery(tags.clone(), Tbl::Diff);
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
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(tags, Tbl::Diff);
        cand_q.union(sea_query::UnionType::Distinct, tt_q.to_owned());

        cand_q
    }

    /// Registry に登録されている全ての機能名を、Type アイテムの候補として生成します。
    fn registry_variants(registry: &FunctionRegistry) -> SelectStatement {
        let funcs = registry.all_functions();
        if funcs.is_empty() {
            // 空の場合はダミーの空クエリを返す
            let mut q = Query::select();
            q.expr(Expr::val(1)).and_where(Expr::val(1).eq(0));
            return q;
        }

        let mut query = Query::select();
        // 最初の要素で初期化
        let first = &funcs[0];
        query
            .expr_as(Expr::val("type"), Col::ItemKind)
            .expr_as(Expr::val(first.name()), Col::Content)
            .expr_as(Expr::val(first.name()), Col::Type)
            .expr_as(Expr::cust("NULL"), Col::Label);

        // 残りをUNION
        for func in funcs.iter().skip(1) {
            let mut sub = Query::select();
            sub.expr_as(Expr::val("type"), Col::ItemKind)
                .expr_as(Expr::val(func.name()), Col::Content)
                .expr_as(Expr::val(func.name()), Col::Type)
                .expr_as(Expr::cust("NULL"), Col::Label);
            query.union(sea_query::UnionType::Distinct, sub);
        }
        query
    }

    /// 登録候補の中から、既存データにない新規分のみを抽出します。
    fn filter_new(candidates: SelectStatement, items_path: &str) -> SelectStatement {
        Query::select()
            .column((Tbl::Item, Col::ItemKind))
            .column((Tbl::Item, Col::Content))
            .column((Tbl::Item, Col::Type))
            .column((Tbl::Item, Col::Label))
            .distinct()
            .from_subquery(candidates, Tbl::Item)
            .join_subquery(
                JoinType::LeftJoin,
                util::parquet_query(items_path),
                Tbl::ItemEntities,
                Condition::all()
                    .add(
                        Expr::col((Tbl::Item, Col::ItemKind))
                            .eq(Expr::col((Tbl::ItemEntities, Col::ItemKind))),
                    )
                    .add(
                        Expr::col((Tbl::Item, Col::Content))
                            .eq(Expr::col((Tbl::ItemEntities, Col::Content))),
                    ),
            )
            .and_where(Expr::col((Tbl::ItemEntities, Col::ItemId)).is_null())
            .to_owned()
    }

    /// 新規アイテムに対し、開始IDからの連番とランクを付与するクエリを構築します。
    fn assign_ids(start_id: i64, registry: &FunctionRegistry) -> SelectStatement {
        let rank_expr = crate::rank::build_rank_expr(
            registry,
            Condition::all().add(Expr::col(Col::ItemKind).eq("type")), // Guard condition
            Expr::col(Col::Content),             // Key expression
            SystemRank::DEFAULT,                 // Default rank
        );

        Query::select()
            .expr_as(
                Expr::cust_with_exprs(
                    "$1 - (row_number() OVER () - 1)",
                    [Expr::val(start_id).into()],
                ),
                Col::ItemId,
            )
            .expr_as(rank_expr, Col::Rank)
            .column(Col::ItemKind)
            .column(Col::Content)
            .column(Col::Type)
            .column(Col::Label)
            .from(Tbl::Item)
            .to_owned()
    }

    /// 新規Item（IdItem）から、そのItem自体を説明するシステムタグを生成します。
    fn metadata_tags() -> SelectStatement {
        let mut meta = Query::select();
        meta.column(Col::ItemId)
            .expr_as(Expr::val("type"), Col::Type)
            .expr_as(
                CaseStatement::new()
                    .case(
                        Expr::col(Col::ItemKind).eq("typedtag"),
                        Expr::col(Col::Type),
                    )
                    .finally(Expr::col(Col::ItemKind)),
                Col::Label,
            )
            .from(Tbl::IdItem);

        let mut label = Query::select();
        label
            .column(Col::ItemId)
            .expr_as(Expr::val("label"), Col::Type)
            .column(Col::Label)
            .from(Tbl::IdItem)
            .and_where(Expr::col(Col::ItemKind).eq("typedtag"));

        meta.union(sea_query::UnionType::All, label).to_owned()
    }

    fn unchanged(
        scan_path: &str,
        entities_path: &str,
        loc_path: &str,
    ) -> SelectStatement {
        let path_name = ScanEntry::schema()[0].name;
        let col_path = Col::from_str(path_name).map(|c| c.into_iden()).unwrap_or_else(|| Alias::new(path_name).into_iden());
        Query::select()
            .column((Tbl::FileEntities, Col::ItemId))
            .from_subquery(util::parquet_query(entities_path), Tbl::FileEntities)
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(scan_path),
                Tbl::Scan,
                Self::identity(Tbl::FileEntities, Tbl::Scan),
            )
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(loc_path),
                Tbl::Locations,
                Expr::col((Tbl::FileEntities, Col::ItemId))
                    .eq(Expr::col((Tbl::Locations, Col::ItemId))),
            )
            .and_where(
                Expr::col((Tbl::Locations, Col::Path)).eq(Expr::col((Tbl::Scan, col_path))),
            )
            .cond_where(Self::integrity(Tbl::FileEntities, Tbl::Scan))
            .to_owned()
    }
}

// ========================================================
// 3. Execution Extensions (SelectFetchExt)
// ========================================================

pub trait SelectFetchExt {
    fn fetch_entries(&self, conn: &Connection) -> Result<Vec<ScanEntry>>;
    fn fetch_ids(&self, conn: &Connection) -> Result<Vec<i64>>;
    fn fetch_moved(&self, conn: &Connection) -> Result<Vec<(i64, String)>>;
}

impl SelectFetchExt for SelectStatement {
    fn fetch_entries(&self, conn: &Connection) -> Result<Vec<ScanEntry>> {
        let sql = self.to_string(PostgresQueryBuilder);
        conn.prepare(&sql)?
            .query_map([], |row| ScanEntry::from_row(row))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn fetch_ids(&self, conn: &Connection) -> Result<Vec<i64>> {
        let sql = self.to_string(PostgresQueryBuilder);
        conn.prepare(&sql)?
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn fetch_moved(&self, conn: &Connection) -> Result<Vec<(i64, String)>> {
        let sql = self.to_string(PostgresQueryBuilder);
        conn.prepare(&sql)?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

// ========================================================
// 4. Main Indexer
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
        let (results, moved) = self.triage_phase(diff.to_tag, diff.moved)?;
        self.merge_phase(results, moved, diff.deleted_ids, diff.unchanged_ids)?;
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

        crate::oneview::OneView::recreate(self.conn, &all_cols, &self.store.db_dir)?;
        
        // システム定義アイテム（name, kind等）を先行登録
        self.update_system_items(None)?;
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
        let table = Tbl::Master; // Safe temp name for initialization
        self.store
            .build_table_schema(target, table, columns)
            .execute(self.conn)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        table.write_parquet(self.conn, path)?;
        Table::drop().table(table).execute(self.conn).ok();
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
        let (mut scanner, rx) = FileScanner::new(
            self.conn,
            root_path.to_path_buf(),
            self.store.db_dir.clone(),
            dry_run,
            on_progress.map(|f| f as _),
        );

        scanner.prepare_tray()?;

        let count = std::thread::scope(|s| {
            scanner.scan(s);
            scanner.write(rx)
        })?;

        scanner.finalize_table(&self.store.temp_scan_path())?;

        Ok(count)
    }

    fn diff_phase(&self) -> Result<IndexDiff> {
        let diff = DiffAuditor::new(&self.store);

        // 初回スキャンの場合
        if diff.is_initial() {
            return Ok(IndexDiff {
                to_tag: diff.query_all().fetch_entries(self.conn)?,
                ..Default::default()
            });
        }

        // 通常の更新の場合
        Ok(IndexDiff {
            to_tag: diff.query_to_tag().fetch_entries(self.conn)?,
            moved: diff.query_moved().fetch_moved(self.conn)?,
            deleted_ids: diff.query_deleted().fetch_ids(self.conn)?,
            unchanged_ids: diff.query_unchanged().fetch_ids(self.conn)?,
        })
    }

    fn triage_phase(
        &self,
        to_tag: Vec<ScanEntry>,
        moved: Vec<(i64, String)>,
    ) -> Result<(Vec<TaggingResult>, Vec<DynamicRow>)> {
        let triager = ItemTriager::new(self.conn, self.registry, &self.store);

        // 1. 各ファイルから並列で情報を抽出
        let raw_values = triager.extract_all(to_tag)?;

        // 2. 抽出された情報を ID 付与と共にデータベース形式へ選別
        let results = triager.assemble_records(raw_values)?;

        // 3. 移動されたファイルに対する場所情報の再生成
        let moved_rows = triager.rebuild_moved_locations(moved)?;

        Ok((results, moved_rows))
    }

    fn merge_phase(
        &self,
        results: Vec<TaggingResult>,
        moved: Vec<DynamicRow>,
        deleted_ids: Vec<i64>,
        unchanged_ids: Vec<i64>,
    ) -> Result<()> {
        // 変更がない場合は更新をスキップ
        if results.is_empty() && moved.is_empty() && deleted_ids.is_empty() {
            return Ok(());
        }

        let has_updates = !results.is_empty() || !moved.is_empty();

        // 1. 各カテゴリごとにステージングと同期
        let ent = self.entities_merger()
            .prepare()?
            .ingest(&results)?
            .sync(&deleted_ids)?;

        let loc = self.locations_merger()
            .prepare()?
            .ingest(&results, &moved)?
            .sync(&unchanged_ids)?;

        let tag = self.tags_merger()
            .prepare()?
            .ingest(&results)?
            .sync(&deleted_ids)?;

        if has_updates {
            // 候補の特定 (抽出 -> 展開)
            let tags = QueryParts::diff_tags(&self.registry.get_all_columns());
            let candidates_data = QueryParts::expand_variants(tags);
            self.update_system_items(Some(candidates_data))?;
        }

        // 3. クリーンアップ
        ent.cleanup()?;
        loc.cleanup()?;
        tag.cleanup()?;
        std::fs::remove_file(self.store.temp_scan_path()).ok();
        Ok(())
    }

    pub fn update_system_items(&self, data_candidates: Option<SelectStatement>) -> Result<()> {
        let items_path = self.store.item_entities_path();
        let system_tags_path = self.store.system_tags_path();
        let items_str = items_path.to_string_lossy();
        let stags_str = system_tags_path.to_string_lossy();

        // 1. 候補の結合 (Registry + Data)
        let mut all_candidates = QueryParts::registry_variants(self.registry);
        
        if let Some(data) = data_candidates {
            all_candidates.union(sea_query::UnionType::Distinct, data);
        }

        QueryParts::filter_new(all_candidates, &items_str)
            .create_temp_table_as(self.conn, Tbl::Item)?;

        if self.count_table(Tbl::Item)? == 0 {
            Tbl::Item.drop_table(self.conn)?;
            return Ok(());
        }

        // 3. IDの割り当て
        let start_id = self.next_item_id(&items_str)?;
        let tmp_items = items_path.with_extension("parquet.tmp");
        let tmp_stags = system_tags_path.with_extension("parquet.tmp");

        QueryParts::assign_ids(start_id, self.registry)
            .create_temp_table_as(self.conn, Tbl::IdItem)?;

        // 4. item_entities 更新
        util::parquet_query(&items_str)
            .union(
                sea_query::UnionType::All,
                Query::select()
                    .columns([Col::ItemId, Col::Rank, Col::ItemKind, Col::Content])
                    .from(Tbl::IdItem)
                    .to_owned(),
            )
            .save_parquet(self.conn, &tmp_items)?;

        // 5. system_tags 更新
        util::parquet_query(&stags_str)
            .union(sea_query::UnionType::All, QueryParts::metadata_tags())
            .save_parquet(self.conn, &tmp_stags)?;

        // 6. 後片付け
        self.finalize_updates(
            &items_path,
            &system_tags_path,
            &tmp_items,
            &tmp_stags,
        )
    }

    fn count_table(&self, table: impl Iden + Clone + 'static) -> Result<i64> {
        let sql = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from(table)
            .to_string(PostgresQueryBuilder);
        self.conn.query_row(&sql, [], |r| r.get(0)).map_err(Into::into)
    }

    fn finalize_updates(
        &self,
        items_path: &Path,
        stags_path: &Path,
        tmp_items: &Path,
        tmp_stags: &Path,
    ) -> Result<()> {
        std::fs::rename(tmp_items, items_path)?;
        std::fs::rename(tmp_stags, stags_path)?;
        Tbl::Item.drop_table(self.conn)?;
        Tbl::IdItem.drop_table(self.conn)?;
        Ok(())
    }

    fn entities_merger(&self) -> FileEntityMerger {
        FileEntityMerger { indexer: self }
    }

    fn locations_merger(&self) -> LocationMerger {
        LocationMerger { indexer: self }
    }

    fn tags_merger(&self) -> BaseTagMerger {
        BaseTagMerger { indexer: self }
    }

    /// 既存のアイテムエントリから、次に割り当てるべき開始ID（負の値）を取得します。
    fn next_item_id(&self, items_path: &str) -> Result<i64> {
        let query_min = Query::select()
            .expr(
                Func::cust(DuckDbFunc::Coalesce)
                    .args([Expr::col(Col::ItemId).min().into(), Expr::val(0).into()]),
            )
            .from_subquery(util::parquet_query(items_path), Tbl::ItemEntities)
            .to_string(PostgresQueryBuilder);

        let min_id: i64 = self.conn.query_row(&query_min, [], |r| r.get(0))?;
        Ok(if min_id > -1 { -1 } else { min_id - 1 })
    }
}

// ========================================================
// 5. Merger Contexts
// ========================================================

struct FileEntityMerger<'a> {
    indexer: &'a Indexer<'a>,
}

impl FileEntityMerger<'_> {
    fn prepare(self) -> Result<Self> {
        let all_cols = self.indexer.registry.get_all_columns();
        self.indexer
            .store
            .build_table_schema(
                TargetTable::FileEntities,
                Tbl::FileEntitiesDiff,
                &all_cols,
            )
            .execute(self.indexer.conn)?;
        Ok(self)
    }

    fn ingest(self, results: &[TaggingResult]) -> Result<Self> {
        if results.is_empty() {
            return Ok(self);
        }
        let mut app = self.indexer.conn.appender(
            &Tbl::FileEntitiesDiff.to_string().replace('"', ""),
        )?;
        for res in results {
            let mut er = vec![&res.entity_row.id as &dyn ToSql];
            er.extend(res.entity_row.values.iter().map(|v| v as &dyn ToSql));
            app.append_row(er.as_slice())?;
        }
        Ok(self)
    }

    fn sync(self, deleted_ids: &[i64]) -> Result<Self> {
        self.indexer.store.merge_and_save(
            &self.indexer.store.file_entities_path(),
            Tbl::FileEntitiesDiff,
            (!deleted_ids.is_empty()).then(|| {
                Condition::all()
                    .add(Expr::col(Col::ItemId).is_not_in(deleted_ids.to_vec()))
            }),
        )?;
        Ok(self)
    }

    fn cleanup(self) -> Result<()> {
        Tbl::FileEntitiesDiff.drop_table(self.indexer.conn).ok();
        Ok(())
    }
}


struct LocationMerger<'a> {
    indexer: &'a Indexer<'a>,
}

impl LocationMerger<'_> {
    fn prepare(self) -> Result<Self> {
        let all_cols = self.indexer.registry.get_all_columns();
        self.indexer
            .store
            .build_table_schema(
                TargetTable::Locations,
                Tbl::LocationsDiff,
                &all_cols,
            )
            .execute(self.indexer.conn)?;
        Ok(self)
    }

    fn ingest(self, results: &[TaggingResult], moved: &[DynamicRow]) -> Result<Self> {
        if results.is_empty() && moved.is_empty() {
            return Ok(self);
        }
        let mut app = self.indexer.conn.appender(
            &Tbl::LocationsDiff.to_string().replace('"', ""),
        )?;
        for res in results {
            let mut lr = vec![&res.location_row.id as &dyn ToSql];
            lr.extend(res.location_row.values.iter().map(|v| v as &dyn ToSql));
            app.append_row(lr.as_slice())?;
        }
        for row in moved {
            let mut lr = vec![&row.id as &dyn ToSql];
            lr.extend(row.values.iter().map(|v| v as &dyn ToSql));
            app.append_row(lr.as_slice())?;
        }
        Ok(self)
    }

    fn sync(self, unchanged_ids: &[i64]) -> Result<Self> {
        self.indexer.store.merge_and_save(
            &self.indexer.store.locations_path(),
            Tbl::LocationsDiff,
            if unchanged_ids.is_empty() {
                Some(Condition::all().add(Expr::val(1).eq(0)))
            } else {
                Some(
                    Condition::all()
                        .add(Expr::col(Col::ItemId).is_in(unchanged_ids.to_vec())),
                )
            },
        )?;
        Ok(self)
    }

    fn cleanup(self) -> Result<()> {
        Tbl::LocationsDiff.drop_table(self.indexer.conn).ok();
        Ok(())
    }
}


struct BaseTagMerger<'a> {
    indexer: &'a Indexer<'a>,
}

impl BaseTagMerger<'_> {
    fn prepare(self) -> Result<Self> {
        let all_cols = self.indexer.registry.get_all_columns();
        self.indexer
            .store
            .build_table_schema(
                TargetTable::BaseTags,
                Tbl::BaseTagsDiff,
                &all_cols,
            )
            .execute(self.indexer.conn)?;
        Ok(self)
    }

    fn ingest(self, results: &[TaggingResult]) -> Result<Self> {
        if results.is_empty() {
            return Ok(self);
        }
        let mut app = self.indexer.conn.appender(
            &Tbl::BaseTagsDiff.to_string().replace('"', ""),
        )?;
        for res in results {
            for t in &res.tags {
                app.append_row([
                    &t.item_id as &dyn ToSql,
                    &t.tag_type,
                    &t.label,
                ])?;
            }
        }
        Ok(self)
    }

    fn sync(self, deleted_ids: &[i64]) -> Result<Self> {
        self.indexer.store.merge_and_save(
            &self.indexer.store.base_tags_path(),
            Tbl::BaseTagsDiff,
            (!deleted_ids.is_empty()).then(|| {
                Condition::all()
                    .add(Expr::col(Col::ItemId).is_not_in(deleted_ids.to_vec()))
            }),
        )?;
        Ok(self)
    }

    fn cleanup(self) -> Result<()> {
        Tbl::BaseTagsDiff.drop_table(self.indexer.conn).ok();
        Ok(())
    }
}




#[derive(Default)]
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
    fn test_diff_auditor_logic() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join("db");
        let conn = Connection::open_in_memory().unwrap();
        let store = IndexStore::new(&conn, db_dir);

        // 1. 初回スキャンのテスト
        let diff = DiffAuditor::new(&store);
        assert!(diff.is_initial());
        // query_all が構文エラーなく生成されるか確認
        let sql = diff.query_all().to_string(PostgresQueryBuilder);
        assert!(sql.contains("read_parquet"));

        // 2. 既存DBがある場合のテスト
        // ダミーのエンティティファイルを作成
        std::fs::create_dir_all(&store.db_dir).unwrap();
        std::fs::write(store.file_entities_path(), "").unwrap();
        let diff2 = DiffAuditor::new(&store);
        assert!(!diff2.is_initial());
    }

    #[test]
    fn test_triager_base_tag_logic() {
        let conn = Connection::open_in_memory().unwrap();
        let registry = FunctionRegistry::new();
        let dir = tempdir().unwrap();
        let store = IndexStore::new(&conn, dir.path().to_path_buf());
        let triager = ItemTriager::new(&conn, &registry, &store);

        // 1. 有効な値のテスト
        let val = TagValue::Text("rs".to_string());
        let piece = triager.triage_base_tag(100, val, "extension");
        if let TriagePiece::Tag(tag) = piece {
            assert_eq!(tag.item_id, 100);
            assert_eq!(tag.tag_type, "extension");
            assert_eq!(tag.label, "rs");
        } else {
            panic!("Should be TriagePiece::Tag");
        }

        // 2. 空文字のテスト（無視されるべき）
        let empty_val = TagValue::Text("".to_string());
        let piece_empty = triager.triage_base_tag(100, empty_val, "extension");
        assert!(matches!(piece_empty, TriagePiece::None));

        // 3. Nullのテスト（無視されるべき）
        let piece_null = triager.triage_base_tag(100, TagValue::Null, "extension");
        assert!(matches!(piece_null, TriagePiece::None));
    }

    #[test]
    fn test_triager_classify_logic() {
        let conn = Connection::open_in_memory().unwrap();
        let registry = FunctionRegistry::new();
        let dir = tempdir().unwrap();
        let store = IndexStore::new(&conn, dir.path().to_path_buf());
        let triager = ItemTriager::new(&conn, &registry, &store);

        // 1. FileEntities のテスト
        let col_ent = ColumnDef {
            name: "size".to_string(),
            sql_type: "BIGINT",
            target_table: TargetTable::FileEntities,
        };
        let p_ent = triager.classify(1, TagValue::BigInt(1024), &col_ent);
        assert!(matches!(p_ent, TriagePiece::Entity(TagValue::BigInt(1024))));

        // 2. Locations のテスト
        let col_loc = ColumnDef {
            name: "path".to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::Locations,
        };
        let p_loc = triager.classify(1, TagValue::Text("/a".to_string()), &col_loc);
        assert!(matches!(p_loc, TriagePiece::Location(TagValue::Text(_))));

        // 3. BaseTags のテスト
        let col_tag = ColumnDef {
            name: "ext".to_string(),
            sql_type: "TEXT",
            target_table: TargetTable::BaseTags,
        };
        let p_tag = triager.classify(1, TagValue::Text("rs".to_string()), &col_tag);
        assert!(matches!(p_tag, TriagePiece::Tag(_)));
    }

    #[test]
    fn test_triager_triage_item_full() {
        let conn = Connection::open_in_memory().unwrap();
        let registry = FunctionRegistry::new();
        let dir = tempdir().unwrap();
        let store = IndexStore::new(&conn, dir.path().to_path_buf());
        let triager = ItemTriager::new(&conn, &registry, &store);

        let cols = vec![
            ColumnDef { name: "size".into(), sql_type: "BIGINT", target_table: TargetTable::FileEntities },
            ColumnDef { name: "path".into(), sql_type: "TEXT", target_table: TargetTable::Locations },
            ColumnDef { name: "ext".into(), sql_type: "TEXT", target_table: TargetTable::BaseTags },
        ];
        let vals = vec![
            TagValue::BigInt(500),
            TagValue::Text("/foo.rs".into()),
            TagValue::Text("rs".into()),
        ];

        let res = triager.triage_item(7, vals, &cols);

        assert_eq!(res.entity_row.id, 7);
        // Entity: [Rank(0), Size(500)]
        assert_eq!(res.entity_row.values.len(), 2);
        assert_eq!(res.entity_row.values[1], TagValue::BigInt(500));

        assert_eq!(res.location_row.id, 7);
        assert_eq!(res.location_row.values[0], TagValue::Text("/foo.rs".into()));

        assert_eq!(res.tags.len(), 1);
        assert_eq!(res.tags[0].tag_type, "ext");
        assert_eq!(res.tags[0].label, "rs");
    }

    #[test]
    fn test_triager_rebuild_from_path_strict() {
        let conn = Connection::open_in_memory().unwrap();
        let registry = FunctionRegistry::with_standard();
        let dir = tempdir().unwrap();
        let store = IndexStore::new(&conn, dir.path().to_path_buf());
        let triager = ItemTriager::new(&conn, &registry, &store);

        let path = Path::new("/test/dir/file.txt");
        let functions = registry.all_functions();
        let values = triager.rebuild_values_from_path(path, functions);

        // registry.with_standard() において TargetTable::Locations なのは:
        // path, parentdir, filename, extension の 4つ。
        assert_eq!(values.len(), 4, "Should contain exactly 4 columns");

        assert_eq!(values[0], TagValue::Text("/test/dir/file.txt".into()));
        assert_eq!(values[1], TagValue::Text("/test/dir".into()));
        assert_eq!(values[2], TagValue::Text("file.txt".into()));
        assert_eq!(values[3], TagValue::Text("txt".into()));
    }

    #[test]
    fn test_incremental_indexing_full_flow() {
        let dir = tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let db_dir = base.join("db");
        let root = base.join("work");
        std::fs::create_dir_all(&root).unwrap();

        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        let all_files = "item_kind:file";

        // 1. 初回: a.txt を作成 (root + a.txt = 2)
        let path_a = root.join("a.txt");
        std::fs::write(&path_a, "initial content").unwrap();
        fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
        assert_eq!(fm.search(all_files).unwrap().len(), 2);
        assert_eq!(fm.search("filename:a.txt").unwrap().len(), 1);

        // 2. 変更なし: そのまま再スキャン (2)
        fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
        assert_eq!(fm.search(all_files).unwrap().len(), 2);

        // 3. 追加: b.rs を作成 (root + a.txt + b.rs = 3)
        let path_b = root.join("b.rs");
        std::fs::write(&path_b, "fn main() {}").unwrap();
        fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
        assert_eq!(fm.search(all_files).unwrap().len(), 3);
        assert_eq!(fm.search("filename:b.rs").unwrap().len(), 1);

        // 4. 更新: a.txt のサイズを変更 (3)
        std::fs::write(&path_a, "updated content with more bytes").unwrap();
        fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
        assert_eq!(fm.search(all_files).unwrap().len(), 3);

        // 5. 削除: b.rs を削除 (root + a.txt = 2)
        std::fs::remove_file(&path_b).unwrap();
        fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
        assert_eq!(fm.search(all_files).unwrap().len(), 2);
        assert_eq!(fm.search("filename:b.rs").unwrap().len(), 0);

        // 6. 移動: a.txt -> c.txt (root + c.txt = 2)
        let path_c = root.join("c.txt");
        std::fs::rename(&path_a, &path_c).unwrap();
        fm.index_directory(&root, None::<&fn(usize)>, false).unwrap();
        assert_eq!(fm.search(all_files).unwrap().len(), 2);
        assert_eq!(fm.search("filename:a.txt").unwrap().len(), 0);
        assert_eq!(fm.search("filename:c.txt").unwrap().len(), 1);
    }

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
            .from(Tbl::OneView)
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
            .from_subquery(util::parquet_query(&items_path), Tbl::ItemEntities)
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

        // 2. それらのItemの origin が system であるか確認 (all_tags ビュー経由)
        let query_origin = Query::select()
            .column(Col::Origin)
            .from(Tbl::OneView)
            .and_where(Expr::col(Col::Name).eq("extension:txt"))
            .and_where(Expr::col(Col::Type).eq("type")) // メタタグ
            .to_string(PostgresQueryBuilder);

        let origin: String = fm.conn
            .query_row(&query_origin, [], |r| r.get(0))
            .unwrap();
        
        assert_eq!(
            origin, "system",
            "Item origin should be 'system' via the view"
        );

        // 3. extension:txt が label:txt というタグを持っているか確認
        let query_label = Query::select()
            .column(Col::Label)
            .from(Tbl::OneView)
            .and_where(Expr::col(Col::Name).eq("extension:txt"))
            .and_where(Expr::col(Col::Type).eq("label"))
            .to_string(PostgresQueryBuilder);

        let label: String = fm.conn
            .query_row(&query_label, [], |r| r.get(0))
            .unwrap();
        assert_eq!(label, "txt");

        // 4. label:txt アイテム自体は label:txt というタグを持っていないことを確認
        // (type:label というメタタグのみ持っているはず)
        let query_item_label = Query::select()
            .expr(Expr::val(1))
            .from(Tbl::OneView)
            .and_where(Expr::col(Col::Name).eq("txt"))
            .and_where(Expr::col(Col::ItemKind).eq("label"))
            .and_where(Expr::col(Col::Type).eq("label"))
            .to_string(PostgresQueryBuilder);
        let exists_self_label: bool = fm.conn.prepare(&query_item_label).unwrap()
            .exists([]).unwrap();
        assert!(!exists_self_label, "Label item should not have self tag");

        // type アイテム (extension) が type:type を持っているか
        let query_type_type = Query::select()
            .expr(Expr::val(1))
            .from(Tbl::OneView)
            .and_where(Expr::col(Col::Name).eq("extension"))
            .and_where(Expr::col(Col::ItemKind).eq("type"))
            .and_where(Expr::col(Col::Type).eq("type"))
            .to_string(PostgresQueryBuilder);
        let exists_tt: bool = fm.conn.prepare(&query_type_type).unwrap()
            .exists([]).unwrap();
        assert!(exists_tt);

        // typedtag アイテム (extension:txt) が type:typedtag を持っていないか
        let query_tt_tt = Query::select()
            .expr(Expr::val(1))
            .from(Tbl::OneView)
            .and_where(Expr::col(Col::Name).eq("extension:txt"))
            .and_where(Expr::col(Col::Type).eq("typedtag"))
            .to_string(PostgresQueryBuilder);
        let exists_tt_tt: bool = fm.conn.prepare(&query_tt_tt).unwrap()
            .exists([]).unwrap();
        assert!(!exists_tt_tt);

        // label アイテム (txt) が type:label というタグを持っていか確認
        // (キーが "type"、値が "label")
        let query_label_meta = Query::select()
            .expr(Expr::val(1))
            .from(Tbl::OneView)
            .and_where(Expr::col(Col::Name).eq("txt"))
            .and_where(Expr::col(Col::ItemKind).eq("label"))
            .and_where(Expr::col(Col::Type).eq("type"))
            .and_where(Expr::col(Col::Label).eq("label"))
            .to_string(PostgresQueryBuilder);
        let exists_label: bool = fm.conn.prepare(&query_label_meta).unwrap()
            .exists([]).unwrap();
        assert!(exists_label);
    }

    #[test]
    fn test_typedtag_listing_via_type_query() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let db_dir = root.join(".ttfm/db");

        // 1. ファイルを準備
        std::fs::write(root.join("test.txt"), "hello").unwrap();

        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        let indexer = Indexer::new(&fm.conn, &fm.registry, db_dir);
        indexer.run(root, None::<&fn(usize)>, false).unwrap();

        // 2. type:extension で検索 -> extension:txt アイテムが見つかるはず
        let results = fm.search("type:extension").unwrap();
        let tt_items: Vec<_> = results.iter()
            .filter(|r| r.item_kind == "typedtag" && r.name == "extension:txt")
            .collect();
        assert_eq!(tt_items.len(), 1, "Should find the typedtag item for extension:txt");

        // 3. extension:txt で検索 -> ファイルだけが見つかるはず（ノイズがないこと）
        let results = fm.search("extension:txt").unwrap();
        let files: Vec<_> = results.iter().filter(|r| r.item_kind == "file").collect();
        let tags: Vec<_> = results.iter().filter(|r| r.item_kind == "typedtag").collect();
        
        assert_eq!(files.len(), 1, "Should find the file");
        assert_eq!(tags.len(), 0, "Should NOT find the typedtag item itself as noise");
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
    fn test_no_empty_extension_system_item() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let db_dir = root.join(".ttfm/db");

        // 拡張子のないファイルを作成
        std::fs::write(root.join("no_extension"), "test").unwrap();

        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        let indexer = Indexer::new(&fm.conn, &fm.registry, db_dir);
        indexer.run(root, None::<&fn(usize)>, false).unwrap();

        // "extension:" という typedtag が存在しないことを確認
        let items_path = fm.item_entities_path();
        let items_str = items_path.to_string_lossy().to_string();
        
        let sql = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from_subquery(
                util::parquet_query(&items_str),
                Tbl::ItemEntities
            )
            .and_where(Expr::col(Col::ItemKind).eq("typedtag"))
            .and_where(Expr::col(Col::Content).eq("extension:"))
            .to_string(PostgresQueryBuilder);

        let count: i64 = fm.conn.query_row(&sql, [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "Should NOT register 'extension:' system item");
    }

    #[test]
    fn test_definition_only_items_registration() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let db_dir = root.join(".ttfm/db");
        
        // ファイル作成
        std::fs::write(root.join("test.txt"), "").unwrap();

        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        let indexer = Indexer::new(&fm.conn, &fm.registry, db_dir);
        
        // インデックス実行
        indexer.run(root, None::<&fn(usize)>, false).unwrap();

        // item_entities に 'name' が登録されているか確認
        let query = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from_subquery(
                util::parquet_query(&indexer.store.item_entities_path().to_string_lossy()),
                Tbl::ItemEntities
            )
            .and_where(Expr::col(Col::ItemKind).eq("type"))
            .and_where(Expr::col(Col::Content).eq("name"))
            .to_string(PostgresQueryBuilder);

        let count: i64 = fm.conn.query_row(&query, [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "Should register 'name' as a type item");
        
        // 'kind' も確認
        let query_kind = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from_subquery(
                util::parquet_query(&indexer.store.item_entities_path().to_string_lossy()),
                Tbl::ItemEntities
            )
            .and_where(Expr::col(Col::ItemKind).eq("type"))
            .and_where(Expr::col(Col::Content).eq("kind"))
            .to_string(PostgresQueryBuilder);
            
        let count_kind: i64 = fm.conn.query_row(&query_kind, [], |r| r.get(0)).unwrap();
        assert_eq!(count_kind, 1, "Should register 'kind' as a type item");
    }
}