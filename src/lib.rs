//! # TTFM (Typed Tag File Manager) Core Library
//!
//! このライブラリは、Typed Tag（型付きタグ）を用いたファイル管理システムのコア機能を提供します。
//! DuckDBをバックエンドに使用し、Parquet形式でのインデックス保存と高速な検索を実現します。

use crate::db::{Col, Tbl};
use crate::util::{DotOk, ExecuteSql, IdenExt, SelectExt};
use anyhow::{Context, Result};
use duckdb::Connection;
use file_id::get_file_id;
use sea_query::{Expr, JoinType, PostgresQueryBuilder, Query};
use std::path::Path;

pub mod config;
pub mod db;
mod functions;
pub mod indexing;
pub mod macros;
pub mod oneview;
pub mod plugins;
pub mod query;
pub mod query_functions;
pub mod rank;
mod taggers;
pub mod types;
pub mod util;

pub use db::TargetTable;
use functions::{
    ContentTagFunction, DirectoryFunction, ExtensionFunction, FilenameFunction,
    InodeFunction, KindTagFunction, ModifiedStrFunction, ModifiedTsFunction,
    NameTagFunction, ParentDirFunction, PathFunction, SizeBytesFunction,
    SizeStrFunction, StemFunction, TagFunction, TypeFromExtFunction,
};
pub use query::{parse, QueryNode};
pub use taggers::{ColumnDef, TagValue, Tagger};
pub use types::{FileRef, Label, SearchResult, TagType, TypedTag};

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
        Self {
            functions: Vec::new(),
        }
    }

    /// 新しい機能（`TagFunction`）をレジストリに追加します。
    /// 同名の機能が既に登録されている場合はスキップします。
    pub fn register(&mut self, func: Box<dyn TagFunction>) {
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
        reg.register(Box::new(NameTagFunction));
        reg.register(Box::new(KindTagFunction));
        reg.register(Box::new(ContentTagFunction));

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
}

impl FileManager {
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

    /// クエリ文字列を使用してインデックスを検索し、結果のリストを返します。
    pub fn search(&self, query: &str) -> Result<types::SearchResponse> {
        if !self.path_for_target(TargetTable::FileReferences).exists() {
            return Err(anyhow::anyhow!(
                "Index not found. Please run 'index' command first."
            ));
        }

        let mut projections = Vec::new();

        // 1. 検索条件にマッチするIDを抽出するクエリ
        let sub_query = if query.trim().is_empty() {
            Query::select()
                .column(Col::ItemId)
                .distinct()
                .from(Tbl::OneView)
                .to_owned()
        } else {
            let node = crate::query::parse(query)?;
            let q_reg = crate::query::QueryFunctionRegistry::with_standard();
            let expanded = node.expand(&q_reg);
            projections = expanded.get_projections();
            expanded.to_sql("oneview")
        };

        // DuckDBにおいて、複雑なビューに対する集計(MAXなど)と
        // 相関サブクエリやIN/EXISTSを組み合わせた際、「WHERE句に集計関数を含められない」
        // といった意図しないオプティマイザの挙動を回避するため、一時テーブルに隔離する。
        Tbl::Sub.drop_table(&self.conn).ok();
        sub_query.create_table_as(&self.conn, Tbl::Sub)?;

        // 2. マッチしたIDの全タグを取得して集計
        // アイテムごとの全タグを集約した CTE
        let mut tag_agg = Query::select();
        tag_agg
            .column(Col::ItemId)
            .expr_as(
                crate::db::CustomFunc::max_filter(
                    Expr::col(Col::LabelStr),
                    Expr::col(Col::Type).eq("item_kind"),
                ),
                Col::ItemKind,
            )
            .expr_as(
                crate::db::CustomFunc::max_filter(
                    Expr::col(Col::LabelInt),
                    Expr::col(Col::Type).eq("rank"),
                ),
                Col::Rank,
            );

        for col in crate::db::Col::tag_value_columns() {
            let source = if col == crate::db::Col::Types {
                crate::db::Col::Type
            } else {
                col
            };
            tag_agg
                .expr_as(crate::db::CustomFunc::list(Expr::col(source)), col);
        }

        tag_agg
            .from(Tbl::OneView)
            .and_where(
                Expr::col((Tbl::OneView, Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from(Tbl::Sub)
                        .to_owned(),
                ),
            )
            .group_by_col(Col::ItemId);

        // 各アイテムの「名前(場所)」ごとに独立した結果行を生成
        let mut query = Query::select();
        query
            .column((Tbl::Identities, Col::ItemId))
            .column((Tbl::AggTags, Col::ItemKind))
            .column((Tbl::Identities, Col::Name))
            .column((Tbl::AggTags, Col::Rank));

        for col in crate::db::Col::tag_value_columns() {
            query.column((Tbl::AggTags, col));
        }

        query
            .from_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .expr_as(Expr::col(Col::LabelStr), Col::Name)
                    .from(Tbl::OneView)
                    .and_where(Expr::col(Col::Type).eq("name"))
                    .to_owned(),
                Tbl::Identities,
            )
            .join_subquery(
                JoinType::InnerJoin,
                tag_agg,
                Tbl::AggTags,
                Expr::col((Tbl::Identities, Col::ItemId))
                    .eq(Expr::col((Tbl::AggTags, Col::ItemId))),
            )
            .order_by_expr(
                Expr::col((Tbl::AggTags, Col::Rank)).into(),
                sea_query::Order::Desc,
            )
            .limit(100);

        let sql = query.to_string(PostgresQueryBuilder);
        let mut stmt = self.conn.prepare(&sql)?;

        let c_id: &str = Col::ItemId.into();
        let c_kind: &str = Col::ItemKind.into();
        let c_name: &str = Col::Name.into();
        let c_rank: &str = Col::Rank.into();

        let tag_value_cols: Vec<String> = crate::db::Col::tag_value_columns()
            .into_iter()
            .map(|c| c.to_string())
            .collect();

        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(c_id)?;
            // Aggregation result might be Null if no tag found, handle gracefully
            let item_kind: String =
                row.get(c_kind).unwrap_or_else(|_| "unknown".to_string());
            let name: String = row.get(c_name).unwrap_or_default();
            let rank: i64 = row.get(c_rank).unwrap_or(0);

            use duckdb::types::Value;

            fn to_vec(v: Value) -> Vec<Value> {
                if let Value::List(vec) = v {
                    vec
                } else {
                    Vec::new()
                }
            }

            let tag_lists: Vec<Vec<Value>> = tag_value_cols
                .iter()
                .map(|col_name| {
                    to_vec(row.get(col_name as &str).unwrap_or(Value::Null))
                })
                .collect();

            let mut intrinsic = types::Intrinsic::default();
            let mut tags = types::Tags::new();

            // カラム定義から対象のリストを取得するヘルパー
            let get_list = |st: crate::db::Col| -> Option<&Vec<Value>> {
                let col_name = st.to_string();
                let idx = tag_value_cols.iter().position(|c| c == &col_name)?;
                tag_lists.get(idx)
            };

            // 型名のリスト（Types カラム）を起点に走査
            let Some(types_list) = get_list(crate::db::Col::Types) else {
                return Ok(types::SearchResult {
                    id,
                    item_kind,
                    name,
                    rank,
                    intrinsic,
                    tags,
                });
            };

            // 1. データ抽出ヘルパーの構成
            let extract_tag_type = |n: types::TagNumber| {
                get_list(crate::db::Col::Types)
                    .and_then(|l| l.get(n))
                    .and_then(|v| match v {
                        Value::Text(s) => Some(types::TagType::from(s.clone())),
                        Value::BigInt(n_val) => {
                            Some(types::TagType::from(n_val.to_string()))
                        }
                        _ => None,
                    })
            };

            let extract_origin =
                |n: types::TagNumber| match get_list(crate::db::Col::Origin)
                    .and_then(|l| l.get(n))
                {
                    Some(Value::Text(s)) if s == "system" => {
                        types::Origin::System
                    }
                    _ => types::Origin::User,
                };

            let extract_label = |n: types::TagNumber| {
                crate::db::Col::typed_label_columns().into_iter().find_map(
                    |col| {
                        get_list(col).and_then(|l| l.get(n)).and_then(|v| {
                            match v {
                                Value::Text(s) => {
                                    Some(types::Label::String(s.clone()))
                                }
                                Value::BigInt(n_val) => {
                                    Some(types::Label::Integer(*n_val))
                                }
                                Value::Double(d) => {
                                    Some(types::Label::String(d.to_string()))
                                }
                                Value::Boolean(b) => {
                                    Some(types::Label::String(b.to_string()))
                                }
                                _ => None,
                            }
                        })
                    },
                )
            };

            // 2. 特殊処理: TypedTag の抽出
            let extract_typedtag =
                |n: types::TagNumber, origin, tags: &mut types::Tags| {
                    if let Some(Value::Text(tt_val)) =
                        get_list(crate::db::Col::TypedTag)
                            .and_then(|l| l.get(n))
                    {
                        tags.entry(types::TagType::Base(
                            types::SType::TypedTag,
                        ))
                        .or_default()
                        .push(types::TagValue {
                            label: types::Label::String(tt_val.clone()),
                            origin,
                        });
                    }
                };

            // 3. 振り分けヘルパー
            let dispatch_tag =
                |tag_type: types::TagType,
                 label: types::Label,
                 origin: types::Origin,
                 intrinsic: &mut types::Intrinsic,
                 tags: &mut types::Tags| {
                    match &tag_type {
                        types::TagType::Base(types::SType::Size) => {
                            intrinsic.size =
                                Some(types::FileSize(label.as_i64()))
                        }
                        types::TagType::Base(types::SType::Mtime) => {
                            intrinsic.mtime =
                                Some(types::FileTimestamp(label.as_i64()))
                        }
                        types::TagType::Base(types::SType::Hash) => {
                            intrinsic.hash = Some(label.as_str())
                        }
                        types::TagType::Base(types::SType::TypedTag) => {}
                        _ => {
                            tags.entry(tag_type)
                                .or_default()
                                .push(types::TagValue { label, origin });
                        }
                    }
                };

            // 4. メインループ: 走査と振り分け
            for i in 0..types_list.len() {
                let n = i as types::TagNumber;
                let (Some(tag_type), origin, Some(label)) =
                    (extract_tag_type(n), extract_origin(n), extract_label(n))
                else {
                    continue;
                };

                // 特殊処理
                extract_typedtag(n, origin, &mut tags);

                // 振り分け
                dispatch_tag(
                    tag_type,
                    label,
                    origin,
                    &mut intrinsic,
                    &mut tags,
                );
            }

            types::SearchResult {
                id,
                item_kind,
                name,
                rank,
                intrinsic,
                tags,
            }
            .to_ok()
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }

        Ok(types::SearchResponse {
            results,
            projections,
        })
    }

    /// インデックスディレクトリ全体を削除し、完全にクリーンな状態にします。
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

        Ok(new_id)
    }

    /// アイテム（ファイルまたは Item Entity）にタグを付与します。
    pub fn tag_item(&self, item: &str, tag_str: &str) -> Result<()> {
        let (key, value) = tag_str
            .split_once(':')
            .context("Tag must be in 'key:value' format")?;

        // 1. タグ自体の Item Entity が存在することを確認（なければ作成）
        self.get_or_create_item("type", key)?;
        self.get_or_create_item("label", value)?;
        self.get_or_create_item("typedtag", tag_str)?;

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
            .filter(|r| r.item_kind == "file")
            .map(|r| r.id)
            .collect();
        let item_ids: Vec<i64> = results
            .iter()
            .filter(|r| r.item_kind != "file")
            .map(|r| r.id)
            .collect();

        if !file_ids.is_empty() {
            self.batch_update_rank(&file_ids, true, rank)?;
        }
        if !item_ids.is_empty() {
            self.batch_update_rank(&item_ids, false, rank)?;
        }
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

        temp_table.write_parquet(&self.conn, &path)?;
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
                            println!(
                                "Loaded plugin: {} from {:?}",
                                adapter.name, path
                            );
                            self.registry.register(Box::new(adapter));
                        } else {
                            println!(
                                "Plugin {} is disabled via config. Skipping.",
                                adapter.name
                            );
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
}
