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

use crate::cache::CacheManager;
use crate::db::Pronoun::*;
use crate::db::{Store, TargetTable};
use crate::response::SearchResponse;
use crate::tag::TagRegistry;
use crate::types::Progress;
use anyhow::Result;
use sea_query::{Expr, PostgresQueryBuilder, Query};
use std::collections::HashMap;
use std::path::Path;

/// 検索オプションを制御する構造体。
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// 取得件数 (None または 0 は全件)
    pub n: Option<usize>,
    /// 開始位置 (None は自動または0)
    pub offset: Option<usize>,
    /// 利用するキャッシュ ID
    pub cid: Option<String>,
    /// キャッシュを使うか (既定 true。ただし n=None/0 の全件検索では無効)
    pub cache: bool,
    /// 明示的な並び順（複数キー可）。空なら resolve 済みクエリからの判定、
    /// それも無ければ既定（rank 降順）にフォールバックする。
    pub order: Vec<crate::types::Order>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            n: None,
            offset: None,
            cid: None,
            cache: true,
            order: Vec::new(),
        }
    }
}

/// キャッシュ上限サイズ (3 GiB)。
const CACHE_MAX_BYTES: i64 = 3 * 1024 * 1024 * 1024;

/// クエリ文字列を使用してインデックスを検索します。
pub fn search(
    store: &Store,
    registry: &TagRegistry,
    query: &str,
    options: SearchOptions,
    sink: &mut dyn crate::query::error::WarningSink,
) -> Result<SearchResponse> {
    if !store.path_for_target(TargetTable::FileReferences).exists() {
        return Err(anyhow::anyhow!(
            "Index not found. Please run 'index' command first."
        ));
    }

    let n = options.n.unwrap_or(0);
    let offset = options.offset.unwrap_or(0);

    // キャッシュ無効パス。cache:false、または「全件(n=0)かつ読み込む cid も無い」場合。
    // cid があれば n に関わらずキャッシュ読みを試みる（下の有効パスへ）。
    if !options.cache || (n == 0 && options.cid.is_none()) {
        let (results, has_more) = search_core(
            store,
            registry,
            query,
            n,
            offset,
            &options.order,
            sink,
        )?;
        return Ok(SearchResponse::from_results(
            results, None, has_more, n, offset, query,
        ));
    }

    // ここから先はキャッシュ有効が確定。CacheManager は必ず生成する。
    let cache = CacheManager::new(store.db_dir.join("cache"), CACHE_MAX_BYTES);

    if let Some(res) =
        try_resolve_cache(store, registry, &cache, query, &options, sink)?
    {
        return Ok(res);
    }

    let (results, has_more) =
        search_core(store, registry, query, n, offset, &options.order, sink)?;

    let cid = if has_more {
        let new_cid = uuid::Uuid::new_v4().to_string();
        spawn_cache_worker(store.db_dir.clone(), &cache, &new_cid, query)?;
        Some(new_cid)
    } else {
        None
    };

    Ok(SearchResponse::from_results(
        results, cid, has_more, n, offset, query,
    ))
}

/// クエリ文字列を使用してインデックスを検索します。警告は破棄します。
pub fn search_nowarn(
    store: &Store,
    registry: &TagRegistry,
    query: &str,
    options: SearchOptions,
) -> Result<SearchResponse> {
    let mut discard: Vec<crate::query::error::Warning> = Vec::new();
    search(store, registry, query, options, &mut discard)
}

/// 検索のコア処理。resolver 生成・fetch・スカラー表示整形・ページングを行う。
/// キャッシュには一切依存しない。
fn search_core(
    store: &Store,
    registry: &TagRegistry,
    query: &str,
    n: usize,
    offset: usize,
    order: &[crate::types::Order],
    sink: &mut dyn crate::query::error::WarningSink,
) -> Result<(Vec<crate::response::Item>, bool)> {
    let resolver =
        crate::query::lens_resolver::Resolver::new(query, registry, sink)?
            .with_order(order);
    let fetcher = crate::query::fetcher::Fetcher::new(&resolver, &store.conn);

    let mut results = fetcher.fetch(n, offset)?;

    if let Some(tt) = resolver.get_scalar_result_label_type() {
        use crate::types::{Bitical, Origin, SType, TypedTag};
        for result in &mut results {
            let raw = result
                .tags
                .entries
                .iter()
                .find(|e| e.typed_tag.tag_type().as_str() == "value")
                .and_then(|e| match e.typed_tag.value() {
                    Bitical::Integer(i) => Some(i.to_string()),
                    Bitical::Double(d) => Some((d as i64).to_string()),
                    _ => None,
                });
            if let Some(raw) = raw {
                let formatted = registry.format_display(tt.as_str(), &raw);
                result.representative =
                    vec![TypedTag::new(SType::Name, formatted.clone())].into();
                result.tags.push(
                    TypedTag::new(SType::Name, formatted),
                    Origin::Builtin,
                );
            }
        }
    }

    let nvalue_tag_type = resolver.get_scalar_result_label_type();
    for result in &mut results {
        if let Some(raw) = result
            .tags
            .entries
            .iter()
            .find(|e| e.typed_tag.tag_type().as_str() == "nvalue")
            .map(|e| e.typed_tag.as_str())
        {
            let display = nvalue_tag_type
                .as_ref()
                .map(|tt| registry.format_display(tt.as_str(), &raw))
                .unwrap_or(raw);
            result.representative.nvalue =
                Some(crate::types::Label::from(display));
        }
    }

    let has_more = n > 0 && results.len() > n;
    if has_more {
        results.truncate(n);
    }

    Ok((results, has_more))
}

/// 非同期に全件検索結果を Parquet キャッシュとして書き出します。
pub fn spawn_cache_worker(
    db_dir: std::path::PathBuf,
    cache: &CacheManager,
    cid: &str,
    query: &str,
) -> Result<()> {
    let cache_path = cache.path_for(cid);
    let cache_path_tmp = format!("{}.tmp", cache_path.to_string_lossy());

    let cid_owned = cid.to_string();
    let query_owned = query.to_string();

    std::thread::spawn(move || {
        let res = (|| -> Result<()> {
            let conn = crate::db::open_connection()?;

            let std_registry = crate::tag::TagRegistry::with_standard();
            let resolver = crate::query::lens_resolver::Resolver::new_nowarn(
                &query_owned,
                &std_registry,
            )?;
            let fetcher = crate::query::fetcher::Fetcher::new(&resolver, &conn);

            let registry = crate::tag::TagRegistry::with_standard();
            let all_columns = registry.get_all_columns();
            let reader = crate::query::lens_reader::Reader::build(
                &registry,
                crate::db::Tbl::_OneView,
            );
            crate::oneview::OneView::recreate(
                &conn,
                &registry,
                &all_columns,
                reader,
                &db_dir,
            )?;

            let created_at = chrono::Utc::now().to_rfc3339();

            let mut metadata = HashMap::new();
            metadata.insert(crate::cache::META_QUERY.to_string(), query_owned);
            metadata
                .insert(crate::cache::META_CREATED_AT.to_string(), created_at);
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
    conn: &duckdb::Connection,
    cache: &CacheManager,
    cid: &str,
) -> Result<HashMap<String, String>> {
    let path = cache.path_for(cid);
    if !path.exists() {
        return Err(anyhow::anyhow!("Cache file not found: {:?}", path));
    }
    let path_str = path.to_string_lossy();

    use crate::db::{BiticalType, DuckDbFunc, Val};
    let mut meta_query = Query::select();
    meta_query
        .expr(Expr::col(Val::Key).cast_as(BiticalType::String))
        .expr(Expr::col(Val::Value).cast_as(BiticalType::String))
        .from_function(
            sea_query::Func::cust(DuckDbFunc::ParquetKvMetadata)
                .arg(Expr::val(path_str)),
            Diff,
        );

    let map: HashMap<String, String> = conn
        .prepare(&meta_query.to_string(PostgresQueryBuilder))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<HashMap<String, String>, _>>()?;

    Ok(map)
}

fn try_resolve_cache(
    store: &Store,
    registry: &TagRegistry,
    cache: &CacheManager,
    query: &str,
    options: &SearchOptions,
    sink: &mut dyn crate::query::error::WarningSink,
) -> Result<Option<SearchResponse>> {
    let Some(cid) = &options.cid else {
        return Ok(None);
    };

    let cache_path = cache.path_for(cid);
    let tmp_path = format!("{}.tmp", cache_path.to_string_lossy());

    if !cache_path.exists() && !Path::new(&tmp_path).exists() {
        return Ok(None);
    }

    let progress = cache.get_progress(cid)?;
    let meta = match read_cache_metadata(&store.conn, cache, cid) {
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
        return Ok(Some(SearchResponse::new_unfinished(cid, progress, query)));
    }

    Ok(Some(search_from_cache(
        store,
        registry,
        cache,
        &cache_path,
        options.clone(),
        cid,
        sink,
    )?))
}

fn search_from_cache(
    store: &Store,
    registry: &TagRegistry,
    cache: &CacheManager,
    path: &Path,
    options: SearchOptions,
    cid: &str,
    sink: &mut dyn crate::query::error::WarningSink,
) -> Result<SearchResponse> {
    let n = options.n.unwrap_or(0);
    let offset = options.offset.unwrap_or(0);
    let path_str = path.to_string_lossy().to_string();

    let meta = read_cache_metadata(&store.conn, cache, cid)?;
    let query = meta
        .get(crate::cache::META_QUERY)
        .ok_or_else(|| anyhow::anyhow!("Query not found in cache"))?;

    let resolver =
        crate::query::lens_resolver::Resolver::new(query, registry, sink)?
            .with_order(&options.order);
    let fetcher = crate::query::fetcher::Fetcher::new(&resolver, &store.conn);
    let src = crate::db::Src::Parquet(path_str);

    let mut all_results = fetcher.fetch_from(&src, n, offset)?;
    let has_more = n > 0 && all_results.len() > n;
    if has_more {
        all_results.truncate(n);
    }

    let current_n = all_results.len();
    let mut response = if all_results.is_empty() {
        SearchResponse::new_empty(
            Some(cid.to_string()),
            has_more,
            query.as_str(),
        )
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
            query: query.to_string(),
        }
    };
    response.progress = cache.get_progress(cid)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::Indexer;
    use crate::tag::TagRegistry;
    use std::fs::File;
    use tempfile::tempdir;

    fn setup(
        db_dir: &std::path::Path,
    ) -> Result<(Store, TagRegistry, CacheManager)> {
        let store = Store::open(db_dir)?;
        let registry = TagRegistry::with_standard();
        Indexer::new(&store, &registry).initialize_tables()?;
        let cache = CacheManager::new(store.db_dir.join("cache"), 0);
        Ok((store, registry, cache))
    }

    #[test]
    fn test_spawn_cache_worker_activation() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        File::create(root.join("test.txt"))?;

        let (store, registry, cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            root,
            None::<&fn(usize)>,
            false,
        )?;

        let cid = "test-worker-cid";
        spawn_cache_worker(store.db_dir.clone(), &cache, cid, "extension:txt")?;

        let cache_path = cache.path_for(cid);
        let mut found = false;
        for _ in 0..20 {
            if cache_path.exists() {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        assert!(found, "Cache file should be created by worker");

        let meta = read_cache_metadata(&store.conn, &cache, cid)?;
        assert_eq!(
            meta.get(crate::cache::META_QUERY).unwrap(),
            "extension:txt"
        );

        Ok(())
    }

    #[test]
    fn test_search_forwards_warnings_to_caller_sink() -> Result<()> {
        let dir = tempdir()?;
        let db_dir = dir.path().join("db");
        std::fs::create_dir(&db_dir)?;
        let (store, registry, _cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            dir.path(),
            None::<&fn(usize)>,
            false,
        )?;

        let mut warnings: Vec<crate::query::error::Warning> = Vec::new();
        search(
            &store,
            &registry,
            "width:>height:",
            SearchOptions {
                n: Some(10),
                ..Default::default()
            },
            &mut warnings,
        )?;

        assert!(
            warnings.iter().any(|w| w.0.contains("width: :> height:")),
            "expected sink to receive the warning, got: {:?}",
            warnings.iter().map(|w| &w.0).collect::<Vec<_>>()
        );

        Ok(())
    }

    #[test]
    fn test_metadata_special_characters() -> Result<()> {
        let dir = tempdir()?;
        let db_dir = dir.path().join("db");
        std::fs::create_dir(&db_dir)?;
        let (store, _registry, cache) = setup(&db_dir)?;

        let cid = "test-special-cid";
        let special_query = "tag:special_query_with_symbols";

        let mut metadata = HashMap::new();
        metadata.insert(
            crate::cache::META_QUERY.to_string(),
            special_query.to_string(),
        );

        let cache_path = cache.path_for(cid);
        let query = Query::select().expr(Expr::val(1)).to_owned();
        crate::util::save_parquet(
            &store.conn,
            &query,
            &cache_path,
            Some(&metadata),
        )?;

        let read_meta = read_cache_metadata(&store.conn, &cache, cid)?;
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

        for i in 1..=5 {
            File::create(root.join(format!("file{:02}.txt", i)))?;
        }

        let (store, registry, _cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            root,
            None::<&fn(usize)>,
            false,
        )?;

        let query = "extension:txt";

        let res_full = search_nowarn(
            &store,
            &registry,
            query,
            SearchOptions {
                n: Some(10),
                ..Default::default()
            },
        )?;
        assert_eq!(res_full.results.len(), 5);

        let res_p1 = search_nowarn(
            &store,
            &registry,
            query,
            SearchOptions {
                n: Some(2),
                offset: Some(0),
                ..Default::default()
            },
        )?;
        let res_p2 = search_nowarn(
            &store,
            &registry,
            query,
            SearchOptions {
                n: Some(2),
                offset: Some(2),
                ..Default::default()
            },
        )?;
        let res_p3 = search_nowarn(
            &store,
            &registry,
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

        assert_eq!(res_full.results[0].id, res_p1.results[0].id);
        assert_eq!(res_full.results[2].id, res_p2.results[0].id);
        assert_eq!(res_full.results[4].id, res_p3.results[0].id);

        Ok(())
    }

    #[test]
    fn test_default_options_returns_all() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        for i in 1..=150 {
            File::create(root.join(format!("file{:03}.txt", i)))?;
        }

        let (store, registry, _cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            root,
            None::<&fn(usize)>,
            false,
        )?;

        let res = search_nowarn(
            &store,
            &registry,
            "extension:txt",
            SearchOptions::default(),
        )?;
        assert_eq!(res.results.len(), 150);
        assert!(!res.has_more);

        Ok(())
    }

    #[test]
    fn test_cache_false_disables_worker() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        for i in 1..=5 {
            File::create(root.join(format!("file{:02}.txt", i)))?;
        }

        let (store, registry, _cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            root,
            None::<&fn(usize)>,
            false,
        )?;

        let res = search_nowarn(
            &store,
            &registry,
            "extension:txt",
            SearchOptions {
                n: Some(2),
                cache: false,
                ..Default::default()
            },
        )?;

        assert!(
            res.has_more,
            "finite n over more results should set has_more"
        );
        assert!(res.cid.is_none(), "cache=false must not issue a cid");

        Ok(())
    }

    #[test]
    fn test_search_paging_out_of_bounds() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        File::create(root.join("a.txt"))?;
        let (store, registry, _cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            root,
            None::<&fn(usize)>,
            false,
        )?;

        let res = search_nowarn(
            &store,
            &registry,
            "extension:txt",
            SearchOptions {
                n: Some(10),
                offset: Some(10),
                ..Default::default()
            },
        )?;

        assert!(res.results.is_empty());
        assert!(!res.has_more);

        Ok(())
    }

    #[test]
    fn test_tag_mapping_accuracy() -> Result<()> {
        use crate::types::ItemKind;

        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        let path = root.join("test.bin");
        std::fs::write(&path, vec![0u8; 123])?;

        let (store, registry, _cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            root,
            None::<&fn(usize)>,
            false,
        )?;

        let res = search_nowarn(
            &store,
            &registry,
            "name:test.bin",
            SearchOptions::default(),
        )?;
        assert_eq!(res.results.len(), 1);
        let r = &res.results[0];
        assert_eq!(r.item_kind, ItemKind::File);

        Ok(())
    }

    #[test]
    fn test_projection_cache_consistency() -> Result<()> {
        use crate::types::ItemKind;

        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        File::create(root.join("main.rs"))?;
        File::create(root.join("another.rs"))?;
        File::create(root.join("test.txt"))?;

        let (store, registry, cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            root,
            None::<&fn(usize)>,
            false,
        )?;

        let query = "extension:";

        let res_db =
            search_nowarn(&store, &registry, query, SearchOptions::default())?;
        assert!(!res_db.results.is_empty());
        assert!(res_db.results.iter().any(|r| r
            .tags
            .entries
            .iter()
            .any(|e| e.typed_tag.tag_type()
                == crate::types::TagType::from("item"))));
        assert!(res_db
            .results
            .iter()
            .all(|r| r.item_kind == ItemKind::Volatile));

        let cid = "test-proj-cache-cid";
        spawn_cache_worker(store.db_dir.clone(), &cache, cid, query)?;

        let cache_path = cache.path_for(cid);
        for _ in 0..20 {
            if cache_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(cache_path.exists());

        let res_cache = search_nowarn(
            &store,
            &registry,
            query,
            SearchOptions {
                cid: Some(cid.to_string()),
                ..Default::default()
            },
        )?;

        let db_has_item_tag = res_db.results.iter().any(|r| {
            r.tags.entries.iter().any(|e| {
                e.typed_tag.tag_type() == crate::types::TagType::from("item")
            })
        });
        let cache_has_item_tag = res_cache.results.iter().any(|r| {
            r.tags.entries.iter().any(|e| {
                e.typed_tag.tag_type() == crate::types::TagType::from("item")
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
                db_item.raw_repr(),
                cache_item.raw_repr(),
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

    #[test]
    fn test_worker_complex_query_sql_integrity() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        File::create(root.join("readme.md"))?;
        File::create(root.join("test.rs"))?;

        let (store, registry, cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            root,
            None::<&fn(usize)>,
            false,
        )?;

        let query = "extension:md | extension:rs";
        let cid = "test-complex-cid";

        spawn_cache_worker(store.db_dir.clone(), &cache, cid, query)?;

        let cache_path = cache.path_for(cid);
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

        let res = search_nowarn(
            &store,
            &registry,
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
        let (store, registry, _cache) = setup(&db_dir)?;

        let res = search_nowarn(
            &store,
            &registry,
            "name:non-existent",
            SearchOptions::default(),
        )?;
        assert!(res.results.is_empty());
        assert!(!res.has_more);
        assert_eq!(res.cid, None);

        Ok(())
    }
}
