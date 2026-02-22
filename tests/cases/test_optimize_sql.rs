use sea_query::SqliteQueryBuilder;
use ttfm::query::lens_resolver::Resolver;

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
        "parentdir: &: (count(ext:rs) > 0) & parentdir: &: (sum(size:) > 1000)";

    let resolved = Resolver::new(query_str).unwrap().resolved_query;
    let optimized = ttfm::query::lens_optimizer::optimize(resolved);

    let stmt = ttfm::query::sql::build_pick_sql(&optimized, "tags_view");
    let sql = normalize_sql(&stmt.to_string(SqliteQueryBuilder));
    println!("Logical SQL: {}", sql);

    // 期待されるSQL: 1つのGROUP BY に対する HAVING に複数条件がマージされている
    let expected = normalize_sql(r#"
        SELECT DISTINCT "item_id", "rank", "item_kind" FROM "tags_view"
        WHERE "type" = 'parentdir' AND "label_str" IN (
            SELECT "group_label" FROM (
                SELECT "proj"."label_str" AS "group_label"
                FROM "tags_view" AS "proj"
                INNER JOIN "tags_view"."c" ON "proj"."item_id" = "c"."item_id"
                WHERE "proj"."type" = 'parentdir'
                GROUP BY "proj"."label_str"
                HAVING COUNT(DISTINCT (CASE WHEN ("c"."type" = 'ext' AND "c"."label_str" = 'rs') THEN "c"."item_id" ELSE NULL END)) > CAST(0 AS DOUBLE)
                AND SUM((CASE WHEN ("c"."type" = 'size') THEN TRY_CAST(c.label_str AS DOUBLE) ELSE 0 END)) > CAST(1000 AS DOUBLE)
            ) AS "nfilter"
        )
    "#);

    assert_eq!(sql, expected, "Logical SQL structure should match expected grouped query");
}

#[test]
fn test_build_optimized_merged_projection_sql_arithmetic() {
    let query_str =
        "(((parentdir: &: count(ext:rs))) / ((parentdir: &: count()))) :> 100";

    let resolved = Resolver::new(query_str).unwrap().resolved_query;
    let optimized = ttfm::query::lens_optimizer::optimize(resolved);

    let stmt = ttfm::query::sql::build_pick_sql(&optimized, "tags_view");
    let sql = normalize_sql(&stmt.to_string(SqliteQueryBuilder));
    println!("Arithmetic SQL: {}", sql);

    // 理想的なSQL: 算術演算も単一のGROUP BYとHAVING句内の計算に統合されること
    // (L INNER JOIN R) ではなく、一つの SELECT ... GROUP BY ... HAVING (agg / agg) > 100 となるべき
    let expected = normalize_sql(r#"
        SELECT DISTINCT "item_id", "rank", "item_kind" FROM "tags_view"
        WHERE "type" = 'parentdir' AND "label_str" IN (
            SELECT "group_label" FROM (
                SELECT "proj"."label_str" AS "group_label"
                FROM "tags_view" AS "proj"
                INNER JOIN "tags_view"."c" ON "proj"."item_id" = "c"."item_id"
                WHERE "proj"."type" = 'parentdir'
                GROUP BY "proj"."label_str"
                HAVING CAST(COUNT(DISTINCT (CASE WHEN ("c"."type" = 'ext' AND "c"."label_str" = 'rs') THEN "c"."item_id" ELSE NULL END)) AS DOUBLE)
                    / CAST(COUNT(DISTINCT (CASE WHEN ("c"."type" = '*' AND "c"."label_str" = '*') THEN "c"."item_id" ELSE NULL END)) AS DOUBLE)
                > CAST(100 AS DOUBLE)
            ) AS "nfilter"
        )
    "#);

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

    let resolved = Resolver::new(query_str).unwrap().resolved_query;
    let optimized = ttfm::query::lens_optimizer::optimize(resolved);

    let stmt = ttfm::query::sql::build_pick_sql(&optimized, "tags_view");
    let sql = normalize_sql(&stmt.to_string(SqliteQueryBuilder));
    println!("Comparison SQL: {}", sql);

    // 集計関数同士の比較が一つのHAVING句内で完結していることを確認
    let expected = normalize_sql(r#"
        SELECT DISTINCT "item_id", "rank", "item_kind" FROM "tags_view"
        WHERE "type" = 'parentdir' AND "label_str" IN (
            SELECT "group_label" FROM (
                SELECT "proj"."label_str" AS "group_label"
                FROM "tags_view" AS "proj"
                INNER JOIN "tags_view"."c" ON "proj"."item_id" = "c"."item_id"
                WHERE "proj"."type" = 'parentdir'
                GROUP BY "proj"."label_str"
                HAVING COUNT(DISTINCT (CASE WHEN ("c"."type" = 'size') THEN "c"."item_id" ELSE NULL END)) = SUM("label_int")
            ) AS "nfilter"
        )
    "#);

    assert_eq!(sql, expected, "Comparison SQL structure should match expected grouped query");
}
