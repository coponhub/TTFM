//! # TTFM (Typed Tag File Manager) Core Library
//!
//! このライブラリは、Typed Tag（型付きタグ）を用いたファイル管理システムのコア機能を提供します。
//! DuckDBをバックエンドに使用し、Parquet形式でのインデックス保存と高速な検索を実現します。

use anyhow::{Context, Result};
use file_id::get_file_id;
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
pub mod tag;
mod taggers;
pub mod types;
pub mod util;

pub use db::{Store, TargetTable};
pub use query::{parse, QueryNode};
pub use response::{SearchResponse, SearchResult};
pub use taggers::{ColumnDef, TagValue, Tagger};
pub use types::{FileRef, ItemKind, Label, Progress, TagType, TypedTag};

mod cache;
pub use cache::CacheManager;
pub mod search;
pub use search::SearchOptions;
pub mod tagging;
pub use tagging::{add_item, get_or_create_item, tag_item};
pub use rank::{get_type_ranks, set_rank_by_id, update_ranks};

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

#[cfg(test)]
mod tests_store {
    use crate::db::{Col, Tbl};
    use crate::tagging;
    use tempfile::tempdir;

    fn setup_test_env() -> (
        crate::db::Store,
        crate::tag::TagRegistry,
        crate::CacheManager,
        std::path::PathBuf,
        tempfile::TempDir,
    ) {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let store = crate::db::Store::open(&db_dir).unwrap();
        let registry = crate::tag::TagRegistry::with_standard();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();
        let cache = crate::CacheManager::new(db_dir.join("cache"), 0);
        (store, registry, cache, db_dir, dir)
    }

    #[test]
    fn test_user_tags_sorting() {
        let (store, _registry, _cache, db_dir, _dir) = setup_test_env();

        // Manually create empty user_tags.parquet to ensure existence
        let path = db_dir.join("user_tags.parquet");
        store.conn.execute("CREATE TABLE temp_create (item_id BIGINT, type VARCHAR, label_str VARCHAR, label_int BIGINT, label_double DOUBLE, label_bool BOOLEAN)", []).unwrap();
        store.conn
            .execute(
                &format!(
                    "COPY temp_create TO '{}' (FORMAT PARQUET)",
                    path.to_string_lossy()
                ),
                [],
            )
            .unwrap();
        store.conn.execute("DROP TABLE temp_create", []).unwrap();

        let id = -100; // Dummy ID

        tagging::append_tag_to_parquet(
            &store,
            path.clone(),
            Tbl::UserTagsDiff,
            Col::ItemId,
            id,
            "type_z",
            "val_1",
        )
        .unwrap();

        tagging::append_tag_to_parquet(
            &store,
            path.clone(),
            Tbl::UserTagsDiff,
            Col::ItemId,
            id,
            "type_a",
            "val_2",
        )
        .unwrap();

        let rows: Vec<String> = store
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
