//! # TTFM (Typed Tag File Manager) Core Library
//!
//! このライブラリは、Typed Tag（型付きタグ）を用いたファイル管理システムのコア機能を提供します。
//! DuckDBをバックエンドに使用し、Parquet形式でのインデックス保存と高速な検索を実現します。

use crate::db::{Col, Tbl};
use crate::util::{ExecuteSql, IdenExt, SelectExt};
use anyhow::{Context, Result};
use duckdb::Connection;
use file_id::get_file_id;
use sea_query::{Expr, PostgresQueryBuilder, Query};
use std::path::Path;

pub mod config;
pub mod db;
pub mod indexing;
pub mod macros;
pub mod oneview;
pub mod plugins;
pub mod query;
pub mod rank;
pub mod response;
mod taggers;
pub mod types;
pub mod util;

pub use db::TargetTable;
use indexing::functions::{
    ContentIndexingFunction, DirectoryFunction, ExtensionFunction,
    FilenameFunction, IndexingFunction, InodeFunction, KindIndexingFunction,
    ModifiedStrFunction, ModifiedTsFunction, NameIndexingFunction,
    ParentDirFunction, PathFunction, SizeBytesFunction, SizeStrFunction,
    StemFunction, TypeFromExtFunction,
};
pub use query::{parse, QueryNode};
pub use response::{SearchResponse, SearchResult};
pub use taggers::{ColumnDef, TagValue, Tagger};
pub use types::{FileRef, ItemKind, Label, Progress, TagType, TypedTag};

mod cache;
pub use cache::CacheManager;
mod search;
pub use search::SearchOptions;

/// ファイルの一意識別子を 128ビット数値(FileRef)として取得します。
pub fn get_file_ref(path: &Path) -> Result<FileRef> {
    // 1. Inode 取得を試みる
    if let Ok(id) = get_file_id(path) {
        let (upper, lower) = match id {
            // Unix/Linux: device_id (64bit) + inode_number (64bit)
            file_id::FileId::Inode {
                device_id,
                inode_number,
            } => (device_id, inode_number),
            // Windows (Standard): volume_serial_number (32bit) + file_index (64bit)
            file_id::FileId::LowRes {
                volume_serial_number,
                file_index,
            } => (volume_serial_number as u64, file_index),
            // Windows (High Precision / ReFS): volume_serial_number (64bit) + file_id (128bit)
            file_id::FileId::HighRes {
                volume_serial_number,
                file_id,
            } => (
                (file_id >> 64) as u64 ^ volume_serial_number,
                file_id as u64,
            ),
        };
        return Ok(uuid::Uuid::from_u64_pair(upper, lower));
    }

    // 2. 失敗した場合（ELOOP, EIO等）はパス名から決定論的な UUID を生成
    Ok(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_DNS,
        path.to_string_lossy().as_bytes(),
    ))
}

/// TTFMのホームディレクトリを取得します。
/// 環境変数 `TTFM_HOME` が設定されていればそれを優先し、
/// なければ OS 標準のホームディレクトリ下の `.ttfm` を返します。
pub fn get_ttfm_home() -> Result<std::path::PathBuf> {
    if let Ok(home) = std::env::var("TTFM_HOME") {
        return Ok(std::path::PathBuf::from(home));
    }

    let mut home =
        dirs::home_dir().context("Failed to determine home directory")?;
    home.push(".ttfm");
    Ok(home)
}

/// TTFMのプラグインディレクトリを取得します。
pub fn get_ttfm_plugins_dir() -> Result<std::path::PathBuf> {
    Ok(get_ttfm_home()?.join("plugins"))
}

/// 全ての `IndexingFunction` を管理し、インデックス作成と検索の仲介を行うレジストリ。
pub struct FunctionRegistry {
    /// 登録されている機能のリスト
    functions: Vec<Box<dyn IndexingFunction>>,
}

impl Default for FunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FunctionRegistry {
    /// 空のレジストリを作成します。
    pub fn new() -> Self {
        Self {
            functions: Vec::new(),
        }
    }

    /// 新しい機能（`IndexingFunction`）をレジストリに追加します。
    /// 同名の機能が既に登録されている場合はスキップします。
    pub fn register(&mut self, func: Box<dyn IndexingFunction>) {
        let name = func.name();
        if self.functions.iter().any(|f| f.name() == name) {
            return;
        }
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

        // 定義のみの機能（ランク付けや検索用）
        reg.register(Box::new(NameIndexingFunction));
        reg.register(Box::new(KindIndexingFunction));
        reg.register(Box::new(ContentIndexingFunction));

        reg
    }

    /// 全ての登録済み関数への参照を返します。
    pub fn all_functions(&self) -> &[Box<dyn IndexingFunction>] {
        &self.functions
    }

    // --- Indexing Support ---

    /// 登録されている全機能からデータベースのカラム定義を取得します。
    pub fn get_all_columns(&self) -> Vec<ColumnDef> {
        let mut cols = Vec::new();
        for func in &self.functions {
            if let Some(tagger) = func.tagger() {
                cols.extend(tagger.get_columns());
            }
        }
        cols
    }

    /// 指定されたファイルパスに対してタグ付けを実行し、1行分のデータを返します。
    pub fn process_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let mut row = Vec::new();
        for func in &self.functions {
            if let Some(tagger) = func.tagger() {
                let values = tagger.tag_file(path)?;
                row.extend(values);
            }
        }
        Ok(row)
    }

    // --- Search Support ---

    /// `all_tags` ビューに対する検索SQL（IDのリストを返すクエリ）を生成します。
    pub fn generate_view_query(
        &self,
        node: &crate::query::QueryNode,
        view_name: &str,
    ) -> String {
        let registry = crate::query::QueryFunctionRegistry::with_standard();
        let select = node.clone().expand(&registry).to_sql(view_name);
        select.to_string(PostgresQueryBuilder)
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
    /// キャッシュマネージャ
    cache_manager: CacheManager,
}

impl FileManager {
    pub fn get_connection(&self) -> &Connection {
        &self.conn
    }
    /// デフォルト設定で `FileManager` を作成します。
    pub fn new() -> Result<Self> {
        let home = get_ttfm_home()?;
        let plugins_dir = home.join("plugins");

        // ホームディレクトリの準備
        if !plugins_dir.exists() {
            std::fs::create_dir_all(&plugins_dir).with_context(|| {
                format!(
                    "Failed to create plugins directory at {:?}",
                    plugins_dir
                )
            })?;
        }

        // デフォルトプラグインの展開
        let mimetype_path = plugins_dir.join("mimetype_plugin.component.wasm");
        if !mimetype_path.exists() {
            let bytes =
                include_bytes!("../plugins/mimetype_plugin.component.wasm");
            std::fs::write(&mimetype_path, bytes).with_context(|| {
                format!("Failed to setup default plugin at {:?}", mimetype_path)
            })?;
        }

        Self::new_with_db_dir(home.join("db"))
    }

    /// 指定されたデータベースディレクトリで `FileManager` を作成します。
    pub fn new_with_db_dir<P: AsRef<Path>>(db_dir: P) -> Result<Self> {
        let db_dir = db_dir.as_ref().to_path_buf();

        // データベースディレクトリを作成（存在しない場合）
        if !db_dir.exists() {
            std::fs::create_dir_all(&db_dir).context(format!(
                "Failed to create database directory: {:?}",
                db_dir
            ))?;
        }

        let cache_dir = db_dir.join("cache");
        // デフォルトのキャッシュ上限は 3GB
        let cache_manager =
            CacheManager::new(cache_dir, 3 * 1024 * 1024 * 1024);

        let conn = Connection::open_in_memory()
            .context("Failed to open in-memory database connection")?;

        let registry = FunctionRegistry::with_standard();

        // Initialize tables and views
        let indexer =
            crate::indexing::Indexer::new(&conn, &registry, db_dir.clone());
        indexer
            .initialize_tables()
            .context("Failed to initialize database tables")?;

        Ok(Self {
            conn,
            db_dir,
            registry,
            cache_manager,
        })
    }

    // 互換性のためのエイリアス
    pub fn new_with_index_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::new_with_db_dir(path)
    }

    /// データベースファイルを物理的に削除します。
    /// インスタンス化を行わずに実行できるため、データベース破損時の復旧に使用できます。
    pub fn delete_database() -> Result<()> {
        let home = get_ttfm_home()?;
        let db_dir = home.join("db");

        if db_dir.exists() {
            std::fs::remove_dir_all(&db_dir).with_context(|| {
                format!("Failed to remove database directory: {:?}", db_dir)
            })?;
        }
        Ok(())
    }

    /// ターゲットテーブルに対応するパスを生成します。
    pub fn path_for_target(&self, target: TargetTable) -> std::path::PathBuf {
        self.db_dir.join(format!("{}.parquet", target))
    }

    /// テストなどの用途向けにインメモリでのみ動作する `FileManager` を作成します。
    pub fn new_in_memory() -> Result<Self> {
        Self::new()
    }

    /// 指定されたディレクトリを再帰的にスキャンし、インデックスを作成します。
    pub fn index_directory<P: AsRef<Path>, F>(
        &self,
        root_path: P,
        on_progress: Option<&F>,
        dry_run: bool,
    ) -> Result<usize>
    where
        F: Fn(usize) + Sync + Send,
    {
        let indexer = crate::indexing::Indexer::new(
            &self.conn,
            &self.registry,
            self.db_dir.clone(),
        );
        indexer.run(root_path, on_progress, dry_run)
    }

    pub fn clear_index(&self) -> Result<()> {
        if self.db_dir.exists() {
            std::fs::remove_dir_all(&self.db_dir)
                .context("Failed to completely clear database directory")?;
        }
        // 次回の初期化のために空のディレクトリを再作成
        std::fs::create_dir_all(&self.db_dir)
            .context("Failed to recreate clean database directory")?;
        Ok(())
    }

    /// 新しいアイテム（Type, Label, Note等）をデータベースに追加します。
    pub fn add_item(&self, kind: &str, content: &str) -> Result<i64> {
        let path = self.path_for_target(TargetTable::ItemReferences);
        if !path.exists() {
            return Err(anyhow::anyhow!(
                "Item entities table not found. Please run index first."
            ));
        }

        // 1. Get current min ID
        let path_str = path.to_string_lossy();
        let query_min = Query::select()
            .expr(Expr::col(Col::ItemId).min())
            .from_subquery(util::parquet_query(&path_str), Tbl::ItemReferences)
            .to_string(PostgresQueryBuilder);

        let min_id: i64 = self
            .conn
            .query_row(&query_min, [], |r| r.get(0))
            .unwrap_or(0);
        let new_id = if min_id > -1 { -1 } else { min_id - 1 };

        // 2. Append new row via Temp Table & COPY
        let temp_table = Tbl::Item;

        util::parquet_query(&path_str)
            .create_table_as(&self.conn, temp_table)?;

        // INSERT INTO ...
        Query::insert()
            .into_table(temp_table)
            .columns([Col::ItemId, Col::ItemKind, Col::Content])
            .values_panic([new_id.into(), kind.into(), content.into()])
            .execute(&self.conn)?;

        temp_table.write_parquet(&self.conn, &path)?;
        temp_table.drop_table(&self.conn)?;

        self.refresh_view()?;
        Ok(new_id)
    }

    /// アイテム（ファイルまたは Item Entity）にタグを付与します。
    pub fn tag_item(&self, item: &str, tag_str: &str) -> Result<()> {
        let (key, value) = tag_str
            .split_once(':')
            .context("Tag must be in 'key:value' format")?;

        // 1. タグ自体の Item Entity が存在することを確認（なければ作成）
        self.get_or_create_item("type", key)?;
        // label はデフォルトで揮発性のため、エンティティ作成をスキップ
        self.get_or_create_item("tag", tag_str)?;

        // 2. ターゲットの ID を特定
        let item_id = if let Ok(id) = item.parse::<i64>() {
            id
        } else {
            // A. パスとして扱い、locations から ID を取得
            let query_path = Query::select()
                .column(Col::ItemId)
                .from_subquery(
                    util::parquet_query(
                        &self
                            .path_for_target(TargetTable::Locations)
                            .to_string_lossy(),
                    ),
                    Tbl::Locations,
                )
                .and_where(Expr::col(Col::Path).eq(item))
                .to_string(PostgresQueryBuilder);

            if let Ok(id) = self.conn.query_row(&query_path, [], |r| r.get(0)) {
                id
            } else {
                // B. 名前（抽象化された名称）として扱い、all_tags から ID を取得
                let query_name = Query::select()
                    .column(Col::ItemId)
                    .from(Tbl::OneView)
                    .and_where(Expr::col(Col::Type).eq("name"))
                    .and_where(Expr::col(Col::LabelStr).eq(item))
                    .to_string(PostgresQueryBuilder);

                self.conn.query_row(&query_name, [], |r| r.get(0)).context(
                    format!("Item not found by path or name: {}", item),
                )?
            }
        };

        // 3. User Tags テーブルに保存
        self.append_tag_to_parquet(
            self.path_for_target(TargetTable::UserTags),
            Tbl::UserTagsDiff,
            Col::ItemId,
            item_id,
            key,
            value,
        )?;

        self.refresh_view()?;

        Ok(())
    }

    /// 検索結果リストに対して優先度を一括設定します。
    pub fn update_ranks(
        &self,
        results: &[SearchResult],
        rank: i64,
    ) -> Result<()> {
        let file_ids: Vec<i64> = results
            .iter()
            .filter(|r| r.item_kind == crate::types::ItemKind::File)
            .map(|r| r.id.as_i64())
            .collect();
        let item_ids: Vec<i64> = results
            .iter()
            .filter(|r| r.item_kind != crate::types::ItemKind::File)
            .map(|r| r.id.as_i64())
            .collect();

        if !file_ids.is_empty() {
            self.batch_update_rank(&file_ids, true, rank)?;
        }
        if !item_ids.is_empty() {
            self.batch_update_rank(&item_ids, false, rank)?;
        }
        self.refresh_view()?;
        Ok(())
    }

    fn batch_update_rank(
        &self,
        ids: &[i64],
        is_file: bool,
        rank: i64,
    ) -> Result<()> {
        let path = if is_file {
            self.path_for_target(TargetTable::FileReferences)
        } else {
            self.path_for_target(TargetTable::ItemReferences)
        };

        let path_str = path.to_string_lossy();
        let temp_table = Tbl::Target;

        util::parquet_query(&path_str)
            .create_table_as(&self.conn, temp_table)?;

        Query::update()
            .table(temp_table)
            .values([(Col::Rank, rank.into())])
            .and_where(
                Expr::col(Col::ItemId).is_in(
                    ids.iter()
                        .cloned()
                        .map(sea_query::Value::from)
                        .collect::<Vec<_>>(),
                ),
            )
            .execute(&self.conn)?;

        temp_table.write_parquet(&self.conn, &path)?;
        temp_table.drop_table(&self.conn)?;

        Ok(())
    }

    /// IDを指定して優先度を設定します。
    pub fn set_rank_by_id(
        &self,
        id: i64,
        is_file: bool,
        rank: i64,
    ) -> Result<()> {
        self.batch_update_rank(&[id], is_file, rank)
    }

    /// 全てのタグ型の優先度（RANK）を取得します。
    pub fn get_type_ranks(
        &self,
    ) -> Result<std::collections::HashMap<String, i64>> {
        let path = self.path_for_target(TargetTable::ItemReferences);
        if !path.exists() {
            return Ok(Default::default());
        }

        let query = Query::select()
            .column(Col::Content)
            .column(Col::Rank)
            .from_subquery(
                util::parquet_query(&path.to_string_lossy()),
                Tbl::ItemReferences,
            )
            .and_where(Expr::col(Col::ItemKind).eq("type"))
            .to_string(PostgresQueryBuilder);

        let mut stmt = self.conn.prepare(&query)?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;

        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (name, rank) = row?;
            map.insert(name, rank);
        }
        Ok(map)
    }

    /// 指定されたタグ名のデフォルトランクを取得します。
    pub fn get_default_rank(&self, name: &str) -> crate::types::Rank {
        crate::rank::get_rank_by_name(&self.registry, name)
    }

    pub fn get_or_create_item(&self, kind: &str, content: &str) -> Result<i64> {
        let path = self.path_for_target(TargetTable::ItemReferences);
        let query = Query::select()
            .column(Col::ItemId)
            .from_subquery(
                util::parquet_query(&path.to_string_lossy()),
                Tbl::ItemReferences,
            )
            .and_where(Expr::col(Col::ItemKind).eq(kind))
            .and_where(Expr::col(Col::Content).eq(content))
            .to_string(PostgresQueryBuilder);

        if let Ok(id) = self.conn.query_row(&query, [], |r| r.get(0)) {
            Ok(id)
        } else {
            self.add_item(kind, content)
        }
    }

    fn append_tag_to_parquet(
        &self,
        path: std::path::PathBuf,
        temp_table: Tbl,
        id_col: Col,
        id: i64,
        key: &str,
        value: &str,
    ) -> Result<()> {
        let path_str = path.to_string_lossy();

        util::parquet_query(&path_str)
            .create_table_as(&self.conn, temp_table)?;

        // Type inference for user tags
        let val_i64 = value.parse::<i64>().ok();
        let val_f64 = value.parse::<f64>().ok();
        let val_bool = value.parse::<bool>().ok();

        // INSERT INTO ...
        Query::insert()
            .into_table(temp_table)
            .columns([
                id_col,
                Col::Type,
                Col::LabelStr,
                Col::LabelInt,
                Col::LabelDouble,
                Col::LabelBool,
            ])
            .values_panic([
                id.into(),
                key.into(),
                Some(value).into(), // Always populate label_str
                val_i64.into(),
                val_f64.into(),
                val_bool.into(),
            ])
            .execute(&self.conn)?;

        let query = Query::select()
            .column(sea_query::Asterisk)
            .from(temp_table.clone())
            .order_by(Col::Type, sea_query::Order::Asc)
            .order_by(Col::LabelInt, sea_query::Order::Asc)
            .order_by(Col::LabelStr, sea_query::Order::Asc)
            .order_by(Col::ItemId, sea_query::Order::Asc)
            .to_owned();

        util::save_parquet(&self.conn, &query, &path, None)?;
        temp_table.drop_table(&self.conn)?;

        Ok(())
    }

    /// 指定されたディレクトリからWasmプラグインをロードし、レジストリに登録します。
    /// ".wasm" 拡張子を持つファイルを対象とします。
    pub fn load_plugins(
        &mut self,
        dir: impl AsRef<Path>,
        status: &std::collections::HashMap<String, bool>,
    ) -> Result<()> {
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
                        let is_enabled =
                            *status.get(&adapter.name).unwrap_or(&true);
                        if is_enabled {
                            if cfg!(debug_assertions)
                                && std::env::var("TTFM_DEBUG").is_ok()
                            {
                                println!(
                                    "Loaded plugin: {} from {:?}",
                                    adapter.name, path
                                );
                            }
                            self.registry.register(Box::new(adapter));
                        } else {
                            if cfg!(debug_assertions)
                                && std::env::var("TTFM_DEBUG").is_ok()
                            {
                                println!(
                                    "Plugin {} is disabled via config. Skipping.",
                                    adapter.name
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to load plugin {:?}: {}",
                            path, e
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// OneView を再構築し、最新の Parquet ファイルの状態を反映させます。
    pub fn refresh_view(&self) -> Result<()> {
        let all_columns = self.registry.get_all_columns();
        crate::oneview::OneView::recreate(
            &self.conn,
            &all_columns,
            &self.db_dir,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests_file_manager {
    use super::*;
    use duckdb::Connection;
    use tempfile::tempdir;

    fn setup_test_env() -> (FileManager, std::path::PathBuf, tempfile::TempDir)
    {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        let registry = FunctionRegistry::with_standard();

        // Initialize tables
        let indexer =
            crate::indexing::Indexer::new(&conn, &registry, db_dir.clone());
        indexer.initialize_tables().unwrap();

        let cache_manager = CacheManager::new(db_dir.join("cache"), 0);

        (
            FileManager {
                conn,
                db_dir: db_dir.clone(),
                registry,
                cache_manager,
            },
            db_dir,
            dir,
        )
    }

    #[test]
    fn test_user_tags_sorting() {
        let (fm, db_dir, _dir) = setup_test_env();

        // Manually create empty user_tags.parquet to ensure existence
        let path = db_dir.join("user_tags.parquet");
        fm.conn.execute("CREATE TABLE temp_create (item_id BIGINT, type VARCHAR, label_str VARCHAR, label_int BIGINT, label_double DOUBLE, label_bool BOOLEAN)", []).unwrap();
        fm.conn
            .execute(
                &format!(
                    "COPY temp_create TO '{}' (FORMAT PARQUET)",
                    path.to_string_lossy()
                ),
                [],
            )
            .unwrap();
        fm.conn.execute("DROP TABLE temp_create", []).unwrap();

        let id = -100; // Dummy ID

        fm.append_tag_to_parquet(
            fm.path_for_target(TargetTable::UserTags),
            Tbl::UserTagsDiff,
            Col::ItemId,
            id,
            "type_z",
            "val_1",
        )
        .unwrap();

        fm.append_tag_to_parquet(
            fm.path_for_target(TargetTable::UserTags),
            Tbl::UserTagsDiff,
            Col::ItemId,
            id,
            "type_a",
            "val_2",
        )
        .unwrap();

        let path = fm.path_for_target(TargetTable::UserTags);
        let rows: Vec<String> = fm
            .conn
            .prepare(&format!(
                "SELECT type FROM read_parquet('{}')",
                path.to_string_lossy()
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            rows,
            vec!["type_a", "type_z"],
            "User tags should be sorted by type"
        );
    }
}
