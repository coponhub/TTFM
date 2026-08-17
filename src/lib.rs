// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

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
pub mod types;
pub mod util;

pub use db::{ColumnDef, Store, TargetTable};
pub use query::{parse, QueryNode};
pub use response::{Item, SearchResponse};
pub use types::{
    FileRef, ItemKind, Label, Origin, Progress, TagType, TypedTag,
};

mod cache;
pub use cache::CacheManager;
pub mod search;
pub use search::SearchOptions;
pub mod edit;
pub mod tagging;
pub use rank::{get_type_ranks, set_rank_by_id};
pub use tagging::{add_item, get_or_create_item};

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
