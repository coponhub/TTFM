use crate::query::lens_resolver::Resolver;
use crate::response::{RawTagRow, SearchResult};
use crate::types::{ItemId, ItemKind, SType, TagType};
use anyhow::Result;
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

    /// 新規：計算クエリ（集約またはブーリアン）を実行し、SearchResult形式で返します。
    pub fn fetch_computation(&self) -> Result<SearchResult> {
        if let Some(_) = self.resolver.get_scalar_expression() {
            self.compute_aggregation()
        } else if self.resolver.resolved_query.is_boolean_result() {
            self.compute_boolean()
        } else {
            Err(anyhow::anyhow!(
                "Query is not a computation (scalar or boolean expression)"
            ))
        }
    }

    /// クエリをトップレベルのスカラー式（集約計算など）として実行し、SearchResultを返します。
    fn compute_aggregation(&self) -> Result<SearchResult> {
        let op = self.resolver.get_scalar_expression().ok_or_else(|| {
            anyhow::anyhow!("Query is not a top-level scalar expression")
        })?;

        let sql = crate::query::sql::build_resolved_scalar_sql(&op, "oneview");

        let sql_str = sql.to_string(sea_query::PostgresQueryBuilder);
        if std::env::var("TTFM_DEBUG").is_ok() {
            println!(
                "--- COMPUTE AGGREGATION SQL ---\n{}\n----------------",
                sql_str
            );
        }

        let mut stmt = self.conn.prepare(&sql_str)?;
        let val_id = stmt
            .query_row([], |r| {
                let val: duckdb::types::Value = r.get(0)?;
                Ok(val)
            })
            .map_err(|e| {
                anyhow::anyhow!("Failed to compute aggregation: {}", e)
            })?;
        let duckdb_type_str = format!("{:?}", stmt.column_type(0));
        use crate::types::{Label, LabelValue, TagType};

        let label_val = LabelValue::from(val_id.clone());
        let type_name = if let LabelValue::Null = label_val {
            crate::util::get_db_coltype(&duckdb_type_str)
        } else {
            (&label_val).into()
        };

        let id = ItemId::new_volatile();
        let name = label_val.as_display_name();

        let mut res = SearchResult::new_empty(id, ItemKind::Volatile, name);

        // 1. 型分類タグ (type:integer 等)
        res.apply_tag(
            Label::resolve(
                TagType::Base(crate::types::SType::Type),
                LabelValue::String(type_name.to_string()),
            ),
            crate::types::Origin::System,
        );

        // 2. 実値タグ (value:123 等)
        res.apply_tag(
            Label::resolve(
                TagType::Base(crate::types::SType::Value),
                label_val,
            ),
            crate::types::Origin::System,
        );

        Ok(res)
    }

    /// ブーリアンのみを返す特殊なクエリを実行し、SearchResultを返します。
    fn compute_boolean(&self) -> Result<SearchResult> {
        let sql = crate::query::sql::build_boolean_sql(
            &self.resolver.resolved_query,
            "oneview",
        );
        let sql_str = sql.to_string(sea_query::PostgresQueryBuilder);

        if std::env::var("TTFM_DEBUG").is_ok() {
            println!(
                "--- COMPUTE BOOLEAN SQL ---\n{}\n----------------",
                sql_str
            );
        }

        // NULL対応: Option<i64> で受け取る
        let mut stmt = self.conn.prepare(&sql_str)?;
        let id_val: Option<i64> = stmt.query_row([], |r| r.get(0))?;

        let id = ItemId::new_volatile();
        let label_val = match id_val {
            Some(1) => LabelValue::Boolean(true),
            Some(_) => LabelValue::Boolean(false),
            None => LabelValue::Null,
        };

        let mut res = SearchResult::new_empty(
            id,
            ItemKind::Volatile,
            label_val.as_display_name(),
        );

        // 正確な型情報を型付きタグとして注入する
        use crate::types::{Label, LabelValue, TagType};
        let type_name: &str = (&LabelValue::Boolean(true)).into();

        // 1. 型分類タグ (type:boolean 等)
        res.apply_tag(
            Label::resolve(
                TagType::Base(crate::types::SType::Type),
                LabelValue::String(type_name.to_string()),
            ),
            crate::types::Origin::System,
        );

        // 2. 実値タグ (value:true 等)
        res.apply_tag(
            Label::resolve(
                TagType::Base(crate::types::SType::Value),
                label_val,
            ),
            crate::types::Origin::System,
        );

        Ok(res)
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
            if seen_ids.insert(item.id.clone()) {
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
    ) -> Result<Vec<SearchResult>> {
        use crate::response::SearchResult;
        use crate::types::ItemId;
        use duckdb::types::Value;
        use sea_query::PostgresQueryBuilder;

        let select_sql = if let Some(node) = self.resolver.get_label_set_op_node() {
            crate::query::sql::build_fetch_label_set_op_sql(
                node, "oneview", limit, offset,
            )?
        } else {
            crate::query::sql::build_fetch_label_groups_sql(
                self.resolver,
                proj_type,
                "oneview",
                limit,
                offset,
            )?
        };

        let sql_str = select_sql.to_string(PostgresQueryBuilder);
        if std::env::var("TTFM_DEBUG").is_ok() {
            println!(
                "--- FETCH LABEL GROUPS SQL ---\n{}\n----------------",
                sql_str
            );
        }

        let mut stmt = self.conn.prepare(&sql_str)?;
        let mut rows = stmt.query([])?;
        let mut results = Vec::new();

        let operands = self
            .resolver
            .resolved_query
            .get_projection_operands();

        while let Some(row) = rows.next()? {
            // SQLから label_value, group_total, item_refs を取得
            // ラベル値を解決（複数キー対応）
            let label_str = if let Some(ops) = operands {
                let mut label_parts = Vec::new();
                if ops.len() > 1 {
                    for i in 0..ops.len() {
                        let col_name = format!("label_value_{}", i);
                        let val: Value = row.get(col_name.as_str())?;
                        label_parts.push(
                            ops[i].resolve_label(self.resolver.lens(), &val),
                        );
                    }
                } else {
                    let label_val: Value = row.get("label_value")?;
                    label_parts.push(
                        ops[0].resolve_label(self.resolver.lens(), &label_val),
                    );
                }
                label_parts.join(" &: ")
            } else {
                // LabelSetOp path: operands not available, read label_value directly
                let label_val: Value = row.get("label_value")?;
                match &label_val {
                    Value::Text(s) => s.clone(),
                    Value::BigInt(i) => i.to_string(),
                    Value::Double(d) => d.to_string(),
                    Value::Boolean(b) => b.to_string(),
                    _ => "null".to_string(),
                }
            };

            let total_count: i64 = row.get("group_total")?;
            let Value::List(item_refs_list) = row.get("item_refs")? else {
                continue;
            };

            // Label volatile item を作成
            let label_id = ItemId::new_volatile();
            let mut label_item = SearchResult::new_empty(
                label_id,
                ItemKind::Volatile,
                label_str.clone(),
            );

            // SQLで生成済みの "name#id" 文字列をタグとして追加（Type="item"は明示的に指定）
            for item_ref in item_refs_list {
                if let Value::Text(s) = item_ref {
                    let typed_tag = crate::types::TypedTag::new("item", s);
                    label_item.tags.entries.push(crate::types::TagEntry {
                        label: typed_tag.label,
                        origin: crate::types::Origin::System,
                    });
                }
            }

            // total_count を projected_label に保存
            label_item.projected_label =
                Some(crate::types::Label::from(format!("{}", total_count)));

            // nvalue カラムがある場合、タグとして格納
            if let Ok(nv) = row.get::<_, Value>("nvalue") {
                let nv_label = crate::types::LabelValue::from(nv);
                if !matches!(nv_label, crate::types::LabelValue::Null) {
                    label_item.apply_tag(
                        crate::types::Label::resolve(
                            crate::types::TagType::from("nvalue"),
                            nv_label,
                        ),
                        crate::types::Origin::System,
                    );
                }
            }

            results.push(label_item);
        }

        Ok(results)
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

    /// DuckDB の Row から SearchResult を構築します。
    fn decode_item_from_row(
        &self,
        row: &duckdb::Row,
    ) -> duckdb::Result<SearchResult> {
        use duckdb::types::Value;

        let item_kind: String = row.get(SType::ItemKind.name().as_str())?;
        let id_val: i64 = row.get(SType::ItemId.name().as_str())?;

        let kind = item_kind
            .as_str()
            .parse::<ItemKind>()
            .unwrap_or(ItemKind::Volatile);
        let id = if kind.is_volatile() {
            // "volatile" の場合、既存のカラム値は ID として扱う
            ItemId::Volatile(id_val as u64)
        } else {
            ItemId::Stored(id_val)
        };

        let mut res = SearchResult::new_empty(id, kind, String::new());
        res.rank = row
            .get::<_, Option<i64>>(SType::Rank.name().as_str())?
            .unwrap_or(0);

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
                StorageMapping::RowTag { tag_type, .. } => {
                    assert_eq!(tag_type, "size")
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

        let res = fetcher.compute_boolean().unwrap();
        assert!(res.id.is_volatile());
        assert_eq!(res.name, "NULL"); // NULL (データがないので判定不能)

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

        let res2 = fetcher.compute_boolean().unwrap();
        assert!(res2.id.is_volatile());
        assert_eq!(res2.name, "TRUE"); // TRUE
    }

    #[test]
    fn test_fetch_nvalue_tags() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE oneview (
            item_id BIGINT, rank BIGINT, item_kind TEXT, origin TEXT, type TEXT,
            label_str TEXT, label_int BIGINT, label_double DOUBLE, label_bool BOOLEAN
        )",
            [],
        )
        .unwrap();

        // item 1: parentdir=src, extension=jpg, name=photo1.jpg
        conn.execute(
            "INSERT INTO oneview VALUES (1, 10, 'file', 'user', 'parentdir', 'src', NULL, NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (1, 10, 'file', 'user', 'extension', 'jpg', NULL, NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (1, 10, 'file', 'user', 'is_dir', 'false', NULL, NULL, FALSE)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (1, 10, 'file', 'user', 'name', 'photo1.jpg', NULL, NULL, NULL)",
            [],
        ).unwrap();

        // item 2: parentdir=src, extension=png, name=image.png
        conn.execute(
            "INSERT INTO oneview VALUES (2, 5, 'file', 'user', 'parentdir', 'src', NULL, NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (2, 5, 'file', 'user', 'extension', 'png', NULL, NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (2, 5, 'file', 'user', 'is_dir', 'false', NULL, NULL, FALSE)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (2, 5, 'file', 'user', 'name', 'image.png', NULL, NULL, NULL)",
            [],
        ).unwrap();

        // item 3: parentdir=docs, extension=jpg, name=photo2.jpg
        conn.execute(
            "INSERT INTO oneview VALUES (3, 3, 'file', 'user', 'parentdir', 'docs', NULL, NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (3, 3, 'file', 'user', 'extension', 'jpg', NULL, NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (3, 3, 'file', 'user', 'is_dir', 'false', NULL, NULL, FALSE)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (3, 3, 'file', 'user', 'name', 'photo2.jpg', NULL, NULL, NULL)",
            [],
        ).unwrap();

        std::env::set_var("TTFM_DEBUG", "1");

        // parentdir: &: count(extension:jpg) → src=1, docs=1
        let resolver = crate::query::lens_resolver::Resolver::new(
            "parentdir: &: count(extension:jpg)",
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);
        let proj_type =
            resolver.get_projection().expect("Should have projection");

        let results = fetcher.fetch_label_groups(&proj_type, 100, 0).unwrap();

        // 2つの parentdir グループ: docs, src
        assert_eq!(results.len(), 2, "Should have 2 parentdir groups");

        // 各グループに nvalue タグがあることを確認
        for item in &results {
            let nvalue_tag = item.tags.entries.iter().find(|e| {
                e.label.tag_type() == crate::types::TagType::from("nvalue")
            });
            assert!(
                nvalue_tag.is_some(),
                "Label '{}' should have nvalue tag",
                item.name
            );
        }

        // docs: jpg 1件, src: jpg 1件
        let docs = results.iter().find(|r| r.name == "docs").unwrap();
        let docs_nvalue = docs
            .tags
            .entries
            .iter()
            .find(|e| {
                e.label.tag_type() == crate::types::TagType::from("nvalue")
            })
            .unwrap();
        assert_eq!(
            docs_nvalue.label.as_str(),
            "1",
            "docs should have 1 jpg file"
        );

        let src = results.iter().find(|r| r.name == "src").unwrap();
        let src_nvalue = src
            .tags
            .entries
            .iter()
            .find(|e| {
                e.label.tag_type() == crate::types::TagType::from("nvalue")
            })
            .unwrap();
        assert_eq!(
            src_nvalue.label.as_str(),
            "1",
            "src should have 1 jpg file"
        );
    }

    #[test]
    fn test_fetch_label_groups_no_nvalue_regression() {
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
            "INSERT INTO oneview VALUES (1, 10, 'file', 'user', 'extension', 'rs', NULL, NULL, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (1, 10, 'file', 'user', 'is_dir', 'false', NULL, NULL, FALSE)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (1, 10, 'file', 'user', 'name', 'main.rs', NULL, NULL, NULL)",
            [],
        ).unwrap();

        let resolver =
            crate::query::lens_resolver::Resolver::new("extension:").unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);
        let proj_type =
            resolver.get_projection().expect("Should have projection");

        let results = fetcher.fetch_label_groups(&proj_type, 100, 0).unwrap();
        assert_eq!(results.len(), 1);

        // nvalue タグがないことを確認
        let has_nvalue = results[0].tags.entries.iter().any(|e| {
            e.label.tag_type() == crate::types::TagType::from("nvalue")
        });
        assert!(!has_nvalue, "Normal projection should NOT have nvalue tag");
    }
}
