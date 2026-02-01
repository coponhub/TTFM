use crate::query::lens_resolver::Resolver;
use crate::query::lens_schema::StorageMapping;
use crate::response::{RawTagRow, SearchResult};
use crate::types::ItemId;
use crate::types::{Origin, SType, TagType};
use anyhow::Result;
use duckdb::types::Value;
use std::collections::HashMap;
use std::path::Path;

/// 検索結果の抽出（Pick）計画。
#[derive(Debug, Clone)]
pub struct PickPlan {
    /// 候補となるアイテムの ID と本来の優先度を取得するための SQL。
    pub select_sql: sea_query::SelectStatement,
    /// すでに抽出された候補 ID のリスト。
    pub candidate_ids: Vec<i64>,
}

/// クエリに基づきデータベースからデータを取得（Fetch）を担当する。
pub struct Fetcher<'a> {
    pub resolver: &'a Resolver,
    pub conn: &'a duckdb::Connection,
}

impl<'a> Fetcher<'a> {
    pub fn new(resolver: &'a Resolver, conn: &'a duckdb::Connection) -> Self {
        Self { resolver, conn }
    }

    /// 合致するアイテムの ID と Rank を抽出する計画を立て、実行します。
    pub fn pick(
        &self,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<PickPlan> {
        use sea_query::{Expr, Order};

        let resolved = &self.resolver.resolved_query;

        // 1. SQL 構築
        let mut select_sql =
            crate::query::sql::build_pick_sql(resolved, "oneview");

        // 検索仕様に基づき Rank と ItemId で降順ソート
        let rank_col = self.resolver.lens().resolve_col(SType::Rank)?;
        let id_col = self.resolver.lens().resolve_col(SType::ItemId)?;

        select_sql.order_by_expr(Expr::col(rank_col).into(), Order::Desc);
        select_sql.order_by_expr(Expr::col(id_col).into(), Order::Desc);

        if let Some(o) = offset {
            select_sql.offset(o as u64);
        }
        if let Some(l) = limit {
            select_sql.limit(l as u64);
        }

        // 2. DB 実行して ID を抽出
        let candidate_ids = fetch_ids(self.conn, &select_sql)?;

        Ok(PickPlan {
            select_sql,
            candidate_ids,
        })
    }

    /// クエリをトップレベルのスカラー式（集約計算など）として実行し、単一の数値を返します。
    pub fn fetch_scalar(&self) -> Result<f64> {
        let op = self.resolver.get_scalar_expression().ok_or_else(|| {
            anyhow::anyhow!("Query is not a top-level scalar expression")
        })?;

        let sql = crate::query::sql::build_resolved_scalar_sql(&op, "oneview");

        let sql_str = sql.to_string(sea_query::PostgresQueryBuilder);
        if std::env::var("TTFM_DEBUG").is_ok() {
            println!("--- FETCH SCALAR SQL ---\n{}\n----------------", sql_str);
        }

        self.conn
            .prepare(&sql_str)?
            .query_row([], |r| {
                let val: duckdb::types::Value = r.get(0)?;
                match val {
                    duckdb::types::Value::Null => Ok(0.0),
                    duckdb::types::Value::Float(f) => Ok(f as f64),
                    duckdb::types::Value::Double(d) => Ok(d),
                    duckdb::types::Value::Int(i) => Ok(i as f64),
                    duckdb::types::Value::BigInt(i) => Ok(i as f64),
                    duckdb::types::Value::HugeInt(i) => Ok(i as f64),
                    other => {
                        // 文字列等の場合はパースを試みる
                        let s_val = format!("{:?}", other);
                        if let Ok(f) = s_val.trim_matches('"').parse::<f64>() {
                            Ok(f)
                        } else {
                            Err(duckdb::Error::InvalidColumnType(
                                0,
                                s_val,
                                duckdb::types::Type::Double,
                            ))
                        }
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("Failed to fetch scalar: {}", e))
    }

    /// ブーリアンのみを返す特殊なクエリを実行します。
    pub fn fetch_boolean(&self) -> Result<bool> {
        let sql = crate::query::sql::build_boolean_sql(
            &self.resolver.resolved_query,
            "oneview",
        );
        let sql_str = sql.to_string(sea_query::PostgresQueryBuilder);

        if std::env::var("TTFM_DEBUG").is_ok() {
            println!(
                "--- FETCH BOOLEAN SQL ---\n{}\n----------------",
                sql_str
            );
        }

        // COALESCE(MAX(item_id), 0) の結果 (1 or 0) が返るはず
        let id_val: i64 = self.conn.query_row(&sql_str, [], |r| r.get(0))?;
        Ok(id_val == 1)
    }

    /// 条件に合致するアイテムと、その全タグを 1 クエリで取得します。
    pub fn fetch_items(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        use sea_query::PostgresQueryBuilder;

        let select_sql = crate::query::sql::build_fetch_items_sql(
            &self.resolver.resolved_query,
            "oneview",
            limit,
            offset,
        );

        let sql_str = select_sql.to_string(PostgresQueryBuilder);
        if std::env::var("TTFM_DEBUG").is_ok() {
            println!("--- FETCH ITEMS SQL ---\n{}\n----------------", sql_str);
        }

        let mut stmt = self.conn.prepare(&sql_str)?;
        let item_iter = stmt.query_map([], |row| {
            // 列名の重複回避のため、ID 直接指定ではなく「構造化データ全体」を受け取るのを理想とするが
            // 現状の fetch_items_sql が生成する単一行から復元する。
            self.decode_item_from_row(row)
        })?;

        let mut results = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for item in item_iter {
            let item = item?;
            if seen_ids.insert(item.id) {
                results.push(item);
            }
        }
        Ok(results)
    }

    /// ラベル（型）ごとの集約結果を取得します。
    pub fn fetch_label_groups(
        &self,
        proj_type: &TagType,
        limit: usize,
        offset: usize,
    ) -> Result<crate::response::PagedResult<crate::response::LabelGroup>> {
        use crate::response::{LabelGroup, PagedResult};
        use duckdb::types::Value;
        use sea_query::PostgresQueryBuilder;

        let select_sql = crate::query::sql::build_fetch_label_groups_sql(
            self.resolver,
            proj_type,
            "oneview",
            limit,
            offset,
        )?;

        let sql_str = select_sql.to_string(PostgresQueryBuilder);
        if std::env::var("TTFM_DEBUG").is_ok() {
            println!(
                "--- FETCH LABEL GROUPS SQL ---\n{}\n----------------",
                sql_str
            );
        }

        let mut stmt = self.conn.prepare(&sql_str)?;
        let mut rows = stmt.query([])?;
        let mut groups = Vec::new();

        let desc = self.resolver.lens().look_up_or_default(proj_type);
        let main_col_name = match &desc.storage {
            StorageMapping::Column(col) => col.name(),
            StorageMapping::RowTag { column, .. } => column.name(),
            _ => anyhow::bail!(
                "Unsupported storage for projection: {:?}",
                desc.storage
            ),
        };

        while let Some(row) = rows.next()? {
            use sea_query::Iden;
            let label_val: Value = row.get(main_col_name.as_str())?;
            let total_count: i64 =
                row.get(Iden::to_string(&SType::Label).as_str())?;
            let Value::List(items_list_of_lists) = row.get(
                Iden::to_string(&crate::db::Tbl::AggregatedItems).as_str(),
            )?
            else {
                continue;
            };

            let results = self.decode_grouped_items(items_list_of_lists);

            groups.push(LabelGroup {
                label: self
                    .resolver
                    .lens()
                    .resolve_label(proj_type, &label_val),
                results,
                total_count: total_count as usize,
            });
        }

        Ok(PagedResult::new(groups, limit, offset))
    }

    /// 平坦なタグデータのリストを取得（メモリ上での利用・デバッグ用）
    pub fn fetch_flat_table(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Vec<RawTagRow>> {
        use sea_query::PostgresQueryBuilder;
        let select_sql = crate::query::sql::build_flat_table_sql(
            &self.resolver.resolved_query,
            &self.resolver.expanded_query,
            "oneview",
            limit,
            offset,
        );
        let sql_str = select_sql.to_string(PostgresQueryBuilder);

        let mut stmt = self.conn.prepare(&sql_str)?;
        let rows = stmt.query_map([], |row| RawTagRow::from_row(row))?;

        let mut results = Vec::new();
        for res in rows {
            results.push(res?);
        }
        Ok(results)
    }

    /// 高速に Parquet 保存（キャッシュ生成用）
    pub fn fetch_save_flat_table(
        &self,
        path: &Path,
        metadata: Option<&HashMap<String, String>>,
    ) -> Result<()> {
        let select_sql = crate::query::sql::build_flat_table_sql(
            &self.resolver.resolved_query,
            &self.resolver.expanded_query,
            "oneview",
            None,
            None,
        );
        crate::util::save_parquet(self.conn, &select_sql, path, metadata)
    }

    /// プロジェクション（ラベル集計）で得られた、入れ子構造のアイテムリストをデコードします。
    fn decode_grouped_items(
        &self,
        items_list: Vec<Value>,
    ) -> Vec<SearchResult> {
        items_list
            .into_iter()
            .filter_map(|v| match v {
                Value::List(l) => Some(l),
                _ => None,
            })
            .filter_map(|item_tags| {
                let mut it = item_tags.into_iter().filter_map(|v| match v {
                    Value::Struct(m) => Some(m),
                    _ => None,
                });

                let first_map = it.next()?;
                let mut res = self.decode_item_from_map(&first_map).ok()?;
                for tag_map in it {
                    let _ = self.decode_item_tags_from_map(&mut res, &tag_map);
                }
                Some(res)
            })
            .collect()
    }

    /// DuckDB の Row から SearchResult を構築します。
    fn decode_item_from_row(
        &self,
        row: &duckdb::Row,
    ) -> duckdb::Result<SearchResult> {
        use duckdb::types::Value;

        let item_kind: String = row.get(SType::ItemKind.name().as_str())?;
        let id_val: i64 = row.get(SType::ItemId.name().as_str())?;

        let id = if item_kind == "virtual" {
            // "virtual" の場合、id_val は 1 (True) or 0 (False)
            // もし値が想定外なら 0 (False) 扱いにする等の安全策
            let val = if id_val != 0 { 1 } else { 0 };
            ItemId::Virtual(crate::types::VirtualItem::Boolean(val))
        } else {
            ItemId::Real(id_val)
        };

        let mut res = SearchResult::new_empty(id);
        res.rank = row
            .get::<_, Option<i64>>(SType::Rank.name().as_str())?
            .unwrap_or(0);
        res.item_kind = item_kind;

        let Value::List(tags) =
            row.get(crate::db::QueryResultCol::Tags.to_string().as_str())?
        else {
            return Ok(res);
        };

        for v in tags {
            let Value::Struct(map) = v else {
                continue;
            };
            if let Some(row) = RawTagRow::from_map(&map) {
                #[allow(deprecated)]
                res.apply_raw_tag(row);
            }
        }
        Ok(res)
    }

    /// 物理カラムの Map (struct_pack 結果) から SearchResult を構築します。
    fn decode_item_from_map(
        &self,
        map: &duckdb::types::OrderedMap<String, duckdb::types::Value>,
    ) -> Result<SearchResult> {
        use duckdb::types::Value;

        let id = map
            .get(&SType::ItemId.name())
            .and_then(|v| match v {
                Value::BigInt(i) => Some(*i),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("Missing item_id in packed data"))?;

        let mut res = SearchResult::new_empty(id.into());
        res.rank = map
            .get(&SType::Rank.name())
            .and_then(|v| match v {
                Value::BigInt(i) => Some(*i),
                _ => None,
            })
            .unwrap_or(0);
        res.item_kind = map
            .get(&SType::ItemKind.name())
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();

        self.decode_item_tags_from_map(&mut res, map)?;

        Ok(res)
    }

    /// Map 形式のタグデータ（1行分）を SearchResult に適用します。
    fn decode_item_tags_from_map(
        &self,
        res: &mut SearchResult,
        map: &duckdb::types::OrderedMap<String, duckdb::types::Value>,
    ) -> Result<()> {
        use duckdb::types::Value;

        let type_key = SType::Type.name();
        let origin_key = SType::Origin.name();

        let Some(tag_type_str) = map.get(&type_key).and_then(|v| match v {
            Value::Text(s) => Some(s.as_str()),
            _ => None,
        }) else {
            return Ok(());
        };

        let tag_type = TagType::from(tag_type_str);
        if let Some(label) =
            self.resolver.lens().decode_label_from_map(&tag_type, map)
        {
            let origin = map
                .get(&origin_key)
                .and_then(|v| match v {
                    Value::Text(s) if s == "user" => Some(Origin::User),
                    _ => None,
                })
                .unwrap_or(Origin::System);

            res.apply_tag(label, origin);
        }

        Ok(())
    }
}

/// DB から ID リストを抽出する汎用ヘルパー。
pub fn fetch_ids(
    conn: &duckdb::Connection,
    select_sql: &sea_query::SelectStatement,
) -> Result<Vec<i64>> {
    use sea_query::PostgresQueryBuilder;
    let sql_str = select_sql.to_string(PostgresQueryBuilder);
    if std::env::var("TTFM_DEBUG").is_ok() {
        println!("--- PICK SQL ---\n{}\n----------------", sql_str);
    }
    let mut stmt = conn.prepare(&sql_str)?;
    let id_iter = stmt.query_map([], |row| row.get::<_, i64>(0))?;

    let mut candidate_ids = Vec::new();
    for id in id_iter {
        candidate_ids.push(id?);
    }
    candidate_ids.sort_unstable();
    candidate_ids.dedup();
    Ok(candidate_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::QueryNode;
    use crate::query::lens_resolver::ResolvedNode;
    use crate::query::lens_schema::StorageMapping;
    use crate::types::{Label, SType, TagType};

    #[test]
    fn test_expand_query_recursive() {
        // Focused Lens 生成（ここでパース・展開・解決が行われる）
        let resolver =
            crate::query::lens_resolver::Resolver::new("directory:docs")
                .unwrap();
        let expanded = &resolver.expanded_query;

        let _target_label = Label::from("docs");

        // 少なくとも TypedTag(Directory) ではなくなっているはず
        if let QueryNode::TypedTag(tt) = &expanded {
            assert_ne!(tt.label.tag_type(), TagType::Base(SType::Directory));
        }
    }

    #[test]
    fn test_resolve_query_physical_mapping() {
        // Focused Lens 生成
        let resolver =
            crate::query::lens_resolver::Resolver::new("size:100").unwrap();
        let resolved = &resolver.resolved_query;

        if let ResolvedNode::Match {
            storage, sql_type, ..
        } = resolved
        {
            match storage {
                StorageMapping::RowTag { tag_key, .. } => {
                    assert_eq!(tag_key, "size")
                }
                _ => panic!("Expected RowTag mapping for size"),
            }
            // Size は LabelInt (BIGINT)
            assert_eq!(*sql_type, crate::db::SqlType::BIGINT);
        } else {
            panic!("Expected Match node");
        }
    }

    #[test]
    fn test_pick_integration() {
        std::env::set_var("TTFM_DEBUG", "1");

        let conn = duckdb::Connection::open_in_memory().unwrap();
        // モックテーブル作成
        conn.execute("CREATE TABLE oneview (
            item_id BIGINT, rank BIGINT, item_kind TEXT, origin TEXT, type TEXT,
            label_str TEXT, label_int BIGINT, label_double DOUBLE, label_bool BOOLEAN
        )", []).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES 
            (1, 10, 'file', 'user', 'extension', 'rs', NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES 
            (1, 10, 'file', 'user', 'is_dir', 'false', NULL, NULL, FALSE)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES 
            (2, 5, 'file', 'user', 'extension', 'txt', NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES 
            (2, 5, 'file', 'user', 'is_dir', 'false', NULL, NULL, FALSE)",
            [],
        )
        .unwrap();

        let resolver =
            crate::query::lens_resolver::Resolver::new("extension:rs").unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let plan = fetcher.pick(None, None).unwrap();

        assert_eq!(plan.candidate_ids, vec![1]);
    }

    #[test]
    fn test_fetch_flat_table() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE oneview (
            item_id BIGINT, rank BIGINT, item_kind TEXT, origin TEXT, type TEXT,
            label_str TEXT, label_int BIGINT, label_double DOUBLE, label_bool BOOLEAN
        )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES 
            (1, 10, 'file', 'user', 'extension', 'rs', NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES 
            (1, 10, 'file', 'user', 'is_dir', 'false', NULL, NULL, FALSE)",
            [],
        )
        .unwrap();

        let resolver =
            crate::query::lens_resolver::Resolver::new("extension:rs").unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let results = fetcher.fetch_flat_table(None, None).unwrap();
        assert_eq!(results.len(), 2); // extension + is_dir
        assert!(results.iter().any(|r| r.tag_type == "extension"));
        assert!(results.iter().any(|r| r.tag_type == "is_dir"));
    }

    #[test]
    fn test_fetch_save_flat_table() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE oneview (
            item_id BIGINT, rank BIGINT, item_kind TEXT, origin TEXT, type TEXT,
            label_str TEXT, label_int BIGINT, label_double DOUBLE, label_bool BOOLEAN
        )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES 
            (1, 10, 'file', 'user', 'extension', 'rs', NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES 
            (1, 10, 'file', 'user', 'is_dir', 'false', NULL, NULL, FALSE)",
            [],
        )
        .unwrap();

        let resolver =
            crate::query::lens_resolver::Resolver::new("extension:rs").unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("test.parquet");

        fetcher.fetch_save_flat_table(&path, None).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_fetch_boolean() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE oneview (
            item_id BIGINT, rank BIGINT, item_kind TEXT, origin TEXT, type TEXT,
            label_str TEXT, label_int BIGINT, label_double DOUBLE, label_bool BOOLEAN
        )",
            [],
        )
        .unwrap();

        // 1. max(mtime:) < 2026-02-01 (should be TRUE if we have appropriate data)
        // データがない -> fetch_boolean は FALSE (0) を返すはず
        let resolver = crate::query::lens_resolver::Resolver::new(
            "max(mtime:) < 2026-02-01",
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let res = fetcher.fetch_boolean().unwrap();
        assert!(!res); // FALSE

        // データ投入
        conn.execute(
            "INSERT INTO oneview VALUES 
            (1, 10, 'file', 'user', 'mtime', NULL, 100, NULL, NULL)",
            [],
        )
        .unwrap();
        // mtime=100 < 2026-02-01 (huge number) -> TRUE
        // Date parsing happens at lens resolution time, so 2026-02-01 becomes a timestamp integer.
        // Assuming the query parser works correctly, this should return TRUE.

        let res2 = fetcher.fetch_boolean().unwrap();
        assert!(res2); // TRUE
    }
}
