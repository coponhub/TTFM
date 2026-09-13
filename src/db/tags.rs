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
use crate::tag::TagRegistry;
use crate::util;
use anyhow::Result;
use duckdb::Connection;
use sea_query::{
    Expr, Func, JoinType, PostgresQueryBuilder, Query, SelectStatement,
    UnionType,
};

fn sanitize_tag_type(tag_type: &str) -> Result<&str> {
    if tag_type.is_empty()
        || tag_type.contains('/')
        || tag_type.contains('\\')
        || tag_type.contains("..")
    {
        anyhow::bail!("Invalid tag_type for partition: {}", tag_type);
    }
    Ok(tag_type)
}

fn build_tags_select_query(
    store: &Store,
    registry: &TagRegistry,
    target_type: Option<&str>,
) -> SelectStatement {
    let mut union_q = Query::select();
    let loc_label = Expr::col((Tbl::TagsByLocation, Col::LabelStr));
    let loc_tag = Func::cust(crate::db::DuckDbFunc::Concat).args([
        Expr::col((Tbl::TagsByLocation, Col::Type)).into(),
        Expr::val(":").into(),
        loc_label.clone().into(),
    ]);
    union_q
        .column((Tbl::TagsByLocation, Col::Type))
        .expr_as(loc_label, Col::Label)
        .expr_as(loc_tag, Col::TypedTag)
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::TagsByLocation)
                    .to_string_lossy(),
            ),
            Tbl::TagsByLocation,
        );
    if let Some(t) = target_type {
        union_q.and_where(Expr::col((Tbl::TagsByLocation, Col::Type)).eq(t));
    }

    let mut user_q = Query::select();
    let user_label = crate::oneview::build_label_str_expr(Tbl::UserTags);
    let user_tag = Func::cust(crate::db::DuckDbFunc::Concat).args([
        Expr::col((Tbl::UserTags, Col::Type)).into(),
        Expr::val(":").into(),
        user_label.clone().into(),
    ]);
    user_q
        .column((Tbl::UserTags, Col::Type))
        .expr_as(user_label, Col::Label)
        .expr_as(user_tag, Col::TypedTag)
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::UserTags)
                    .to_string_lossy(),
            ),
            Tbl::UserTags,
        );
    if let Some(t) = target_type {
        user_q.and_where(Expr::col((Tbl::UserTags, Col::Type)).eq(t));
    }
    union_q.union(UnionType::All, user_q);

    let mut base_q = Query::select();
    let base_label = crate::oneview::build_label_str_expr(Tbl::BaseTags);
    let base_tag = Func::cust(crate::db::DuckDbFunc::Concat).args([
        Expr::col((Tbl::BaseTags, Col::Type)).into(),
        Expr::val(":").into(),
        base_label.clone().into(),
    ]);
    base_q
        .column((Tbl::BaseTags, Col::Type))
        .expr_as(base_label, Col::Label)
        .expr_as(base_tag, Col::TypedTag)
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::BaseTags)
                    .to_string_lossy(),
            ),
            Tbl::BaseTags,
        );
    let excluded = registry.get_excluded_from_tags();
    if !excluded.is_empty() {
        base_q.and_where(
            Expr::col((Tbl::BaseTags, Col::Type)).is_not_in(excluded),
        );
    }
    if let Some(t) = target_type {
        base_q.and_where(Expr::col((Tbl::BaseTags, Col::Type)).eq(t));
    }
    union_q.union(UnionType::All, base_q);

    if target_type.is_none() || target_type == Some("rank") {
        let file_refs_path = store.path_for_target(TargetTable::FileReferences);
        if file_refs_path.exists() {
            let mut rank_q = Query::select();
            let rank_str = Expr::col((Tbl::FileReferences, Col::Rank))
                .cast_as(crate::db::BiticalType::String);
            let rank_tag = Func::cust(crate::db::DuckDbFunc::Concat)
                .args([Expr::val("rank:").into(), rank_str.clone().into()]);
            rank_q
                .expr_as(Expr::val("rank"), Col::Type)
                .expr_as(rank_str, Col::Label)
                .expr_as(rank_tag, Col::TypedTag)
                .from_subquery(
                    util::parquet_query(&file_refs_path.to_string_lossy()),
                    Tbl::FileReferences,
                );
            union_q.union(UnionType::All, rank_q);
        }
    }

    let mut item_refs_q = Query::select();
    item_refs_q
        .columns([Col::Content, Col::ItemId, Col::Rank])
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::ItemReferences)
                    .to_string_lossy(),
            ),
            Tbl::ItemReferences,
        )
        .and_where(Expr::col(Col::ItemKind).eq("tag"));

    let rank_expr = Func::cust(crate::db::DuckDbFunc::Coalesce).args([
        Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
        Expr::val(0i64).into(),
    ]);

    let mut q = Query::select();
    q.distinct();
    if target_type.is_none() {
        q.column((Tbl::ItemTags, Col::Type));
    }
    q.column((Tbl::ItemTags, Col::Label))
        .column((Tbl::ItemTags, Col::TypedTag))
        .expr_as(Expr::col((Tbl::ItemReferences, Col::ItemId)), Col::ItemId)
        .expr_as(rank_expr, Col::Rank)
        .from_subquery(union_q, Tbl::ItemTags)
        .join_subquery(
            JoinType::LeftJoin,
            item_refs_q,
            Tbl::ItemReferences,
            Expr::col((Tbl::ItemTags, Col::TypedTag))
                .eq(Expr::col((Tbl::ItemReferences, Col::Content))),
        )
        .order_by((Tbl::ItemTags, Col::Label), sea_query::Order::Asc)
        .order_by((Tbl::ItemReferences, Col::ItemId), sea_query::Order::Asc);
    q
}

fn update_tag_type_partition(
    store: &Store,
    conn: &Connection,
    registry: &TagRegistry,
    tag_type: &str,
) -> Result<()> {
    let tag_type = sanitize_tag_type(tag_type)?;
    let part_dir = store.tags_dir().join(format!("type={tag_type}"));
    let q = build_tags_select_query(store, registry, Some(&tag_type));
    let select_sql = q.to_string(PostgresQueryBuilder);
    let count_sql = format!("SELECT COUNT(*) FROM ({select_sql})");
    let mut stmt = conn.prepare(&count_sql)?;
    let count: i64 = stmt.query_row([], |r| r.get(0))?;
    if count == 0 {
        if part_dir.exists() {
            std::fs::remove_dir_all(&part_dir)?;
        }
        return Ok(());
    }
    let tmp_part = store.temp_tags_dir().join(format!("type={tag_type}"));
    if tmp_part.exists() {
        std::fs::remove_dir_all(&tmp_part)?;
    }
    std::fs::create_dir_all(&tmp_part)?;
    let copy_sql = format!(
        "COPY ({select_sql}) TO '{}' (FORMAT PARQUET)",
        tmp_part
            .join("data.parquet")
            .display()
            .to_string()
            .replace('\'', "''")
    );
    conn.execute(&copy_sql, [])?;
    if part_dir.exists() {
        let old_part = store.db_dir.join(format!("tags_{tag_type}.old"));
        if old_part.exists() {
            std::fs::remove_dir_all(&old_part)?;
        }
        std::fs::rename(&part_dir, &old_part)?;
        std::fs::rename(&tmp_part, &part_dir)?;
        let _ = std::fs::remove_dir_all(&old_part);
    } else {
        std::fs::create_dir_all(store.tags_dir())?;
        std::fs::rename(&tmp_part, &part_dir)?;
    }
    Ok(())
}

pub fn update_tags_partitions(
    store: &Store,
    conn: &Connection,
    registry: &TagRegistry,
    modified_types: &[&str],
) -> Result<()> {
    for &tag_type in modified_types {
        update_tag_type_partition(store, conn, registry, tag_type)?;
    }
    Ok(())
}

pub fn generate_tags_hive_partitioned(
    store: &Store,
    conn: &Connection,
    registry: &TagRegistry,
) -> Result<()> {
    let tmp_dir = store.temp_tags_dir();
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }
    std::fs::create_dir_all(&tmp_dir)?;
    let q = build_tags_select_query(store, registry, None);
    let select_sql = q.to_string(PostgresQueryBuilder);
    let copy_sql = format!(
        "COPY ({select_sql}) TO '{}' \
         (FORMAT PARQUET, PARTITION_BY (type), OVERWRITE_OR_IGNORE)",
        tmp_dir.display().to_string().replace('\'', "''")
    );
    conn.execute(&copy_sql, [])?;
    let meta_file = tmp_dir.join("metadata.parquet");
    let meta_sql = format!(
        "COPY (SELECT 1 AS ready) TO '{}' \
         (FORMAT PARQUET, KV_METADATA {{ 'ttfm.status': 'complete' }})",
        meta_file.display().to_string().replace('\'', "''")
    );
    conn.execute(&meta_sql, [])?;
    if store.tags_dir().exists() {
        let backup = store.db_dir.join("tags.old");
        if backup.exists() {
            std::fs::remove_dir_all(&backup)?;
        }
        std::fs::rename(store.tags_dir(), &backup)?;
        std::fs::rename(&tmp_dir, store.tags_dir())?;
        let _ = std::fs::remove_dir_all(&backup);
    } else {
        std::fs::rename(&tmp_dir, store.tags_dir())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn insert_dummy_test_data(store: &Store) -> Result<()> {
        let loc_p = store.path_for_target(TargetTable::TagsByLocation);
        let base_p = store.path_for_target(TargetTable::BaseTags);
        let user_p = store.path_for_target(TargetTable::UserTags);
        let item_p = store.path_for_target(TargetTable::ItemReferences);

        store.conn.execute(
            &format!(
                "COPY (SELECT 1::BIGINT AS item_id, 'extension' AS type, \
                 'rs' AS label_str, NULL::BIGINT AS label_int, \
                 NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool) \
                 TO '{}' (FORMAT PARQUET)",
                loc_p.to_string_lossy().replace('\'', "''")
            ),
            [],
        )?;

        store.conn.execute(
            &format!(
                "COPY (SELECT 1::BIGINT AS item_id, 'size' AS type, \
                 NULL::VARCHAR AS label_str, 1024::BIGINT AS label_int, \
                 NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                 0::BIGINT AS rank) \
                 TO '{}' (FORMAT PARQUET)",
                base_p.to_string_lossy().replace('\'', "''")
            ),
            [],
        )?;

        store.conn.execute(
            &format!(
                "COPY (SELECT 1::BIGINT AS item_id, 'color' AS type, \
                 'blue' AS label_str, NULL::BIGINT AS label_int, \
                 NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                 0::BIGINT AS rank) \
                 TO '{}' (FORMAT PARQUET)",
                user_p.to_string_lossy().replace('\'', "''")
            ),
            [],
        )?;

        store.conn.execute(
            &format!(
                "COPY (SELECT 10::BIGINT AS item_id, 'tag' AS item_kind, \
                 'extension:rs' AS content, 'extension:rs' AS name, \
                 5::BIGINT AS rank) \
                 TO '{}' (FORMAT PARQUET)",
                item_p.to_string_lossy().replace('\'', "''")
            ),
            [],
        )?;
        Ok(())
    }

    #[test]
    fn test_generate_and_atomic_update_tags_hive() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();

        let mut q = Query::select();
        q.column((Tbl::Tags, Col::Type))
            .expr_as(Expr::col((Tbl::Tags, Col::Label)), Col::Name)
            .from_subquery(
                util::hive_parquet_query(&store.tags_dir()),
                Tbl::Tags,
            )
            .and_where(Expr::col((Tbl::Tags, Col::Type)).eq("extension"));
        let sql = q.to_string(PostgresQueryBuilder);
        let mut stmt = store.conn.prepare(&sql).unwrap();
        let labels: Vec<String> = stmt
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(labels.contains(&"rs".to_string()));

        let size_dir = store.tags_dir().join("type=size");
        assert!(!size_dir.exists());

        update_tags_partitions(&store, &store.conn, &registry, &["extension"])
            .unwrap();
        assert!(store.tags_dir().join("type=extension").exists());
    }

    #[test]
    fn test_differential_update_preserves_hive_schema_uniformity() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();

        // 1つのパーティション（extension）を差分更新
        update_tags_partitions(&store, &store.conn, &registry, &["extension"])
            .unwrap();

        // 個々の parquet ファイルのカラム構成を直接検証（type 列が物理ファイル内に存在せず、全体生成と同一の4列であること）
        let ext_parquet = store.tags_dir().join("type=extension/data.parquet");
        let ext_file_cols: Vec<String> = store
            .conn
            .prepare(&format!(
                "SELECT column_name FROM (DESCRIBE SELECT * FROM read_parquet('{}', hive_partitioning = false))",
                ext_parquet.display().to_string().replace('\'', "''")
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(ext_file_cols, vec!["label", "tag", "item_id", "rank"]);

        // Hive glob 読み込みで全パーティションが正常に読めること
        let read_hive_sql = format!(
            "SELECT count(*) FROM read_parquet('{}/*/*.parquet', hive_partitioning = true)",
            store.tags_dir().display().to_string().replace('\'', "''")
        );
        let count: i64 = store
            .conn
            .query_row(&read_hive_sql, [], |r| r.get(0))
            .unwrap();
        assert!(count > 0);
    }

    #[test]
    fn test_update_tag_type_partition_unions_all_sources() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        insert_dummy_test_data(&store).unwrap();
        let base_p = store.path_for_target(TargetTable::BaseTags);
        let user_p = store.path_for_target(TargetTable::UserTags);

        // base_tags に mimetype: text/rust
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'mimetype' AS type, \
                     'text/rust' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                     0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    base_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        // user_tags に mimetype: text/plain
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 2::BIGINT AS item_id, 'mimetype' AS type, \
                     'text/plain' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                     0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    user_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        // 差分更新を実行
        let registry = TagRegistry::with_standard();
        update_tags_partitions(&store, &store.conn, &registry, &["mimetype"])
            .unwrap();

        // 差分更新された parquet から label を取得
        let mime_parquet = store.tags_dir().join("type=mimetype/data.parquet");
        let labels: Vec<String> = store
            .conn
            .prepare(&format!(
                "SELECT label FROM read_parquet('{}', hive_partitioning = false) ORDER BY label ASC",
                mime_parquet.display().to_string().replace('\'', "''")
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        // base_tags の 'text/rust' と user_tags の 'text/plain' の両方が含まれること
        assert_eq!(labels, vec!["text/plain", "text/rust"]);
    }

    #[test]
    fn test_tags_select_query_name_and_rank() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        let loc_p = store.path_for_target(TargetTable::TagsByLocation);
        let user_p = store.path_for_target(TargetTable::UserTags);
        let file_p = store.path_for_target(TargetTable::FileReferences);

        // TagsByLocation: filename = "sample.rs"
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'filename' AS type, \
                     'sample.rs' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool) \
                     TO '{}' (FORMAT PARQUET)",
                    loc_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        // UserTags: name = "custom_sample"
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 2::BIGINT AS item_id, 'name' AS type, \
                     'custom_sample' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                     0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    user_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        // FileReferences: rank = 5
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 5::BIGINT AS rank, \
                     'file1' AS file_id, 100::BIGINT AS size, 1000::BIGINT AS mtime, \
                     NULL::VARCHAR AS hash, false AS is_dir) \
                     TO '{}' (FORMAT PARQUET)",
                    file_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();

        // Check name partition
        assert!(store.tags_dir().join("type=name").exists());
        let mut q_name = Query::select();
        q_name
            .column((Tbl::Tags, Col::Label))
            .from_subquery(
                util::hive_parquet_query(&store.tags_dir()),
                Tbl::Tags,
            )
            .and_where(Expr::col((Tbl::Tags, Col::Type)).eq("name"));
        let name_sql = q_name.to_string(PostgresQueryBuilder);
        let name_labels: Vec<String> = store
            .conn
            .prepare(&name_sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!name_labels.contains(&"sample.rs".to_string()));
        assert!(name_labels.contains(&"custom_sample".to_string()));

        // Check rank partition
        assert!(store.tags_dir().join("type=rank").exists());
        let mut q_rank = Query::select();
        q_rank
            .column((Tbl::Tags, Col::Label))
            .from_subquery(
                util::hive_parquet_query(&store.tags_dir()),
                Tbl::Tags,
            )
            .and_where(Expr::col((Tbl::Tags, Col::Type)).eq("rank"));
        let rank_sql = q_rank.to_string(PostgresQueryBuilder);
        let rank_labels: Vec<String> = store
            .conn
            .prepare(&rank_sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(rank_labels.contains(&"5".to_string()));
    }
}
