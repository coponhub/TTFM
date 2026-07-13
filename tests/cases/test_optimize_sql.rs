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

use sea_query::PostgresQueryBuilder;
use ttfm::db::Src;
use ttfm::query::lens_resolver::Resolver;
use ttfm::query::sql::BuildPick;
use ttfm::tag::TagRegistry;

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("( ", "(")
        .replace(" )", ")")
}

#[test]
fn test_build_optimized_merged_projection_sql_logical() {
    let query_str =
        "parentdir: &: (count(extension:rs) > 0) & parentdir: &: (sum(size:) > 1000)";

    let resolved = Resolver::new(query_str, &TagRegistry::with_standard())
        .unwrap()
        .resolved_query;
    let optimized = ttfm::query::lens_optimizer::optimize(resolved);

    let stmt =
        ttfm::query::sql::PickNode::new(&Src::OneView, &optimized).build_pick();
    let sql = normalize_sql(&stmt.to_string(PostgresQueryBuilder));
    println!("Logical SQL: {}", sql);

    // 期待されるSQL: 1つのGROUP BY に対する HAVING に複数条件がマージされている
    let expected = normalize_sql(
        r#"
        SELECT DISTINCT "item_id", "rank", "item_kind" FROM "oneview"
        WHERE "type" = 'parentdir' AND (list_value(CAST("label_str" AS UNION("string" VARCHAR, "integer" BIGINT, "double" DOUBLE, "boolean" BOOLEAN, "uuid" UUID)))) IN (
            SELECT "group" FROM (
                SELECT list_value(CAST("proj"."label_str" AS UNION("string" VARCHAR, "integer" BIGINT, "double" DOUBLE, "boolean" BOOLEAN, "uuid" UUID))) AS "group"
                FROM "oneview" AS "proj"
                INNER JOIN "oneview" AS "view" ON "proj"."item_id" = "view"."item_id"
                WHERE "proj"."type" = 'parentdir'
                GROUP BY "proj"."label_str"
                HAVING COUNT(DISTINCT (CASE WHEN ("view"."item_id" IN (SELECT "item_id" FROM (SELECT "item_id", "rank", "item_kind" FROM (SELECT DISTINCT "item_id", "rank", "item_kind" FROM "oneview" WHERE "type" = 'extension' AND "label_str" = 'rs') AS "sub" INTERSECT (SELECT "item_id", "rank", "item_kind" FROM (SELECT DISTINCT "item_id", "rank", "item_kind" FROM "oneview" WHERE "type" = 'is_dir' AND ("label_bool" = 'false' OR "label_bool" = FALSE)) AS "sub")) AS "nv_filter")) THEN "view"."item_id" ELSE NULL END)) > 0
                AND SUM((CASE WHEN ("view"."type" = 'size') THEN COALESCE("view"."label_int", "view"."label_double", TRY_CAST("view"."label_str" AS DOUBLE)) ELSE NULL END)) > 1000
            ) AS "filter"
        )
    "#,
    );

    assert_eq!(
        sql, expected,
        "Logical SQL structure should match expected grouped query"
    );
}

#[test]
fn test_build_optimized_merged_projection_sql_arithmetic() {
    let query_str =
        "(((parentdir: &: count(extension:rs))) / ((parentdir: &: count()))) :> 100";

    let resolved = Resolver::new(query_str, &TagRegistry::with_standard())
        .unwrap()
        .resolved_query;
    let optimized = ttfm::query::lens_optimizer::optimize(resolved);

    let stmt =
        ttfm::query::sql::PickNode::new(&Src::OneView, &optimized).build_pick();
    let sql = normalize_sql(&stmt.to_string(PostgresQueryBuilder));
    println!("Arithmetic SQL: {}", sql);

    // 理想的なSQL: 算術演算も単一のGROUP BYとHAVING句内の計算に統合されること
    // (L INNER JOIN R) ではなく、一つの SELECT ... GROUP BY ... HAVING (agg / agg) > 100 となるべき
    let expected = normalize_sql(
        r#"
        SELECT DISTINCT "item_id", "rank", "item_kind" FROM "oneview"
        WHERE "type" = 'parentdir' AND (list_value(CAST("label_str" AS UNION("string" VARCHAR, "integer" BIGINT, "double" DOUBLE, "boolean" BOOLEAN, "uuid" UUID)))) IN (
            SELECT "group" FROM (
                SELECT list_value(CAST("proj"."label_str" AS UNION("string" VARCHAR, "integer" BIGINT, "double" DOUBLE, "boolean" BOOLEAN, "uuid" UUID))) AS "group"
                FROM "oneview" AS "proj"
                INNER JOIN "oneview" AS "view" ON "proj"."item_id" = "view"."item_id"
                WHERE "proj"."type" = 'parentdir'
                GROUP BY "proj"."label_str"
                HAVING CAST(COUNT(DISTINCT (CASE WHEN ("view"."item_id" IN (SELECT "item_id" FROM (SELECT "item_id", "rank", "item_kind" FROM (SELECT DISTINCT "item_id", "rank", "item_kind" FROM "oneview" WHERE "type" = 'extension' AND "label_str" = 'rs') AS "sub" INTERSECT (SELECT "item_id", "rank", "item_kind" FROM (SELECT DISTINCT "item_id", "rank", "item_kind" FROM "oneview" WHERE "type" = 'is_dir' AND ("label_bool" = 'false' OR "label_bool" = FALSE)) AS "sub")) AS "nv_filter")) THEN "view"."item_id" ELSE NULL END)) AS DOUBLE)
                    / CAST(COUNT(DISTINCT "view"."item_id") AS DOUBLE)
                > 100
            ) AS "filter"
        )
    "#,
    );

    assert_eq!(sql, expected, "Arithmetic SQL should be optimized into a single GROUP BY with calculation in HAVING");

    // JOIN数はベースの1つのみであること
    assert_eq!(
        sql.matches("JOIN").count(),
        1,
        "Arithmetic should be fully merged into a single GROUP BY, using only 1 JOIN"
    );
}

#[test]
fn test_build_optimized_merged_projection_sql_comparison() {
    let query_str =
        "((parentdir: &: count(size:))) := ((parentdir: &: sum(size:)))";

    let resolved = Resolver::new(query_str, &TagRegistry::with_standard())
        .unwrap()
        .resolved_query;
    let optimized = ttfm::query::lens_optimizer::optimize(resolved);

    let stmt =
        ttfm::query::sql::PickNode::new(&Src::OneView, &optimized).build_pick();
    let sql = normalize_sql(&stmt.to_string(PostgresQueryBuilder));
    println!("Comparison SQL: {}", sql);

    // 集計関数同士の比較が一つのHAVING句内で完結していることを確認
    let expected = normalize_sql(
        r#"
        SELECT DISTINCT "item_id", "rank", "item_kind" FROM "oneview"
        WHERE "type" = 'parentdir' AND (list_value(CAST("label_str" AS UNION("string" VARCHAR, "integer" BIGINT, "double" DOUBLE, "boolean" BOOLEAN, "uuid" UUID)))) IN (
            SELECT "group" FROM (
                SELECT list_value(CAST("proj"."label_str" AS UNION("string" VARCHAR, "integer" BIGINT, "double" DOUBLE, "boolean" BOOLEAN, "uuid" UUID))) AS "group"
                FROM "oneview" AS "proj"
                INNER JOIN "oneview" AS "view" ON "proj"."item_id" = "view"."item_id"
                WHERE "proj"."type" = 'parentdir'
                GROUP BY "proj"."label_str"
                HAVING COUNT(DISTINCT (CASE WHEN ("view"."type" = 'size') THEN "view"."item_id" ELSE NULL END)) = SUM((CASE WHEN ("view"."type" = 'size') THEN COALESCE("view"."label_int", "view"."label_double", TRY_CAST("view"."label_str" AS DOUBLE)) ELSE NULL END))
            ) AS "filter"
        )
    "#,
    );

    assert_eq!(
        sql, expected,
        "Comparison SQL structure should match expected grouped query"
    );
}
