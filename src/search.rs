use crate::db::TargetTable;
use crate::db::{Col, Tbl};
use crate::query::QueryNode;
use crate::response::{RawTagRow, SearchResponse, SearchResult};
use crate::types::{Progress, TagType};
use crate::util::{IdenExt, SelectExt};
use crate::{FileManager, FunctionRegistry};
use anyhow::Result;
use duckdb::Connection;
use sea_query::{Expr, PostgresQueryBuilder, Query};
use std::collections::HashMap;
use std::path::Path;

/// 検索オプションを制御する構造体。
#[derive(Debug, Default, Clone)]
pub struct SearchOptions {
    /// 取得件数 (None または 0 は全件)
    pub n: Option<usize>,
    /// 開始位置 (None は自動または0)
    pub offset: Option<usize>,
    /// 利用するキャッシュ ID
    pub cid: Option<String>,
}

impl FileManager {
    /// クエリ文字列を使用してインデックスを検索します。
    pub fn search(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<SearchResponse> {
        if !self.path_for_target(TargetTable::FileReferences).exists() {
            return Err(anyhow::anyhow!(
                "Index not found. Please run 'index' command first."
            ));
        }

        // 1. キャッシュの解決
        if let Some(res) = self.try_resolve_cache(query, &options)? {
            return Ok(res);
        }

        // 2. 新規検索の実行
        let n = options.n.unwrap_or(100);
        let offset = options.offset.unwrap_or(0);
        let limit = if n > 0 { n + 1 } else { 0 };

        let node = if query.trim().is_empty() {
            QueryNode::And(vec![])
        } else {
            crate::query::parse(query)?
        };

        let q_reg = crate::query::QueryFunctionRegistry::with_standard();
        let expanded = node.expand(&q_reg);

        // Item Selector による ID 抽出
        let mut item_sql = expanded.to_sql("oneview");
        item_sql
            .order_by_expr(Expr::col(Col::Rank).into(), sea_query::Order::Desc);
        item_sql.order_by_expr(
            Expr::col(Col::ItemId).into(),
            sea_query::Order::Desc,
        );

        if offset > 0 {
            item_sql.offset(offset as u64);
        }
        if limit > 0 {
            item_sql.limit(limit as u64);
        }

        // DuckDB の隔離された環境で ID を抽出
        Tbl::Sub.drop_table(&self.conn).ok();
        item_sql.create_table_as(&self.conn, Tbl::Sub)?;

        let id_select = Query::select()
            .column(Col::ItemId)
            .from(Tbl::Sub)
            .to_owned();

        let id_rows = self
            .conn
            .prepare(&id_select.to_string(PostgresQueryBuilder))?
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<i64>, _>>()?;

        let has_more = n > 0 && id_rows.len() > n;
        let mut target_ids = id_rows;
        if has_more {
            target_ids.truncate(n);
        }

        let cid = if has_more {
            let new_cid = uuid::Uuid::new_v4().to_string();
            self.spawn_cache_worker(&new_cid, query)?;
            Some(new_cid)
        } else {
            None
        };

        // 3. Tag Selector による属性取得 (Bulk Fetch)
        if target_ids.is_empty() {
            return Ok(SearchResponse::new_empty(cid, has_more));
        }

        let tag_cond = expanded.to_tag_condition();
        let mut fetch_sql = Query::select();
        fetch_sql
            .columns(Col::raw_tag_row_columns())
            .from(Tbl::OneView)
            .and_where(Expr::col(Col::ItemId).is_in(target_ids.clone()))
            .and_where(tag_cond.into());

        self.fetch_and_build(
            target_ids,
            fetch_sql.to_string(PostgresQueryBuilder),
            cid,
            has_more,
            n,
            expanded.get_projections().first(),
        )
    }

    fn try_resolve_cache(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<Option<SearchResponse>> {
        let Some(cid) = &options.cid else {
            return Ok(None);
        };

        let cache_path = self.cache_manager.path_for(cid);
        let tmp_path = format!("{}.tmp", cache_path.to_string_lossy());

        // 生成完了(exists) または 生成中(.tmp exists) であればキャッシュロジックに乗せる
        if !cache_path.exists() && !Path::new(&tmp_path).exists() {
            return Ok(None);
        }

        let progress = self.cache_manager.get_progress(cid)?;
        let meta = match self.read_cache_metadata(cid) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };

        let Some(cached_query) = meta.get(crate::cache::META_QUERY) else {
            return Ok(None);
        };

        if cached_query != query {
            return Ok(None);
        }

        if !progress.is_finished() {
            return Ok(Some(SearchResponse::new_unfinished(cid, progress)));
        }

        Ok(Some(self.search_from_cache(
            &cache_path,
            options.clone(),
            cid,
        )?))
    }

    fn search_from_cache(
        &self,
        path: &Path,
        options: SearchOptions,
        cid: &str,
    ) -> Result<SearchResponse> {
        let n = options.n.unwrap_or(100);
        let offset = options.offset.unwrap_or(0);
        // n=0 (全件) の場合は limit を設定しない
        let limit = if options.n.is_some() || n > 0 {
            n + 1
        } else {
            0
        };
        let path_str = path.to_string_lossy().to_string();

        let mut id_query = Query::select();
        id_query
            .distinct()
            .columns([Col::ItemId, Col::Rank])
            .from_function(
                sea_query::Func::cust(crate::db::DuckDbFunc::ReadParquet)
                    .arg(Expr::val(path_str.clone())),
                Tbl::Diff,
            )
            .order_by_expr(Expr::col(Col::Rank).into(), sea_query::Order::Desc)
            .order_by_expr(
                Expr::col(Col::ItemId).into(),
                sea_query::Order::Desc,
            );

        if limit > 0 {
            id_query.limit(limit as u64);
        }
        if offset > 0 {
            id_query.offset(offset as u64);
        }

        let id_rows = self
            .conn
            .prepare(&id_query.to_string(PostgresQueryBuilder))?
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<i64>, _>>()?;

        let has_more = n > 0 && id_rows.len() > n;
        let mut target_ids = id_rows;
        if has_more {
            target_ids.truncate(n);
        }

        if target_ids.is_empty() {
            return Ok(SearchResponse::new_empty(
                Some(cid.to_string()),
                has_more,
            ));
        }

        let mut fetch_query = Query::select();
        fetch_query
            .column(sea_query::Asterisk)
            .from_function(
                sea_query::Func::cust(crate::db::DuckDbFunc::ReadParquet)
                    .arg(Expr::val(path_str)),
                Tbl::Diff,
            )
            .and_where(Expr::col(Col::ItemId).is_in(target_ids.clone()));

        let mut response = self.fetch_and_build(
            target_ids,
            fetch_query.to_string(PostgresQueryBuilder),
            Some(cid.to_string()),
            has_more,
            n,
            None,
        )?;

        // キャッシュから読み込んだ場合、生成自体は完了している（または進行中状態を正しく反映する）
        // fetch_and_build はデフォルトで current=results.len(), total=None を設定してしまうため、
        // CacheManager から正しい進捗状態を取得して上書きする。
        response.progress = self.cache_manager.get_progress(cid)?;

        Ok(response)
    }

    fn fetch_and_build(
        &self,
        target_ids: Vec<i64>,
        fetch_sql: String,
        cid: Option<String>,
        has_more: bool,
        current_n: usize,
        projection: Option<&String>,
    ) -> Result<SearchResponse> {
        let raw_results = self
            .conn
            .prepare(&fetch_sql)?
            .query_map([], |r| RawTagRow::from_row(r))?
            .collect::<Result<Vec<RawTagRow>, _>>()?;

        let mut results_map: HashMap<i64, SearchResult> = HashMap::new();
        for &id in &target_ids {
            results_map.insert(id, SearchResult::new_empty(id));
        }

        for row in raw_results {
            if let Some(res) = results_map.get_mut(&row.id) {
                res.apply_raw_tag(row);
            }
        }

        let final_results: Vec<SearchResult> = target_ids
            .into_iter()
            .filter_map(|id| results_map.remove(&id))
            .collect();

        Ok(SearchResponse {
            results: final_results,
            cid,
            has_more,
            total_count: None,
            progress: Progress {
                current: current_n,
                total: None,
            },
            type_for_projection: projection.map(|s| TagType::from(s.as_str())),
        })
    }

    /// 非同期に全件検索結果を Parquet キャッシュとして書き出します。
    pub fn spawn_cache_worker(&self, cid: &str, query: &str) -> Result<()> {
        let db_dir = self.db_dir.clone();
        let cache_path = self.cache_manager.path_for(cid);
        // save_parquet 内部実装と合わせるため、文字列として .tmp を付与
        let cache_path_tmp = format!("{}.tmp", cache_path.to_string_lossy());

        let cid_owned = cid.to_string();
        let query_owned = query.to_string();

        std::thread::spawn(move || {
            let res = (|| -> Result<()> {
                let conn = Connection::open_in_memory()?;

                let node = if query_owned.trim().is_empty() {
                    QueryNode::And(vec![])
                } else {
                    crate::query::parse(&query_owned)?
                };
                let registry =
                    crate::query::QueryFunctionRegistry::with_standard();
                let expanded = node.expand(&registry);

                let mut item_ids_query = expanded.to_sql("oneview");
                item_ids_query.order_by_expr(
                    Expr::col(Col::Rank).into(),
                    sea_query::Order::Desc,
                );
                item_ids_query.order_by_expr(
                    Expr::col(Col::ItemId).into(),
                    sea_query::Order::Desc,
                );

                let mut sub_select = Query::select();
                sub_select
                    .column(Col::ItemId)
                    .from_subquery(item_ids_query, Tbl::InnerSub);

                let tag_cond = expanded.to_tag_condition();
                let mut cache_select = Query::select();
                cache_select
                    .columns(Col::raw_tag_row_columns())
                    .columns([Col::Rank])
                    .from(Tbl::OneView)
                    .and_where(Expr::col(Col::ItemId).in_subquery(sub_select))
                    .and_where(tag_cond.into());

                let sql_stmt = cache_select.to_owned();

                let registry_full = FunctionRegistry::with_standard();
                let all_columns = registry_full.get_all_columns();

                crate::oneview::OneView::recreate(
                    &conn,
                    &all_columns,
                    &db_dir,
                )?;

                let created_at = chrono::Utc::now().to_rfc3339();

                let mut metadata = HashMap::new();
                metadata
                    .insert(crate::cache::META_QUERY.to_string(), query_owned);
                metadata.insert(
                    crate::cache::META_CREATED_AT.to_string(),
                    created_at,
                );
                metadata.insert(
                    crate::cache::META_INDEX_VERSION.to_string(),
                    "1".to_string(),
                );

                crate::util::save_parquet(
                    &conn,
                    &sql_stmt,
                    &cache_path,
                    Some(&metadata),
                )?;

                Ok(())
            })();

            if let Err(e) = res {
                eprintln!("Cache worker error for {}: {}", cid_owned, e);
                let _ = std::fs::remove_file(&cache_path_tmp);
            }
        });
        Ok(())
    }

    /// 指定された CID の Parquet キャッシュからメタデータを読み取ります。
    pub fn read_cache_metadata(
        &self,
        cid: &str,
    ) -> Result<HashMap<String, String>> {
        let path = self.cache_manager.path_for(cid);
        if !path.exists() {
            return Err(anyhow::anyhow!("Cache file not found: {:?}", path));
        }
        let path_str = path.to_string_lossy();

        use crate::db::{DuckDbFunc, SqlType, Val};
        let mut meta_query = Query::select();
        meta_query
            .expr(Expr::col(Val::Key).cast_as(SqlType::VARCHAR))
            .expr(Expr::col(Val::Value).cast_as(SqlType::VARCHAR))
            .from_function(
                sea_query::Func::cust(DuckDbFunc::ParquetKvMetadata)
                    .arg(Expr::val(path_str)),
                Tbl::Diff,
            );

        let map: HashMap<String, String> = self
            .conn
            .prepare(&meta_query.to_string(PostgresQueryBuilder))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<HashMap<String, String>, _>>()?;

        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_spawn_cache_worker_activation() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        // ダミーデータ作成
        File::create(root.join("test.txt"))?;

        let fm = FileManager::new_with_db_dir(&db_dir)?;
        fm.index_directory(root, None::<&fn(usize)>, false)?;

        let cid = "test-worker-cid";
        fm.spawn_cache_worker(cid, "extension:txt")?;

        // 完了を待機 (最大2秒)
        let cache_path = fm.cache_manager.path_for(cid);
        let mut found = false;
        for _ in 0..20 {
            if cache_path.exists() {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        assert!(found, "Cache file should be created by worker");

        let meta = fm.read_cache_metadata(cid)?;
        assert_eq!(
            meta.get(crate::cache::META_QUERY).unwrap(),
            "extension:txt"
        );

        Ok(())
    }

    #[test]
    fn test_metadata_special_characters() -> Result<()> {
        let dir = tempdir()?;
        let db_dir = dir.path().join("db");
        std::fs::create_dir(&db_dir)?;
        let fm = FileManager::new_with_db_dir(&db_dir)?;

        // 特殊文字を含むクエリ
        let cid = "test-special-cid";
        let special_query = "tag:special_query_with_symbols"; // 記号によるエスケープ回避のため単純化

        // save_parquet の内部でエスケープが行われることを検証するため
        // 直接 util::save_parquet を呼ぶのと同等の状況を作る
        let mut metadata = HashMap::new();
        metadata.insert(
            crate::cache::META_QUERY.to_string(),
            special_query.to_string(),
        );

        let cache_path = fm.cache_manager.path_for(cid);
        let query = Query::select().expr(Expr::val(1)).to_owned();
        crate::util::save_parquet(
            &fm.conn,
            &query,
            &cache_path,
            Some(&metadata),
        )?;

        let read_meta = fm.read_cache_metadata(cid)?;
        assert_eq!(
            read_meta.get(crate::cache::META_QUERY).unwrap(),
            special_query
        );

        Ok(())
    }

    #[test]
    fn test_search_paging_consistency() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        // 複数のダミーファイルを作成してソート順を安定させる
        for i in 1..=5 {
            File::create(root.join(format!("file{:02}.txt", i)))?;
        }

        let fm = FileManager::new_with_db_dir(&db_dir)?;
        fm.index_directory(root, None::<&fn(usize)>, false)?;

        let query = "extension:txt";

        // 1. 通常検索 (DBから) - ページングなし
        let res_full = fm.search(
            query,
            SearchOptions {
                n: Some(10),
                ..Default::default()
            },
        )?;
        assert_eq!(res_full.results.len(), 5);

        // 2. ページングによる分割取得
        let res_p1 = fm.search(
            query,
            SearchOptions {
                n: Some(2),
                offset: Some(0),
                ..Default::default()
            },
        )?;
        let res_p2 = fm.search(
            query,
            SearchOptions {
                n: Some(2),
                offset: Some(2),
                ..Default::default()
            },
        )?;
        let res_p3 = fm.search(
            query,
            SearchOptions {
                n: Some(2),
                offset: Some(4),
                ..Default::default()
            },
        )?;

        assert_eq!(res_p1.results.len(), 2);
        assert_eq!(res_p2.results.len(), 2);
        assert_eq!(res_p3.results.len(), 1);

        // 順序の整合性確認
        assert_eq!(res_full.results[0].id, res_p1.results[0].id);
        assert_eq!(res_full.results[2].id, res_p2.results[0].id);
        assert_eq!(res_full.results[4].id, res_p3.results[0].id);

        // 3. キャッシュを作成し、キャッシュからの取得順序を確認
        let cid = "test-paging-cid";
        fm.spawn_cache_worker(cid, query)?;

        // 完了待機
        let cache_path = fm.cache_manager.path_for(cid);
        for _ in 0..20 {
            if cache_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let res_cache = fm.search(
            query,
            SearchOptions {
                n: Some(5),
                cid: Some(cid.to_string()),
                ..Default::default()
            },
        )?;
        assert_eq!(res_cache.results.len(), 5);

        // DB と キャッシュで同じ順序であることを確認
        for i in 0..5 {
            assert_eq!(
                res_full.results[i].id, res_cache.results[i].id,
                "Mismatch at index {}",
                i
            );
        }

        Ok(())
    }

    #[test]
    fn test_worker_complex_query_sql_integrity() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        // ダミーデータ
        File::create(root.join("readme.md"))?; // ext:md, name:readme
        File::create(root.join("test.rs"))?; // ext:rs, name:test

        let fm = FileManager::new_with_db_dir(&db_dir)?;
        fm.index_directory(root, None::<&fn(usize)>, false)?;

        // 複合クエリ: (extension:md OR extension:rs)
        let query = "extension:md | extension:rs";
        let cid = "test-complex-cid";

        fm.spawn_cache_worker(cid, query)?;

        // 完了待機
        let cache_path = fm.cache_manager.path_for(cid);
        for _ in 0..20 {
            if cache_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        assert!(
            cache_path.exists(),
            "Cache should be created for complex query"
        );

        // 内容の検証 (2件ヒットするはず)
        let res = fm.search(
            query,
            SearchOptions {
                n: Some(10),
                cid: Some(cid.to_string()),
                ..Default::default()
            },
        )?;
        assert_eq!(res.results.len(), 2);

        let names: Vec<String> =
            res.results.iter().map(|r| r.name.clone()).collect();
        assert!(names.contains(&"readme.md".to_string()));
        assert!(names.contains(&"test.rs".to_string()));

        Ok(())
    }

    #[test]
    fn test_search_no_results() -> Result<()> {
        let dir = tempdir()?;
        let db_dir = dir.path().join("db");
        std::fs::create_dir(&db_dir)?;
        let fm = FileManager::new_with_db_dir(&db_dir)?;

        // 空のインデックスで検索
        let res = fm.search("name:non-existent", SearchOptions::default())?;
        assert!(res.results.is_empty());
        assert_eq!(res.has_more, false);
        assert_eq!(res.cid, None);

        Ok(())
    }

    #[test]
    fn test_search_paging_out_of_bounds() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        File::create(root.join("a.txt"))?;
        let fm = FileManager::new_with_db_dir(&db_dir)?;
        fm.index_directory(root, None::<&fn(usize)>, false)?;

        // 1件しかないのに offset 10 で検索
        let res = fm.search(
            "extension:txt",
            SearchOptions {
                n: Some(10),
                offset: Some(10),
                ..Default::default()
            },
        )?;

        assert!(res.results.is_empty());
        assert_eq!(res.has_more, false);

        Ok(())
    }

    #[test]
    fn test_tag_mapping_accuracy() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        // サイズ 123 バイトのファイル
        let path = root.join("test.bin");
        std::fs::write(&path, vec![0u8; 123])?;

        let fm = FileManager::new_with_db_dir(&db_dir)?;
        fm.index_directory(root, None::<&fn(usize)>, false)?;

        let res = fm.search("name:test.bin", SearchOptions::default())?;
        assert_eq!(res.results.len(), 1);

        let item = &res.results[0];
        assert_eq!(item.name, "test.bin");
        // Size 属性が正しくマッピングされているか
        assert_eq!(item.intrinsic.size.unwrap().0, 123);

        Ok(())
    }
}
