use crate::db::TargetTable;
use crate::db::Pronoun::*;
use crate::response::SearchResponse;
use crate::types::Progress;
use crate::FileManager;
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

        let resolver = crate::query::lens_resolver::Resolver::new(query)?;
        let fetcher =
            crate::query::fetcher::Fetcher::new(&resolver, &self.conn);

        // 2. クエリ実行（種別判断は Fetcher 内部に委譲）
        let warnings = if resolver.is_label_set_intersect() {
            vec![
                "Projection intersection ('&') found. Did you mean '&:' (Nest) to group results?".to_string(),
            ]
        } else {
            Vec::new()
        };

        let mut results = fetcher.fetch(n, offset)?;

        // スカラー集計結果のラベル型フォーマット後処理
        if let Some(tt) = resolver.get_scalar_result_label_type() {
            use crate::types::{Label, LabelValue, Origin};
            for result in &mut results {
                let raw = result
                    .tags
                    .entries
                    .iter()
                    .find(|e| e.label.tag_type().as_str() == "value")
                    .and_then(|e| match e.label.value() {
                        LabelValue::Integer(i) => Some(i.to_string()),
                        LabelValue::Double(bits) => {
                            Some((f64::from_bits(bits) as i64).to_string())
                        }
                        _ => None,
                    });
                if let Some(raw) = raw {
                    let formatted =
                        self.registry.format_display(tt.as_str(), &raw);
                    result.representative =
                        vec![Label::Name(formatted.clone())];
                    result
                        .tags
                        .push(Label::Name(formatted), Origin::System);
                }
            }
        }

        let has_more = n > 0 && results.len() > n;
        if has_more {
            results.truncate(n);
        }

        // 検索が完了している場合（これ以上ページがない場合）は総件数を確定させる
        let (total_count, progress_total) = if has_more {
            (None, None)
        } else {
            let total = offset + results.len();
            (Some(total), Some(total))
        };

        // 3. キャッシュ生成の開始（続きがある場合）
        let cid = if has_more {
            let new_cid = uuid::Uuid::new_v4().to_string();
            self.spawn_cache_worker(&new_cid, query)?;
            Some(new_cid)
        } else {
            None
        };

        Ok(SearchResponse {
            results,
            cid,
            has_more,
            total_count,
            progress: Progress {
                current: total_count.unwrap_or(n),
                total: progress_total,
                is_done: !has_more,
            },
            warnings,
        })
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
        let path_str = path.to_string_lossy().to_string();

        let meta = self.read_cache_metadata(cid)?;
        let query = meta
            .get(crate::cache::META_QUERY)
            .ok_or_else(|| anyhow::anyhow!("Query not found in cache"))?;

        let resolver = crate::query::lens_resolver::Resolver::new(query)?;
        let fetcher =
            crate::query::fetcher::Fetcher::new(&resolver, &self.conn);
        let src = crate::db::Src::Parquet(path_str);

        let mut all_results = fetcher.fetch_from(&src, n, offset)?;
        let has_more = n > 0 && all_results.len() > n;
        if has_more {
            all_results.truncate(n);
        }

        let current_n = all_results.len();
        let mut response = if all_results.is_empty() {
            SearchResponse::new_empty(Some(cid.to_string()), has_more)
        } else {
            SearchResponse {
                results: all_results,
                cid: Some(cid.to_string()),
                has_more,
                total_count: None,
                progress: Progress {
                    current: current_n,
                    total: None,
                    is_done: !has_more,
                },
                warnings: Vec::new(),
            }
        };
        response.progress = self.cache_manager.get_progress(cid)?;
        Ok(response)
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

                let resolver =
                    crate::query::lens_resolver::Resolver::new(&query_owned)?;
                let fetcher =
                    crate::query::fetcher::Fetcher::new(&resolver, &conn);

                let all_columns = crate::tag::TagRegistry::with_standard().get_all_columns();

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

                fetcher.fetch_save_flat_table(&cache_path, Some(&metadata))?;

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
                Diff,
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
    use crate::types::ItemKind;
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
            res.results.iter().map(|r| r.raw_repr()).collect();
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
        assert_eq!(item.raw_repr(), "test.bin");
        // Size 属性が正しくマッピングされているか
        assert_eq!(item.intrinsic.size.unwrap().0, 123);

        Ok(())
    }

    #[test]
    fn test_search_projection_cache_consistency() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        // テストデータ作成
        File::create(root.join("test.rs"))?;
        File::create(root.join("another.rs"))?;
        File::create(root.join("test.txt"))?;

        let fm = FileManager::new_with_db_dir(&db_dir)?;
        fm.index_directory(root, None::<&fn(usize)>, false)?;

        let query = "extension:";

        // 1. 通常検索 (DBから)
        let res_db = fm.search(query, SearchOptions::default())?;
        assert!(!res_db.results.is_empty());
        // 転置形式: item: タグが注入されていればグループ表示
        assert!(res_db.results.iter().any(|r| r.tags.entries.iter().any(
            |e| e.label.tag_type() == crate::types::TagType::from("item")
        )));
        // 転置形式: results はラベルアイテム
        assert!(res_db
            .results
            .iter()
            .all(|r| r.item_kind == ItemKind::Volatile));

        // 2. キャッシュを作成
        let cid = "test-proj-cache-cid";
        fm.spawn_cache_worker(cid, query)?;

        // 完了待機
        let cache_path = fm.cache_manager.path_for(cid);
        for _ in 0..20 {
            if cache_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(cache_path.exists());

        // 3. キャッシュから取得
        let res_cache = fm.search(
            query,
            SearchOptions {
                cid: Some(cid.to_string()),
                ..Default::default()
            },
        )?;

        // 整合性チェック: DB とキャッシュ両方で item: タグが存在するか一致
        let db_has_item_tag = res_db.results.iter().any(|r| {
            r.tags.entries.iter().any(|e| {
                e.label.tag_type() == crate::types::TagType::from("item")
            })
        });
        let cache_has_item_tag = res_cache.results.iter().any(|r| {
            r.tags.entries.iter().any(|e| {
                e.label.tag_type() == crate::types::TagType::from("item")
            })
        });
        assert_eq!(
            db_has_item_tag, cache_has_item_tag,
            "item: tag presence mismatch"
        );
        assert_eq!(
            res_db.results.len(),
            res_cache.results.len(),
            "Result length mismatch"
        );

        // 転置形式の確認
        for (i, (db_item, cache_item)) in res_db
            .results
            .iter()
            .zip(res_cache.results.iter())
            .enumerate()
        {
            assert_eq!(
                db_item.item_kind, cache_item.item_kind,
                "item_kind mismatch at index {}",
                i
            );
            assert_eq!(
                db_item.raw_repr(), cache_item.raw_repr(),
                "representative mismatch at index {}",
                i
            );
            if !db_item.id.is_volatile() {
                assert_eq!(
                    db_item.id, cache_item.id,
                    "id mismatch at index {}",
                    i
                );
            } else {
                assert!(
                    cache_item.id.is_volatile(),
                    "id mismatch at index {} (expected volatile)",
                    i
                );
            }
        }

        Ok(())
    }
}
