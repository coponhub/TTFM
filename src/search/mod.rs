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

pub mod cache;
pub mod eval;

pub use cache::*;
pub use eval::*;

use crate::db::{Store, TargetTable};
use crate::response::SearchResponse;
use crate::tag::TagRegistry;
use anyhow::Result;

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

    if let Some(res) = cache::try_resolve_cache(
        store, registry, &cache, query, &options, sink,
    )? {
        return Ok(res);
    }

    let (results, has_more) =
        search_core(store, registry, query, n, offset, &options.order, sink)?;

    let is_generating = options
        .cid
        .as_ref()
        .map(|c| cache.is_generating(c))
        .unwrap_or(false);

    let cid = if is_generating {
        options.cid.clone()
    } else if has_more {
        let new_cid = uuid::Uuid::new_v4().to_string();
        cache::spawn_cache_worker(
            store.db_dir.clone(),
            &cache,
            &new_cid,
            query,
        )?;
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

pub(crate) fn apply_post_fetch_formatting(
    results: &mut [crate::response::Item],
    resolver: &crate::query::lens_resolver::Resolver,
    registry: &TagRegistry,
) {
    if let Some(tt) = resolver.get_scalar_result_label_type() {
        use crate::types::{Bitical, Origin, SType, TypedTag};
        for result in results.iter_mut() {
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
    for result in results.iter_mut() {
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
}

/// 検索のコア処理。resolver 生成・fetch・スカラー表示整形・ページングを行う。
/// キャッシュには一切依存しない。
pub(crate) fn search_core(
    store: &Store,
    registry: &TagRegistry,
    query: &str,
    n: usize,
    offset: usize,
    order: &[crate::types::Order],
    sink: &mut dyn crate::query::error::WarningSink,
) -> Result<(Vec<crate::response::Item>, bool)> {
    if query.trim().is_empty() {
        return Err(anyhow::anyhow!("Empty search query is not allowed"));
    }
    let parsed = crate::query::parser::parse(query, sink)?;
    let expanded = eval::expand_eval(parsed, store, registry, sink)?;
    let resolver = crate::query::lens_resolver::Resolver::from_node(
        expanded, registry, sink,
    )?
    .with_order(order);
    let fetcher = crate::query::fetcher::Fetcher::new(&resolver, &store.conn);

    let mut results = fetcher.fetch(n, offset)?;
    apply_post_fetch_formatting(&mut results, &resolver, registry);

    let has_more = n > 0 && results.len() > n;
    if has_more {
        results.truncate(n);
    }

    Ok((results, has_more))
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
    fn test_default_options_returns_all() -> Result<()> {
        let dir = tempdir()?;
        let root = dir.path();
        let db_dir = root.join("db");
        std::fs::create_dir(&db_dir)?;

        for i in 1..=3 {
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
            SearchOptions::default(),
        )?;

        assert_eq!(res.results.len(), 3);
        assert!(!res.has_more);
        assert!(
            res.cid.is_none(),
            "All-items query with default options must NOT trigger worker or issue a cid"
        );

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
