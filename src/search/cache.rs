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

use crate::db::Pronoun::*;
use crate::db::Store;
use crate::response::SearchResponse;
use crate::search::SearchOptions;
use crate::tag::TagRegistry;
use crate::types::Progress;
use anyhow::Result;
use sea_query::{Expr, PostgresQueryBuilder, Query};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// キャッシュファイルのメタデータキー
pub const META_QUERY: &str = "ttfm.query";
pub const META_INDEX_VERSION: &str = "ttfm.index_version";
pub const META_CREATED_AT: &str = "ttfm.created_at";

/// 検索結果のキャッシュ（ResultCache）を管理する構造体。
pub struct CacheManager {
    cache_dir: PathBuf,
    max_size_bytes: i64,
}

impl CacheManager {
    pub fn new(cache_dir: PathBuf, max_size_bytes: i64) -> Self {
        if !cache_dir.exists() {
            let _ = std::fs::create_dir_all(&cache_dir);
        }
        Self {
            cache_dir,
            max_size_bytes,
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn is_generating(&self, cid: &str) -> bool {
        job_path_for(&self.cache_dir, cid).exists()
            || self.cache_dir.join(format!("{}.parquet.tmp", cid)).exists()
    }

    /// 指定された CID に対応するキャッシュファイルのパスを返します。
    pub fn path_for(&self, cid: &str) -> PathBuf {
        self.cache_dir.join(format!("{}.parquet", cid))
    }

    /// キャッシュディレクトリ内の古いファイルを削除し、合計サイズを制限以下に保ちます（LRU）。
    pub fn cleanup(&self) -> Result<()> {
        if !self.cache_dir.exists() {
            return Ok(());
        }
        self.cleanup_stale_temp_files()?;
        self.enforce_size_limit()?;
        Ok(())
    }

    fn cleanup_stale_temp_files(&self) -> Result<()> {
        for entry in std::fs::read_dir(&self.cache_dir)?.flatten() {
            if self.is_stale_temp_file(&entry) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    fn is_stale_temp_file(&self, entry: &std::fs::DirEntry) -> bool {
        let is_temp_ext = matches!(
            entry.path().extension().and_then(|s| s.to_str()),
            Some("job" | "tmp")
        );
        let is_expired = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d.as_secs() > 3600);
        is_temp_ext && is_expired
    }

    fn enforce_size_limit(&self) -> Result<()> {
        let mut files: Vec<_> = std::fs::read_dir(&self.cache_dir)?
            .flatten()
            .filter(|e| {
                e.path().extension().is_some_and(|ext| ext == "parquet")
            })
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let accessed = meta
                    .accessed()
                    .or_else(|_| meta.modified())
                    .unwrap_or(std::time::SystemTime::now());
                Some((e.path(), meta.len(), accessed))
            })
            .collect();

        files.sort_by_key(|f| f.2);
        let mut current_size: u64 = files.iter().map(|f| f.1).sum();
        for (path, size, _) in files {
            if current_size <= self.max_size_bytes as u64 {
                break;
            }
            let _ = std::fs::remove_file(path);
            current_size -= size;
        }
        Ok(())
    }

    /// キャッシュファイルの進捗状況（メタデータ）を確認します。
    pub fn get_progress(&self, cid: &str) -> Result<Progress> {
        let path = self.path_for(cid);
        let tmp_path = format!("{}.tmp", path.to_string_lossy());
        if Path::new(&tmp_path).exists() {
            return Ok(Progress {
                current: 0,
                total: None,
                is_done: false,
            });
        }

        if !path.exists() {
            return Ok(Progress::default());
        }

        Ok(Progress {
            current: 1,
            total: Some(1),
            is_done: true,
        })
    }

    /// 全てのキャッシュファイルを物理的に削除します。
    pub fn clear(&self) -> Result<()> {
        if self.cache_dir.exists() {
            std::fs::remove_dir_all(&self.cache_dir)?;
            std::fs::create_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }
}

pub fn job_path_for(cache_dir: &Path, cid: &str) -> PathBuf {
    cache_dir.join(format!("{}.job", cid))
}

struct TmpCleanupGuard {
    path: PathBuf,
    active: bool,
}

impl Drop for TmpCleanupGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn run_cache_worker(db_dir: PathBuf, cid: &str, query: &str) -> Result<()> {
    let cache = CacheManager::new(db_dir.join("cache"), 0);
    let cache_path = cache.path_for(cid);
    let cache_path_tmp = format!("{}.tmp", cache_path.to_string_lossy());
    let mut guard = TmpCleanupGuard {
        path: PathBuf::from(&cache_path_tmp),
        active: true,
    };
    let _ = std::fs::File::create(&guard.path);

    let conn = crate::db::open_connection()?;
    let mut registry = crate::tag::TagRegistry::with_standard();
    let config = crate::config::Config::load();
    if config.plugins.enabled {
        registry.load_from_dir(
            crate::get_ttfm_plugins_dir()?,
            &config.plugins.status,
        )?;
        registry.load_builtins(&config.plugins.status)?;
    }
    let store = Store {
        conn,
        db_dir: db_dir.clone(),
    };
    registry.load_type_configs(&store)?;
    let all_columns = registry.get_all_columns();
    let reader = crate::query::lens_reader::Reader::build(
        &registry,
        crate::db::Tbl::_OneView,
    );
    crate::oneview::OneView::recreate(
        &store.conn,
        &registry,
        &all_columns,
        reader,
        &db_dir,
    )?;
    let parsed = if query.trim().is_empty() {
        crate::query::ast::QueryNode::And(vec![])
    } else {
        crate::query::parser::parse_nowarn(query)?
    };
    let expanded =
        super::eval::expand_eval(parsed, &store, &registry, &mut Vec::new())?;
    let resolver = crate::query::lens_resolver::Resolver::from_node(
        expanded,
        &registry,
        &mut Vec::new(),
    )?;
    let fetcher = crate::query::fetcher::Fetcher::new(&resolver, &store.conn);
    let mut metadata = HashMap::new();
    metadata.insert(META_QUERY.to_string(), query.to_string());
    metadata
        .insert(META_CREATED_AT.to_string(), chrono::Utc::now().to_rfc3339());
    metadata.insert(META_INDEX_VERSION.to_string(), "1".to_string());
    fetcher.fetch_save_flat_table(&cache_path, Some(&metadata))?;
    guard.active = false;
    Ok(())
}

pub fn maybe_run_worker() -> Result<bool> {
    let (Ok(cid), Ok(token)) = (
        std::env::var("TTFM_CACHE_WORKER_CID"),
        std::env::var("TTFM_CACHE_WORKER_TOKEN"),
    ) else {
        return Ok(false);
    };
    let Ok(db_dir_str) = std::env::var("TTFM_CACHE_WORKER_DB_DIR") else {
        return Ok(false);
    };
    let db_dir = PathBuf::from(db_dir_str);
    let cache_dir = db_dir.join("cache");
    let job_file = job_path_for(&cache_dir, &cid);
    if !job_file.exists() {
        return Ok(false);
    }
    let content = match std::fs::read_to_string(&job_file) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    let mut lines = content.lines();
    let expected_token = match lines.next() {
        Some(t) => t.trim(),
        None => return Ok(false),
    };
    let expected_db_dir = match lines.next() {
        Some(d) => d.trim(),
        None => return Ok(false),
    };
    let query = lines.collect::<Vec<_>>().join("\n");
    if expected_token != token || PathBuf::from(expected_db_dir) != db_dir {
        return Ok(false);
    }
    let _ = std::fs::remove_file(&job_file);
    run_cache_worker(db_dir, &cid, &query)?;
    Ok(true)
}

pub fn clear_cache(db_dir: &Path) {
    let cache_dir = db_dir.join("cache");
    if cache_dir.exists() {
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::create_dir_all(&cache_dir);
    }
}

pub fn spawn_cache_worker(
    db_dir: PathBuf,
    cache: &CacheManager,
    cid: &str,
    query: &str,
) -> Result<()> {
    cache.cleanup()?;
    let exe_path = std::env::current_exe()?;
    if exe_path.file_stem().and_then(|s| s.to_str()) != Some("ttfm") {
        return Ok(());
    }
    let token = uuid::Uuid::new_v4().to_string();
    let job_file = job_path_for(&cache.cache_dir, cid);
    if job_file.exists() {
        return Ok(());
    }
    let content = format!("{}\n{}\n{}", token, db_dir.display(), query);
    std::fs::write(&job_file, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &job_file,
            std::fs::Permissions::from_mode(0o600),
        );
    }
    let mut cmd = std::process::Command::new(exe_path);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000008);
    }
    let res = cmd
        .env("TTFM_CACHE_WORKER_CID", cid)
        .env("TTFM_CACHE_WORKER_TOKEN", token)
        .env(
            "TTFM_CACHE_WORKER_DB_DIR",
            db_dir.to_string_lossy().to_string(),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&job_file);
        return Err(e.into());
    }
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

pub(crate) fn try_resolve_cache(
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
    if !cache_path.exists() {
        return Ok(None);
    }
    let meta = match read_cache_metadata(&store.conn, cache, cid) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    let Some(cached_query) = meta.get(META_QUERY) else {
        return Ok(None);
    };
    if cached_query != query {
        return Ok(None);
    }
    let n = options.n.unwrap_or(0);
    let offset = options.offset.unwrap_or(0);
    let parsed = if query.trim().is_empty() {
        crate::query::ast::QueryNode::And(vec![])
    } else {
        crate::query::parser::parse(query, sink)?
    };
    let expanded = super::eval::expand_eval(parsed, store, registry, sink)?;
    let resolver = crate::query::lens_resolver::Resolver::from_node(
        expanded, registry, sink,
    )?
    .with_order(&options.order);
    let fetcher = crate::query::fetcher::Fetcher::new(&resolver, &store.conn);
    let src = crate::db::Src::Parquet(cache_path.to_string_lossy().to_string());
    let mut results = match fetcher.fetch_from(&src, n, offset) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    super::apply_post_fetch_formatting(&mut results, &resolver, registry);
    let has_more = n > 0 && results.len() > n;
    if has_more {
        results.truncate(n);
    }
    let mut response = SearchResponse::from_results(
        results,
        Some(cid.to_string()),
        has_more,
        n,
        offset,
        query,
    );
    response.progress.is_done = true;
    Ok(Some(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::Indexer;
    use crate::search::search_nowarn;
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
    fn test_cache_path_for() {
        let dir = tempdir().unwrap();
        let cm = CacheManager::new(dir.path().to_path_buf(), 1024);
        let path = cm.path_for("test-cid");
        assert!(path.to_string_lossy().contains("test-cid.parquet"));
    }

    #[test]
    fn test_cache_cleanup_lru() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let cm = CacheManager::new(cache_dir.clone(), 100);

        let f1 = cache_dir.join("a.parquet");
        let f2 = cache_dir.join("b.parquet");
        std::fs::write(&f1, vec![0u8; 60]).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&f2, vec![0u8; 60]).unwrap();

        cm.cleanup().unwrap();

        assert!(!f1.exists(), "Oldest file should be removed");
        assert!(f2.exists(), "Newer file should be kept");
    }

    #[test]
    fn test_cache_get_progress() {
        let dir = tempdir().unwrap();
        let cm = CacheManager::new(dir.path().to_path_buf(), 1024);
        let cid = "test-progress";
        let path = cm.path_for(cid);
        let tmp_path = format!("{}.tmp", path.to_string_lossy());

        let prog = cm.get_progress(cid).unwrap();
        assert_eq!(prog.current, 0);
        assert_eq!(prog.total, None);

        std::fs::write(&tmp_path, "generating").unwrap();
        let prog = cm.get_progress(cid).unwrap();
        assert_eq!(prog.current, 0);
        assert_eq!(prog.total, None);

        std::fs::remove_file(&tmp_path).unwrap();
        std::fs::write(&path, "finished").unwrap();
        let prog = cm.get_progress(cid).unwrap();
        assert_eq!(prog.current, 1);
        assert_eq!(prog.total, Some(1));
    }

    #[test]
    fn test_cache_clear() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let cm = CacheManager::new(cache_dir.clone(), 1024);

        let f1 = cache_dir.join("a.parquet");
        std::fs::write(&f1, "data").unwrap();
        assert!(f1.exists());

        cm.clear().unwrap();
        assert!(!f1.exists(), "Cache should be cleared");
        assert!(cache_dir.exists(), "Cache directory should be recreated");
    }

    #[test]
    fn test_cache_cleanup_ignores_non_parquet() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let cm = CacheManager::new(cache_dir.clone(), 10);

        let f_parquet = cache_dir.join("a.parquet");
        let f_other = cache_dir.join("important.txt");

        std::fs::write(&f_parquet, vec![0u8; 20]).unwrap();
        std::fs::write(&f_other, vec![0u8; 20]).unwrap();

        cm.cleanup().unwrap();

        assert!(!f_parquet.exists(), "Parquet should be cleaned up");
        assert!(
            f_other.exists(),
            "Non-parquet file should be ignored by cleanup"
        );
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
        run_cache_worker(store.db_dir.clone(), cid, "extension:txt")?;

        let cache_path = cache.path_for(cid);
        assert!(
            cache_path.exists(),
            "Cache file should be created by worker"
        );

        let meta = read_cache_metadata(&store.conn, &cache, cid)?;
        assert_eq!(meta.get(META_QUERY).unwrap(), "extension:txt");

        Ok(())
    }

    #[test]
    fn test_maybe_run_worker_handshake() -> Result<()> {
        let dir = tempdir()?;
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(db_dir.join("cache"))?;
        File::create(dir.path().join("test.txt"))?;
        let (store, registry, cache) = setup(&db_dir)?;
        Indexer::new(&store, &registry).run_single(
            dir.path(),
            None::<&fn(usize)>,
            false,
        )?;
        let cid = "test-handshake-cid";
        let token = "test-secret-token";
        let job_file = job_path_for(&cache.cache_dir, cid);
        std::fs::write(
            &job_file,
            format!("{}\n{}\nextension:txt", token, db_dir.display()),
        )?;
        std::env::set_var("TTFM_CACHE_WORKER_CID", cid);
        std::env::set_var("TTFM_CACHE_WORKER_TOKEN", token);
        std::env::set_var(
            "TTFM_CACHE_WORKER_DB_DIR",
            db_dir.to_string_lossy().to_string(),
        );
        let ran = maybe_run_worker()?;
        assert!(ran);
        assert!(cache.path_for(cid).exists());
        assert!(!job_file.exists());
        std::env::remove_var("TTFM_CACHE_WORKER_CID");
        std::env::remove_var("TTFM_CACHE_WORKER_TOKEN");
        std::env::remove_var("TTFM_CACHE_WORKER_DB_DIR");
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
        metadata.insert(META_QUERY.to_string(), special_query.to_string());

        let cache_path = cache.path_for(cid);
        let query = Query::select().expr(Expr::val(1)).to_owned();
        crate::util::save_parquet(
            &store.conn,
            &query,
            &cache_path,
            Some(&metadata),
        )?;

        let read_meta = read_cache_metadata(&store.conn, &cache, cid)?;
        assert_eq!(read_meta.get(META_QUERY).unwrap(), special_query);

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

        let res1 = search_nowarn(
            &store,
            &registry,
            query,
            SearchOptions {
                n: Some(2),
                offset: Some(0),
                ..Default::default()
            },
        )?;
        assert_eq!(res1.results.len(), 2);
        assert!(res1.has_more);

        let cid = res1.cid.expect("CID should be generated");
        run_cache_worker(store.db_dir.clone(), &cid, query)?;

        let res2 = search_nowarn(
            &store,
            &registry,
            query,
            SearchOptions {
                n: Some(2),
                offset: Some(2),
                cid: Some(cid.clone()),
                ..Default::default()
            },
        )?;
        assert_eq!(res2.results.len(), 2);
        assert!(res2.has_more);

        let res3 = search_nowarn(
            &store,
            &registry,
            query,
            SearchOptions {
                n: Some(2),
                offset: Some(4),
                cid: Some(cid),
                ..Default::default()
            },
        )?;
        assert_eq!(res3.results.len(), 1);
        assert!(!res3.has_more);

        let all_names: Vec<String> = res1
            .results
            .iter()
            .chain(res2.results.iter())
            .chain(res3.results.iter())
            .map(|r| r.raw_repr())
            .collect();

        assert_eq!(all_names.len(), 5);
        for i in 1..=5 {
            assert!(all_names.contains(&format!("file{:02}.txt", i)));
        }

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
        run_cache_worker(store.db_dir.clone(), cid, query)?;

        let cache_path = cache.path_for(cid);
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

        run_cache_worker(store.db_dir.clone(), cid, query)?;

        let cache_path = cache.path_for(cid);
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
}
