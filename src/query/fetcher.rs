// Copyright (C) 2026 Kensuke Aoyagi
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

use crate::db::Src;
use crate::query::lens_resolver::Resolver;
use crate::query::sql::PickNode;
use crate::response::{Item, RawTagRow};
use crate::types::{ItemId, ItemKind, SType};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

/// クエリに基づきデータベースからデータを取得（Fetch）を担当する。
pub struct Fetcher<'a> {
    pub resolver: &'a Resolver,
    pub conn: &'a duckdb::Connection,
}

impl<'a> Fetcher<'a> {
    pub fn new(resolver: &'a Resolver, conn: &'a duckdb::Connection) -> Self {
        Self { resolver, conn }
    }

    /// クエリの種別を ResolvedNode の構造から判断し、適切な結果を返す単一エントリポイント。
    ///
    /// `n` は要求件数（0 = 全件）。内部で n+1 件取得して has_more 判定を呼び出し側に委ねる。
    pub fn fetch(&self, n: usize, offset: usize) -> Result<Vec<Item>> {
        self.fetch_from(&Src::OneView, n, offset)
    }

    pub fn fetch_from(
        &self,
        src: &Src,
        n: usize,
        offset: usize,
    ) -> Result<Vec<Item>> {
        use sea_query::PostgresQueryBuilder;
        let resolver = &self.resolver;

        let sql = crate::query::sql::build_fetch_sql(src, resolver, n, offset)?;
        let sql_str = sql.to_string(PostgresQueryBuilder);

        if std::env::var("TTFM_DEBUG").is_ok() {
            eprintln!("DEBUG: FETCH SQL: {}", sql_str);
        }

        // representative を持つ結果（projection / 単独 DefinitionRef）→ decode_nest_item_from_row
        if resolver.has_representative() {
            let mut stmt = self.conn.prepare(&sql_str)?;
            let results = stmt
                .query_map([], |row| self.decode_nest_item_from_row(row))?
                .collect::<duckdb::Result<Vec<_>>>()?;
            return Ok(results);
        }

        // 通常アイテム / ブーリアン / スカラー → decode_item_from_row (重複除去付き)
        let mut stmt = self.conn.prepare(&sql_str)?;
        let mut results = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for item in stmt.query_map([], |row| self.decode_item_from_row(row))? {
            let item = item?;
            if seen_ids.insert(item.id.clone()) {
                results.push(item);
            }
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
        let pick = crate::query::sql::PickNode::new(
            &Src::OneView,
            &self.resolver.resolved_query,
        );
        let select_sql = crate::query::sql::build_flat_table_sql(
            &Src::OneView,
            &pick,
            &self.resolver.expanded_query,
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
        let pick = PickNode::new(&Src::OneView, &self.resolver.resolved_query);
        let select_sql = crate::query::sql::build_flat_table_sql(
            &Src::OneView,
            &pick,
            &self.resolver.expanded_query,
            None,
            None,
        );
        crate::util::save_parquet(self.conn, &select_sql, path, metadata)
    }

    /// DuckDB の Row から Item を構築します（通常アイテム用）。
    fn decode_item_from_row(&self, row: &duckdb::Row) -> duckdb::Result<Item> {
        use crate::types::{Label, LabelValue};
        let (mut res, raw_tags) = read_base_from_row(row)?;
        for tag_row in raw_tags {
            if tag_row.tag_type == "name" {
                let lv = LabelValue::from(tag_row.value);
                if !matches!(lv, LabelValue::Null) {
                    let s = lv.as_display_name();
                    let s =
                        s.parse::<f64>().map(|f| f.to_string()).unwrap_or(s);
                    res.representative = vec![Label::Name(s)];
                }
            } else {
                #[allow(deprecated)]
                res.apply_raw_tag(tag_row);
            }
        }
        Ok(res)
    }

    /// DuckDB の Row から Projection (Nest) 結果の Item を構築します。
    /// Representative カラムから代表値リストを取得し、型付き Label に変換する。
    /// item_count タグは res.item_count に移動する。
    fn decode_nest_item_from_row(
        &self,
        row: &duckdb::Row,
    ) -> duckdb::Result<Item> {
        use crate::types::{Label, LabelValue, TagType};
        use duckdb::types::Value;

        let (mut res, raw_tags) = read_base_from_row(row)?;

        // Representative カラムから代表値リストを取得
        let repr_col =
            sea_query::Iden::to_string(&crate::db::Pronoun::Representative);
        if let Ok(Value::List(values)) = row.get::<_, Value>(repr_col.as_str())
        {
            let operands =
                self.resolver.resolved_query.get_projection_operands();

            let mut representative = Vec::new();
            for (i, v) in values.into_iter().enumerate() {
                // LabelValue::from は内部で Value::Union を再帰的に解く
                let lv = LabelValue::from(v);

                if let Some(ops) = operands {
                    if let Some(op) = ops.get(i) {
                        if let crate::query::lens_resolver::ResolvedOperand::TagRef {
                            tag_type,
                            ..
                        } = op
                        {
                            // 型情報がある場合はそれを尊重する
                            representative.push(Label::resolve(tag_type.clone(), lv));
                            continue;
                        }
                    }
                }
                // 型情報が不明な場合は Name タグとして扱う (Lv.1 互換)
                representative.push(Label::Name(lv.as_display_name()));
            }
            res.representative = representative;
        }

        for tag_row in raw_tags {
            match tag_row.tag_type.as_str() {
                "item_count" => {
                    let lv = LabelValue::from(tag_row.value);
                    if !matches!(lv, LabelValue::Null) {
                        res.item_count = Some(Label::resolve(
                            TagType::from("item_count"),
                            lv,
                        ));
                    }
                }
                "name" => {
                    // Lv.1 互換: representative が空の場合のみセット
                    if res.representative.is_empty() {
                        let lv = LabelValue::from(tag_row.value.clone());
                        if !matches!(lv, LabelValue::Null) {
                            let s = lv.as_display_name();
                            let s = s
                                .parse::<f64>()
                                .map(|f| f.to_string())
                                .unwrap_or(s);
                            res.representative = vec![Label::Name(s)];
                        }
                    }
                    #[allow(deprecated)]
                    res.apply_raw_tag(tag_row);
                }
                _ => {
                    #[allow(deprecated)]
                    res.apply_raw_tag(tag_row);
                }
            }
        }
        Ok(res)
    }
}

/// DuckDB Row から item_id / item_kind / rank と raw tag rows を読み取る共通ヘルパー。
fn read_base_from_row(
    row: &duckdb::Row,
) -> duckdb::Result<(crate::response::Item, Vec<RawTagRow>)> {
    use duckdb::types::Value;

    let item_kind: String = row.get(SType::ItemKind.name().as_str())?;
    let id_val: i64 = row.get(SType::ItemId.name().as_str())?;

    let kind = item_kind
        .as_str()
        .parse::<ItemKind>()
        .unwrap_or(ItemKind::Volatile);
    let id = if kind.is_volatile() {
        ItemId::Volatile(id_val as u64)
    } else {
        ItemId::Stored(id_val)
    };

    let mut res = crate::response::Item::new_empty(id, kind);
    res.rank = row
        .get::<_, Option<i64>>(SType::Rank.name().as_str())?
        .unwrap_or(0);

    let raw_tags = match row
        .get::<_, Value>(crate::db::QueryResultCol::Tags.to_string().as_str())?
    {
        Value::List(tags) => tags
            .into_iter()
            .filter_map(|v| {
                if let Value::Struct(map) = v {
                    RawTagRow::from_map(&map)
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    };

    Ok((res, raw_tags))
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
    use crate::tag::TagRegistry;
    use crate::types::{Label, SType, TagType};

    #[test]
    fn test_expand_query_recursive() {
        // Focused Lens 生成（ここでパース・展開・解決が行われる）
        let resolver = crate::query::lens_resolver::Resolver::new(
            "directory:docs",
            &TagRegistry::with_standard(),
        )
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
        let resolver = crate::query::lens_resolver::Resolver::new(
            "size:100",
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let resolved = &resolver.resolved_query;

        if let ResolvedNode::Match {
            storage, sql_type, ..
        } = resolved
        {
            match storage {
                StorageMapping::Basic { tag_type, .. } => {
                    assert_eq!(tag_type, "size")
                }
                _ => panic!("Expected Basic mapping for size"),
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

        let resolver = crate::query::lens_resolver::Resolver::new(
            "extension:rs",
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let results = fetcher.fetch(10, 0).unwrap();
        let ids: Vec<_> = results.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![crate::types::ItemId::Stored(1)]);
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

        let resolver = crate::query::lens_resolver::Resolver::new(
            "extension:rs",
            &TagRegistry::with_standard(),
        )
        .unwrap();
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

        let resolver = crate::query::lens_resolver::Resolver::new(
            "extension:rs",
            &TagRegistry::with_standard(),
        )
        .unwrap();
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
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let mut results = fetcher.fetch(100, 0).unwrap();
        let res = results.remove(0);
        assert!(res.id.is_volatile());
        assert_eq!(res.raw_repr(), "NULL"); // NULL (データがないので判定不能)

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

        let mut results2 = fetcher.fetch(100, 0).unwrap();
        let res2 = results2.remove(0);
        assert!(res2.id.is_volatile());
        assert_eq!(res2.raw_repr(), "TRUE"); // TRUE
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
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let results = fetcher.fetch(100, 0).unwrap();

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
                item.raw_repr()
            );
        }

        // docs: jpg 1件, src: jpg 1件
        let docs = results.iter().find(|r| r.raw_repr() == "docs").unwrap();
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

        let src = results.iter().find(|r| r.raw_repr() == "src").unwrap();
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
    fn test_fetch_projection_no_nvalue_regression() {
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

        let resolver = crate::query::lens_resolver::Resolver::new(
            "extension:",
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let results = fetcher.fetch(100, 0).unwrap();
        assert_eq!(results.len(), 1);

        // nvalue タグがないことを確認
        let has_nvalue = results[0].tags.entries.iter().any(|e| {
            e.label.tag_type() == crate::types::TagType::from("nvalue")
        });
        assert!(!has_nvalue, "Normal projection should NOT have nvalue tag");
    }

    fn make_oneview(conn: &duckdb::Connection) {
        conn.execute(
            "CREATE TABLE oneview (
            item_id BIGINT, rank BIGINT, item_kind TEXT, origin TEXT, type TEXT,
            label_str TEXT, label_int BIGINT, label_double DOUBLE, label_bool BOOLEAN
        )",
            [],
        )
        .unwrap();
    }

    fn insert_row(
        conn: &duckdb::Connection,
        id: i64,
        rank: i64,
        tag_type: &str,
        label_str: &str,
    ) {
        conn.execute(
            &format!(
                "INSERT INTO oneview VALUES ({}, {}, 'file', 'user', '{}', '{}', NULL, NULL, NULL)",
                id, rank, tag_type, label_str
            ),
            [],
        )
        .unwrap();
    }

    fn insert_bool_row(
        conn: &duckdb::Connection,
        id: i64,
        rank: i64,
        tag_type: &str,
        label_str: &str,
        label_bool: bool,
    ) {
        conn.execute(
            &format!(
                "INSERT INTO oneview VALUES ({}, {}, 'file', 'user', '{}', '{}', NULL, NULL, {})",
                id, rank, tag_type, label_str, label_bool
            ),
            [],
        )
        .unwrap();
    }

    // --- fetch() 統合テスト ---

    #[test]
    fn test_fetch_items_path() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        make_oneview(&conn);
        insert_row(&conn, 1, 10, "extension", "rs");
        insert_bool_row(&conn, 1, 10, "is_dir", "false", false);
        insert_row(&conn, 2, 5, "extension", "txt");
        insert_bool_row(&conn, 2, 5, "is_dir", "false", false);

        let resolver = crate::query::lens_resolver::Resolver::new(
            "extension:rs",
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let results = fetcher.fetch(100, 0).unwrap();
        assert_eq!(results.len(), 1);
        // item: タグは注入されないはず（items パス）
        let has_item_tag =
            results[0].tags.entries.iter().any(|e| {
                e.label.tag_type() == crate::types::TagType::from("item")
            });
        assert!(!has_item_tag, "Items path should not have item: tags");
    }

    #[test]
    fn test_fetch_projection_path() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        make_oneview(&conn);
        insert_row(&conn, 1, 10, "extension", "rs");
        insert_bool_row(&conn, 1, 10, "is_dir", "false", false);
        insert_row(&conn, 2, 5, "extension", "txt");
        insert_bool_row(&conn, 2, 5, "is_dir", "false", false);

        let resolver = crate::query::lens_resolver::Resolver::new(
            "extension:",
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let results = fetcher.fetch(100, 0).unwrap();
        assert_eq!(results.len(), 2, "Should have 2 extension labels");
        // item: タグが注入されているはず（projection パス）
        for r in &results {
            let has_item_tag = r.tags.entries.iter().any(|e| {
                e.label.tag_type() == crate::types::TagType::from("item")
            });
            assert!(
                has_item_tag,
                "Projection result '{}' should have item: tags",
                r.raw_repr()
            );
        }
    }

    #[test]
    fn test_fetch_scalar_path() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        make_oneview(&conn);
        conn.execute(
            "INSERT INTO oneview VALUES (1, 10, 'file', 'user', 'size', NULL, 100, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oneview VALUES (2, 5, 'file', 'user', 'size', NULL, 200, NULL, NULL)",
            [],
        )
        .unwrap();

        let resolver = crate::query::lens_resolver::Resolver::new(
            "sum(size:)",
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let results = fetcher.fetch(100, 0).unwrap();
        assert_eq!(results.len(), 1, "Scalar should return exactly 1 result");
        assert!(results[0].id.is_volatile());
    }

    #[test]
    fn test_fetch_nvalue_condition_uses_items_path() {
        // nvalue_condition 付き projection は Lv.1 フラットリストとして items パスを通る
        let conn = duckdb::Connection::open_in_memory().unwrap();
        make_oneview(&conn);
        insert_row(&conn, 1, 10, "name", "main.rs");
        insert_row(&conn, 1, 10, "parentdir", "src");
        insert_row(&conn, 1, 10, "extension", "rs");
        insert_bool_row(&conn, 1, 10, "is_dir", "false", false);
        insert_row(&conn, 2, 5, "name", "readme.md");
        insert_row(&conn, 2, 5, "parentdir", "docs");
        insert_row(&conn, 2, 5, "extension", "md");
        insert_bool_row(&conn, 2, 5, "is_dir", "false", false);

        let resolver = crate::query::lens_resolver::Resolver::new(
            "parentdir: &: (count(extension:rs) > 0)",
            &TagRegistry::with_standard(),
        )
        .unwrap();
        let fetcher = Fetcher::new(&resolver, &conn);

        let results = fetcher.fetch(100, 0).unwrap();
        // count(rs) > 0 を満たすのは src のみ → src 内のファイル (Lv.1 フラットリスト)
        assert_eq!(results.len(), 1, "Only src has rs files");
        assert_eq!(
            results[0].raw_repr(),
            "main.rs",
            "Items path returns filename, not group label"
        );
        // items パスを通るので item: タグは存在しない
        let has_item_tag =
            results[0].tags.entries.iter().any(|e| {
                e.label.tag_type() == crate::types::TagType::from("item")
            });
        assert!(!has_item_tag, "Flat list result should NOT have item: tags");
    }
}
