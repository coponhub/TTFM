// Copyright (C) 2026 coponhub
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

use crate::tag::TagRegistry;
use crate::types::Rank;
use sea_query::{CaseStatement, Condition, SimpleExpr};

/// システムにおける標準的なランク（優先度）の定義。
/// 数値が大きいほど、検索結果やカラム表示において優先されます。
pub struct SystemRank;

impl SystemRank {
    pub const NAME: Rank = 10;
    pub const SIZE: Rank = 8;
    pub const MTIME: Rank = 7;
    pub const PARENT_DIR: Rank = 6;
    pub const ITEM_KIND: Rank = 5;
    pub const CONTENT: Rank = 4;
    pub const FILENAME: Rank = 9;
    pub const DEFAULT: Rank = 0;
    pub const PATH: Rank = -1;
}

/// TagRegistry の情報に基づき、ランク決定用の SQL 式を構築します。
///
/// # Arguments
/// * `registry` - タグ定義（名前とデフォルトランク）を持つレジストリ
/// * `guard_condition` - ランク付けルールを適用するための条件（例: `ItemKind == "type"`）
/// * `key_expr` - ランク決定のキーとなる値を持つ式（例: `Content`）
/// * `default_rank` - 条件に合致しない、またはキーに対応するランクがない場合のデフォルト値
pub fn build_rank_expr(
    registry: &TagRegistry,
    guard_condition: Condition,
    key_expr: impl Into<SimpleExpr>,
    default_rank: Rank,
) -> SimpleExpr {
    let key: SimpleExpr = key_expr.into();
    let mut key_case = CaseStatement::new();

    for (name, rank) in registry.iter_all_for_rank() {
        if rank != 0 {
            key_case = key_case.case(key.clone().eq(name), rank);
        }
    }

    let key_rank_expr = key_case.finally(default_rank);

    CaseStatement::new()
        .case(guard_condition, key_rank_expr)
        .finally(default_rank)
        .into()
}

/// 検索結果リストに対して優先度を一括設定し、OneView を再構築します。
pub fn update_ranks(
    store: &crate::db::Store,
    registry: &TagRegistry,
    results: &[crate::response::Item],
    rank: i64,
) -> anyhow::Result<()> {
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
        batch_update_rank(store, &file_ids, true, rank)?;
    }
    if !item_ids.is_empty() {
        batch_update_rank(store, &item_ids, false, rank)?;
    }
    let all_columns = registry.get_all_columns();
    let reader = crate::query::lens_reader::Reader::build(
        registry,
        crate::db::Tbl::_OneView,
    );
    crate::oneview::OneView::recreate(
        &store.conn,
        &all_columns,
        reader,
        &store.db_dir,
    )?;
    Ok(())
}

/// IDを指定して優先度を設定します。
pub fn set_rank_by_id(
    store: &crate::db::Store,
    registry: &TagRegistry,
    id: i64,
    is_file: bool,
    rank: i64,
) -> anyhow::Result<()> {
    batch_update_rank(store, &[id], is_file, rank)?;
    let all_columns = registry.get_all_columns();
    let reader = crate::query::lens_reader::Reader::build(
        registry,
        crate::db::Tbl::_OneView,
    );
    crate::oneview::OneView::recreate(
        &store.conn,
        &all_columns,
        reader,
        &store.db_dir,
    )?;
    Ok(())
}

/// 全てのタグ型の優先度（RANK）を取得します。
pub fn get_type_ranks(
    store: &crate::db::Store,
) -> anyhow::Result<std::collections::HashMap<String, i64>> {
    use crate::db::{Col, Tbl};
    use crate::util;
    use sea_query::{Expr, PostgresQueryBuilder, Query};

    let path = store.path_for_target(crate::db::TargetTable::ItemReferences);
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

    let mut stmt = store.conn.prepare(&query)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;

    let mut map = std::collections::HashMap::new();
    for row in rows {
        let (name, r) = row?;
        map.insert(name, r);
    }
    Ok(map)
}

fn batch_update_rank(
    store: &crate::db::Store,
    ids: &[i64],
    is_file: bool,
    rank: i64,
) -> anyhow::Result<()> {
    use crate::db::{Col, TargetTable, Tbl};
    use crate::util::{self, ExecuteSql, IdenExt, ParquetExt, SelectExt};
    use sea_query::{Expr, Query};

    let path = if is_file {
        store.path_for_target(TargetTable::FileReferences)
    } else {
        store.path_for_target(TargetTable::ItemReferences)
    };

    let path_str = path.to_string_lossy();
    let temp_table = Tbl::Target;

    util::parquet_query(&path_str).create_table_as(&store.conn, temp_table)?;

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
        .execute(&store.conn)?;

    Query::select()
        .column(sea_query::Asterisk)
        .from(temp_table)
        .order_by(Col::ItemId, sea_query::Order::Asc)
        .to_owned()
        .save_parquet(&store.conn, &path)?;
    temp_table.drop_table(&store.conn)?;
    Ok(())
}

/// 指定されたタグ名に対応するデフォルトランクを取得します。
pub fn get_rank_by_name(registry: &TagRegistry, name: &str) -> Rank {
    for (n, rank) in registry.iter_all_for_rank() {
        if n == name {
            return rank;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Col, Pronoun::*};
    use crate::tag::TagFunction;
    use sea_query::{Expr, PostgresQueryBuilder, Query};

    struct MockFunc {
        name: &'static str,
        rank: Rank,
    }
    impl TagFunction for MockFunc {
        fn name(&self) -> &str {
            self.name
        }
        fn default_rank(&self) -> Rank {
            self.rank
        }
    }

    fn create_registry() -> TagRegistry {
        let mut reg = TagRegistry::new();
        reg.register_plugin(MockFunc {
            name: "high",
            rank: 100,
        });
        reg.register_plugin(MockFunc {
            name: "low",
            rank: 1,
        });
        reg.register_plugin(MockFunc {
            name: "zero",
            rank: 0,
        });
        reg.register_plugin(MockFunc {
            name: "name",
            rank: 10,
        });
        reg.register_plugin(MockFunc {
            name: "kind",
            rank: 5,
        });
        reg
    }

    #[test]
    fn test_get_rank_by_name() {
        let reg = create_registry();
        assert_eq!(get_rank_by_name(&reg, "high"), 100);
        assert_eq!(get_rank_by_name(&reg, "low"), 1);
        assert_eq!(get_rank_by_name(&reg, "zero"), 0);
        assert_eq!(get_rank_by_name(&reg, "unknown"), 0); // Default for unknown

        // System defaults check
        assert_eq!(get_rank_by_name(&reg, "name"), 10);
        assert_eq!(get_rank_by_name(&reg, "kind"), 5);
    }

    #[test]
    fn test_build_rank_expr_sql_generation() {
        let reg = create_registry();

        // Build the expression
        // Guard: col("kind") = "type"
        // Key: col("content")
        let expr = build_rank_expr(
            &reg,
            Condition::all().add(Expr::col(Kind).eq("type")),
            Expr::col(Col::Content),
            0,
        );

        // Convert to SQL string for verification
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);

        // Verification
        // "SELECT CASE WHEN "kind" = 'type' THEN CASE ... END ELSE 0 END"
        assert!(sql.contains(r#"CASE WHEN ("kind" = 'type') THEN"#));

        // Verify inner cases (Registry items)
        assert!(sql.contains(r#"WHEN ("content" = 'high') THEN 100"#));
        assert!(sql.contains(r#"WHEN ("content" = 'low') THEN 1"#));

        // "zero" (rank 0) should NOT be in the CASE statement (optimization)
        assert!(!sql.contains(r#"WHEN ("content" = 'zero'"#));

        // Verify system defaults
        assert!(sql.contains(r#"WHEN ("content" = 'name') THEN 10"#));
    }
}
