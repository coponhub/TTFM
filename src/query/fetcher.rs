use crate::query::lens::{Lens, StorageMapping};
use crate::response::{RawTagRow, SearchResult};
use crate::types::{Origin, SType, TagType};
use anyhow::Result;
use duckdb::types::Value;

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
    pub lens: &'a Lens,
    pub conn: &'a duckdb::Connection,
}

impl<'a> Fetcher<'a> {
    pub fn new(lens: &'a Lens, conn: &'a duckdb::Connection) -> Self {
        Self { lens, conn }
    }

    /// 合致するアイテムの ID と Rank を抽出する計画を立て、実行します。
    pub fn pick(
        &self,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<PickPlan> {
        use sea_query::{Expr, Order};

        let resolved = &self.lens.resolved_query;

        // 1. SQL 構築
        let mut select_sql =
            crate::query::sql::build_pick_sql(resolved, "oneview");

        // 検索仕様に基づき Rank と ItemId で降順ソート
        let rank_col = self.lens.resolve_col(SType::Rank)?;
        let id_col = self.lens.resolve_col(SType::ItemId)?;

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

    /// 条件に合致するアイテムと、その全タグを 1 クエリで取得します。
    pub fn fetch_items(
        &self,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        use sea_query::PostgresQueryBuilder;

        let select_sql = crate::query::sql::build_fetch_items_sql(
            &self.lens.resolved_query,
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
            self.lens, proj_type, "oneview", limit, offset,
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

        let desc = self.lens.look_up_or_default(proj_type);
        let main_col_name = match &desc.storage {
            StorageMapping::Column(col) => col.name(),
            StorageMapping::RowTag { column, .. } => column.name(),
            _ => anyhow::bail!(
                "Unsupported storage for projection: {:?}",
                desc.storage
            ),
        };

        while let Some(row) = rows.next()? {
            let label_val: Value = row.get(main_col_name.as_str())?;
            let total_count: i64 = row.get(SType::Label.name().as_str())?;
            let Value::List(items_list_of_lists) =
                row.get("aggregated_items")?
            else {
                continue;
            };

            let results = self.decode_grouped_items(items_list_of_lists);

            groups.push(LabelGroup {
                label: self.lens.resolve_label(proj_type, &label_val),
                results,
                total_count: total_count as usize,
            });
        }

        Ok(PagedResult::new(groups, limit, offset))
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

        let id: i64 = row.get(SType::ItemId.name().as_str())?;
        let mut res = SearchResult::new_empty(id);
        res.rank = row
            .get::<_, Option<i64>>(SType::Rank.name().as_str())?
            .unwrap_or(0);
        res.item_kind = row.get(SType::ItemKind.name().as_str())?;

        let Value::List(tags) = row.get("tags")? else {
            return Ok(res);
        };

        for v in tags {
            let Value::Struct(map) = v else {
                continue;
            };
            if let Some(row) = RawTagRow::from_map(&map) {
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

        let mut res = SearchResult::new_empty(id);
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
        if let Some(label) = self.lens.decode_label_from_map(&tag_type, map) {
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
    use crate::query::lens::{Lens, ResolvedNode, StorageMapping};
    use crate::types::{Label, SType, TagType};

    #[test]
    fn test_expand_query_recursive() {
        // Focused Lens 生成（ここでパース・展開・解決が行われる）
        let lens = Lens::with_standard("directory:docs").unwrap();
        let expanded = &lens.expanded_query;

        let _target_label = Label::from("docs");

        // 少なくとも TypedTag(Directory) ではなくなっているはず
        if let QueryNode::TypedTag(tt) = &expanded {
            assert_ne!(tt.label.tag_type(), TagType::Base(SType::Directory));
        }
    }

    #[test]
    fn test_resolve_query_physical_mapping() {
        // Focused Lens 生成
        let lens = Lens::with_standard("size:100").unwrap();
        let resolved = &lens.resolved_query;

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

        let lens = Lens::with_standard("extension:rs").unwrap();
        let fetcher = Fetcher::new(&lens, &conn);

        let plan = fetcher.pick(None, None).unwrap();

        assert_eq!(plan.candidate_ids, vec![1]);
    }
}
