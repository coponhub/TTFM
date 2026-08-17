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

pub mod diff;
pub mod indexer;
pub mod merge;
pub mod scan;
pub mod triage;

pub use indexer::Indexer;
pub use indexer::{DynamicRow, TagRow, TaggingResult};

crate::define_scan_entry! {
    path:  crate::tag::PathFn,
    inode: crate::tag::FileIdFn,
    size:  crate::tag::SizeFn,
    mtime: crate::tag::MtimeFn,
}

pub(crate) fn append_to_target(
    conn: &duckdb::Connection,
    store: &crate::db::Store,
    target: crate::db::TargetTable,
    rows: sea_query::SelectStatement,
) -> anyhow::Result<()> {
    let path = store.path_for_target(target);
    union_and_save(conn, &path, rows, |_| {})
}

pub(crate) fn union_and_save(
    conn: &duckdb::Connection,
    path: &std::path::Path,
    rows: sea_query::SelectStatement,
    retain: impl FnOnce(&mut sea_query::SelectStatement),
) -> anyhow::Result<()> {
    use crate::util::ParquetExt;

    if !path.exists() {
        return rows.save_parquet(conn, path);
    }

    let mut existing = crate::util::parquet_query(&path.to_string_lossy());
    retain(&mut existing);

    existing
        .union(sea_query::UnionType::All, rows)
        .save_parquet(conn, path)
}
