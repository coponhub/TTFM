use crate::query::ast::{ComparisonNode, ComparisonOp, QueryNode};
use crate::query::lens::{Lens, ResolvedNode, StorageMapping};
use crate::types::{Label, SType, TagType, TypedTag};
use anyhow::{anyhow, Result};

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
    Ok(candidate_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::lens::Lens;
    use crate::types::SType;

    #[test]
    fn test_expand_query_recursive() {
        // Focused Lens 生成（ここでパース・展開・解決が行われる）
        let lens = Lens::with_standard("directory:docs").unwrap();
        let expanded = &lens.expanded_query;

        let _target_label = Label::String("docs".to_string());

        // 少なくとも TypedTag(Directory) ではなくなっているはず
        if let QueryNode::TypedTag(tt) = &expanded {
            assert_ne!(tt.tagtype, TagType::Base(SType::Directory));
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
