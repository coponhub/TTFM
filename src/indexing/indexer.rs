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

use crate::db::{Col, Store, TargetTable, Tbl};
// use crate::db::{identifier, DuckDbFunc, Pronoun::*};
use crate::db::ColumnDef;
use crate::indexing::ScanEntry;
use crate::tag::TagRegistry;
use crate::types::{Biticals, ItemId};
// use crate::types::Origin;
use crate::util::{self, ExecuteSql, IdenExt, SelectExt};
// use crate::util::ParquetExt;
use anyhow::{Context, Result};
use duckdb::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rustc_hash::FxHashMap;
use sea_query::{Expr, PostgresQueryBuilder, Query, Table};
// use sea_query::{ExprTrait, Func};
use std::path::Path;

use super::diff;
use super::merge;
// use super::merge::MergeQueryParts;
use super::scan;
use super::triage;

// ========================================================
// Shared Data Structures
// ========================================================

pub struct TaggingResult {
    pub entity_row: DynamicRow,
    pub location_row: DynamicRow,
    pub tags: Vec<TagRow>,
    pub scan_hash: ScanHash,
}

pub struct DynamicRow {
    pub id: i64,
    pub values: Biticals,
}

#[derive(Debug, PartialEq)]
pub struct TagRow {
    pub item_id: i64,
    pub tag_type: String,
    pub label_str: Option<String>,
    pub label_int: Option<i64>,
    pub label_double: Option<f64>,
    pub label_bool: Option<bool>,
}

// ========================================================
// Main Indexer (The Orchestrator)
// ========================================================

pub struct Indexer<'a> {
    pub(crate) store: &'a Store,
    pub(crate) registry: &'a TagRegistry,
}

impl<'a> Indexer<'a> {
    pub fn new(store: &'a Store, registry: &'a TagRegistry) -> Self {
        Self { store, registry }
    }

    /// インデックス作成の全体ワークフローを実行します。
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
        // 既存のハッシュをロード
        let cache = self.load_metadata_cache()?;

        // 1. Scan Phase
        let count = scan::run_scan(
            &self.store.conn,
            &self.store.db_dir,
            &self.store.temp_scan_path(),
            &self.store.temp_live_path(),
            root_path.as_ref(),
            &cache,
            on_progress,
            dry_run,
        )?;

        if dry_run {
            return Ok(count);
        }

        // 2. Diff Phase
        let diff = diff::run_diff(&self.store.conn, &self.store)?;

        // 3. Triage Phase
        let (results, moved_rows) =
            triage::run_triage(&self.store, self.registry, diff.to_process)?;

        // 4. Merge Phase
        merge::run_merge(
            &self.store.conn,
            self.registry,
            &self.store,
            results,
            moved_rows,
            diff.deleted_ids,
            &self.store.temp_scan_path(),
            &self.store.temp_live_path(),
            // |data| self.update_system_items(data),
            |_data| Ok(()),
        )?;

        Ok(count)
    }

    /// 既存のインデックスから変更検知用のメタデータ・キャッシュをロードします。
    pub fn load_metadata_cache(&self) -> Result<FxHashMap<ScanHash, ItemId>> {
        let locs_path = self.store.path_for_target(TargetTable::Locations);
        if !locs_path.exists() {
            return Ok(FxHashMap::default());
        }

        let locs_str = locs_path.to_string_lossy();

        // item_id と scan_hash カラムを取得
        let sql = Query::select()
            .column(Col::ItemId)
            .column(Col::ScanHash)
            .from_subquery(util::parquet_query(&locs_str), Tbl::Locations)
            .to_string(PostgresQueryBuilder);

        let mut stmt = self
            .store
            .conn
            .prepare(&sql)
            .context("Failed to prepare cache load query")?;

        // カラムが存在しない初回の実行などに対応するため、安全にハンドル
        let rows = match stmt.query_map([], |row| {
            Ok((row.get::<_, ItemId>(0)?, row.get::<_, ScanHash>(1)?))
        }) {
            Ok(r) => r,
            Err(_) => return Ok(FxHashMap::default()),
        };

        let mut cache = FxHashMap::default();
        for res in rows {
            if let Ok((id, h)) = res {
                cache.insert(h, id);
            }
        }

        Ok(cache)
    }

    /// データベーステーブルとビューの初期化を行います。
    pub fn initialize_tables(&self) -> Result<()> {
        let all_cols = self.registry.get_all_columns();
        use strum::IntoEnumIterator;

        for target in TargetTable::iter() {
            let path = self.store.path_for_target(target);
            self.ensure_empty_parquet_if_missing(&path, target, &all_cols)?;
        }

        let reader = crate::query::lens_reader::Reader::build(
            self.registry,
            crate::db::Tbl::_OneView,
        );
        crate::oneview::OneView::recreate(
            &self.store.conn,
            &all_cols,
            reader,
            &self.store.db_dir,
        )?;

        self.ensure_data_types()?;
        // self.update_system_items(None)?;
        Ok(())
    }

    /// `data_types` テーブルに初期データを投入します。
    fn ensure_data_types(&self) -> Result<()> {
        let path = self.store.path_for_target(TargetTable::DataTypes);
        if !path.exists() {
            // initialize_tables で空ファイルは作られているはずだが、念のため
            return Ok(());
        }

        // すでにデータがあるかチェック
        let count_sql = Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from_subquery(
                util::parquet_query(&path.to_string_lossy()),
                Tbl::DataTypes,
            )
            .to_string(PostgresQueryBuilder);

        let count: i64 = self
            .store
            .conn
            .query_row(&count_sql, [], |r| r.get(0))
            .unwrap_or(0);

        if count > 0 {
            return Ok(());
        }

        // デフォルトのデータ型定義を挿入
        use crate::db::BiticalType;
        let defaults = vec![
            ("size", BiticalType::Integer),
            ("mtime", BiticalType::Integer),
            ("rank", BiticalType::Integer),
            ("name", BiticalType::String),
            ("kind", BiticalType::String),
            ("content", BiticalType::String),
            // 将来的にはここで標準タグも追加
            ("filename", BiticalType::String),
            ("extension", BiticalType::String),
            ("path", BiticalType::String),
            ("parent_dir", BiticalType::String),
        ];

        let mut insert = Query::insert();
        insert
            .into_table(Tbl::DataTypes)
            .columns([Col::Type, Col::DataType]);

        for (key, type_) in defaults {
            insert.values_panic([key.into(), (type_ as i32).into()]);
        }

        util::parquet_query(&path.to_string_lossy())
            .create_table_as(&self.store.conn, Tbl::DataTypes)?;

        // Append (though table is empty/newly created, we use insert)
        insert.execute(&self.store.conn)?;

        Tbl::DataTypes.write_parquet(&self.store.conn, &path)?;
        Tbl::DataTypes.drop_table(&self.store.conn)?;

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
        let table = Tbl::Master;
        crate::db::Schema::build_table(target, table, columns)
            .execute(&self.store.conn)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        table.write_parquet(&self.store.conn, path)?;
        Table::drop().table(table).execute(&self.store.conn).ok();
        Ok(())
    }

    //     /// システム定義アイテム（拡張子、タグ型など）をデータベースに登録・更新します。
    //     pub fn update_system_items(
    //         &self,
    //         data_candidates: Option<sea_query::SelectStatement>,
    //     ) -> Result<()> {
    //         let items_path =
    //             self.store.path_for_target(TargetTable::ItemReferences);
    //         let system_tags_path =
    //             self.store.path_for_target(TargetTable::SystemTags);
    //         let items_str = items_path.to_string_lossy();
    //
    //         let mut all_candidates =
    //             MergeQueryParts::registry_variants(self.registry);
    //         if let Some(data) = data_candidates {
    //             all_candidates.union(sea_query::UnionType::Distinct, data);
    //         }
    //
    //         MergeQueryParts::filter_new(all_candidates, &items_str)
    //             .create_temp_table_as(&self.store.conn, Tbl::Item)?;
    //
    //         if self.count_table(Tbl::Item)? == 0 {
    //             Tbl::Item.drop_table(&self.store.conn)?;
    //             return Ok(());
    //         }
    //
    //         let start_id = identifier::next(&self.store, Origin::Builtin, 1)?[0];
    //         let tmp_items = items_path.with_extension("parquet.tmp");
    //         let tmp_stags = system_tags_path.with_extension("parquet.tmp");
    //
    //         MergeQueryParts::assign_ids(start_id)
    //             .create_temp_table_as(&self.store.conn, Tbl::IdItem)?;
    //
    //         // query_union.union(sea_query::UnionType::All, ...);
    //
    //         let mut lhs = Query::select();
    //         lhs.columns(Col::item_references_columns()).from_function(
    //             Func::cust(DuckDbFunc::ReadParquet)
    //                 .arg(Expr::val(items_str.to_string())),
    //             Diff,
    //         );
    //
    //         let mut query_union = lhs;
    //         query_union.union(
    //             sea_query::UnionType::All,
    //             Query::select()
    //                 .columns(Col::item_references_columns())
    //                 .from(Tbl::IdItem)
    //                 .to_owned(),
    //         );
    //         query_union.order_by(Col::ItemId, sea_query::Order::Asc);
    //         query_union.save_parquet(&self.store.conn, &tmp_items)?;
    //         // UNION ALL BY NAME を使用して、既存のタグと新しいメタデータ/ラベルを統合します。
    //         let p1 = Query::select()
    //             .column(sea_query::Asterisk)
    //             .from_subquery(
    //                 crate::util::parquet_query(&system_tags_path.to_string_lossy()),
    //                 Tbl::SystemTags,
    //             )
    //             .to_owned();
    //
    //         // 共通のメタデータタグ (kind)
    //         let p2 = Query::select()
    //             .column(Col::ItemId)
    //             .expr_as(Expr::val("type"), Col::Type)
    //             .expr_as(
    //                 Expr::case(
    //                     Expr::col(Col::ItemKind).eq("tag"),
    //                     Expr::col(Col::Type),
    //                 )
    //                 .finally(Expr::col(Col::ItemKind))
    //                 .cast_as(crate::db::BiticalType::String),
    //                 Col::LabelStr,
    //             )
    //             .from(Tbl::IdItem)
    //             .to_owned();
    //
    //         // 型付きタグのラベル部分
    //         let p3 = Query::select()
    //             .column(Col::ItemId)
    //             .expr_as(Expr::val("label"), Col::Type)
    //             .expr_as(
    //                 Expr::col(Col::Label).cast_as(crate::db::BiticalType::String),
    //                 Col::LabelStr,
    //             )
    //             .from(Tbl::IdItem)
    //             .and_where(Expr::col(Col::ItemKind).eq("tag"))
    //             .to_owned();
    //
    //         let ordered_sql = Self::build_ordered_system_tags_sql(
    //             &p1.to_string(PostgresQueryBuilder),
    //             &p2.to_string(PostgresQueryBuilder),
    //             &p3.to_string(PostgresQueryBuilder),
    //         );
    //
    //         self.store.conn.execute(
    //             &format!(
    //                 "COPY ({}) TO '{}' (FORMAT PARQUET)",
    //                 ordered_sql,
    //                 tmp_stags.to_string_lossy()
    //             ),
    //             [],
    //         )?;
    //
    //         self.finalize_updates(
    //             &items_path,
    //             &system_tags_path,
    //             &tmp_items,
    //             &tmp_stags,
    //         )
    //     }
    //
    //     // --- Shared Helpers ---
    //
    //     /// テーブルのレコード数を取得します（共有ロジック）。
    //     pub(crate) fn count_table(
    //         &self,
    //         table: impl sea_query::Iden + Clone + 'static,
    //     ) -> Result<i64> {
    //         let sql = Query::select()
    //             .expr(Expr::cust("COUNT(*)"))
    //             .from(table)
    //             .to_string(PostgresQueryBuilder);
    //         self.store
    //             .conn
    //             .query_row(&sql, [], |r| r.get(0))
    //             .map_err(Into::into)
    //     }
    //
    //     fn finalize_updates(
    //         &self,
    //         items_path: &Path,
    //         stags_path: &Path,
    //         tmp_items: &Path,
    //         tmp_stags: &Path,
    //     ) -> Result<()> {
    //         std::fs::rename(tmp_items, items_path)?;
    //         std::fs::rename(tmp_stags, stags_path)?;
    //         Tbl::Item.drop_table(&self.store.conn)?;
    //         Tbl::IdItem.drop_table(&self.store.conn)?;
    //         Ok(())
    //     }
    //
    //     /// システムタグ更新用のソート済みUNIONクエリを構築します。
    //     fn build_ordered_system_tags_sql(
    //         p1_sql: &str,
    //         p2_sql: &str,
    //         p3_sql: &str,
    //     ) -> String {
    //         let union_sql = format!(
    //             "{} UNION ALL BY NAME {} UNION ALL BY NAME {}",
    //             p1_sql, p2_sql, p3_sql
    //         );
    //         format!(
    //             "SELECT * FROM ({}) ORDER BY type ASC, label_int ASC, label_str ASC, item_id ASC",
    //             union_sql
    //         )
    //     }
}

/// スキャン時の高速フィルタリングに使用するメタデータハッシュ。
/// データベース(BIGINT)との互換性のために内部で i64 を保持します。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScanHash(pub i64);

impl ScanHash {
    /// u64 のハッシュ値をデータベース保存用の i64 に変換します。
    pub fn from_u64(v: u64) -> Self {
        Self(v as i64)
    }
}

// DuckDB 連携用実装
impl FromSql for ScanHash {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(ScanHash)
    }
}

impl ToSql for ScanHash {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

/// 一時的なスキャン結果テーブル（Tbl::Scan）の 1 行を表す構造体。
/// `ScanEntry` の情報に、高速比較用のハッシュ値を付加したものです。
pub struct TempScanEntry {
    pub entry: ScanEntry,
    pub hash: ScanHash,
}

impl TempScanEntry {
    /// データベース書き込み用のパラメータリスト（ハッシュ込み）を返します。
    pub fn params(&self) -> Vec<&dyn duckdb::ToSql> {
        let mut p = self.entry.as_params();
        p.push(&self.hash);
        p
    }

    /// Tbl::Scan テーブル作成用のカラム構成リストを取得します。
    pub fn columns_with_type() -> Vec<(sea_query::DynIden, sea_query::DynIden)>
    {
        use crate::db::{BiticalType, Col};
        use sea_query::IntoIden;

        let mut cols = ScanEntry::columns_with_type();
        cols.push((
            Col::ScanHash.into_iden(),
            BiticalType::Integer.into_iden(),
        ));
        cols
    }

    /// 指定されたオフセットから DuckDB の Row を読み込み、TempScanEntry を復元します。
    pub fn from_row_with_offset(
        row: &duckdb::Row,
        offset: usize,
        loader: &ScanEntryLoader,
    ) -> duckdb::Result<Self> {
        let entry = ScanEntry::from_row_with_offset(row, offset)?;
        let hash: ScanHash = row.get(offset + loader.hash_idx)?;
        Ok(Self { entry, hash })
    }
}

/// `TempScanEntry` をデータベースの行から効率的に読み出すためのローダー。
pub struct ScanEntryLoader {
    hash_idx: usize,
}

impl Default for ScanEntryLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanEntryLoader {
    /// 新しいローダーを作成します。ハッシュのカラム位置を事前に計算します。
    pub fn new() -> Self {
        Self {
            hash_idx: ScanEntry::schema().len(),
        }
    }

    /// DuckDB の Row から `TempScanEntry` を高速に読み出します。
    pub fn load(&self, row: &duckdb::Row) -> duckdb::Result<TempScanEntry> {
        let entry = ScanEntry::from_row(row)?;
        let hash: ScanHash = row.get(self.hash_idx)?;
        Ok(TempScanEntry { entry, hash })
    }
}

/// メタデータ（パス、更新日時、サイズ）から ScanHash を計算します。
pub(crate) fn calc_scanhash(path: &str, mtime: i64, size: i64) -> ScanHash {
    use rustc_hash::FxHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = FxHasher::default();
    (path, mtime, size).hash(&mut hasher);
    ScanHash(hasher.finish() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::TagRegistry;
    use tempfile::tempdir;

    #[test]
    fn test_calc_scanhash() {
        let h1 = calc_scanhash("test.txt", 100, 500);
        let h2 = calc_scanhash("test.txt", 100, 500);
        let h3 = calc_scanhash("test.txt", 101, 500); // mtime違い
        let h4 = calc_scanhash("other.txt", 100, 500); // path違い
        let h5 = calc_scanhash("test.txt", 100, 501); // size違い

        assert_eq!(h1, h2, "同じ入力なら同じハッシュになること");
        assert_ne!(h1, h3, "mtimeが変わればハッシュが変わること");
        assert_ne!(h1, h4, "pathが変わればハッシュが変わること");
        assert_ne!(h1, h5, "sizeが変わればハッシュが変わること");
    }

    #[test]
    fn test_load_metadata_cache_empty() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().to_path_buf();
        let registry = TagRegistry::with_standard();
        let store = Store::open(&db_dir).unwrap();
        let indexer = Indexer::new(&store, &registry);

        // まだファイルがない状態でのロード
        let cache = indexer.load_metadata_cache().unwrap();
        assert!(
            cache.is_empty(),
            "Cache should be empty when no files exist"
        );
    }

    #[test]
    fn test_load_metadata_cache_with_data() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().to_path_buf();
        let registry = TagRegistry::with_standard();
        let store = Store::open(&db_dir).unwrap();
        let indexer = Indexer::new(&store, &registry);

        // 1. テーブルを初期化
        indexer.initialize_tables().unwrap();

        // 2. ダミーデータを直接 locations に書き込む (scan_hash 込み)
        let hash_val = ScanHash(123456789);
        let item_id: ItemId = ItemId::from(1);
        let locs_path = store.path_for_target(TargetTable::Locations);

        store
            .conn
            .execute(
                "CREATE TABLE temp_locs AS SELECT ? as item_id, ? as scan_hash",
                [item_id.as_i64(), hash_val.0],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "COPY temp_locs TO '{}' (FORMAT PARQUET)",
                    locs_path.to_string_lossy()
                ),
                [],
            )
            .unwrap();

        // 3. ロードして検証
        let cache = indexer.load_metadata_cache().unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(
            *cache.get(&hash_val).unwrap(),
            item_id,
            "Cache should contain the correct item_id"
        );
    }

    #[test]
    fn test_initialize_tables() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let registry = TagRegistry::with_standard();
        let store = Store::open(&db_dir).unwrap();
        Indexer::new(&store, &registry).initialize_tables().unwrap();
        assert!(db_dir.join("file_references.parquet").exists());
    }

    // #[test]
    // fn test_build_ordered_system_tags_sql() {
    //     let sql = Indexer::build_ordered_system_tags_sql(
    //         "SELECT * FROM system_tags",
    //         "SELECT * FROM p2",
    //         "SELECT * FROM p3",
    //     );
    //     assert!(
    //         sql.contains("UNION ALL BY NAME"),
    //         "Should contain UNION ALL BY NAME"
    //     );
    //     assert!(
    //         sql.contains(
    //             "ORDER BY type ASC, label_int ASC, label_str ASC, item_id ASC"
    //         ),
    //         "Should contain correct ORDER BY clause"
    //     );
    //     assert!(sql.starts_with("SELECT * FROM ("));
    //     assert!(sql.ends_with("ASC"));
    // }
}
