use crate::cache::CacheManager;
use crate::db::Store;
use crate::response::Item;
use crate::search::SearchOptions;
use crate::tag::TagRegistry;
use anyhow::Result;

pub fn search_and_apply_captures(
    store: &Store,
    registry: &TagRegistry,
    cache: &CacheManager,
    search_query: &str,
    edit_query: &str,
) -> Result<Vec<(Item, String)>> {
    let resp = crate::search::search(store, registry, cache, search_query, SearchOptions::default())?;
    resp.results
        .into_iter()
        .map(|item| apply_captures(&item, edit_query).map(|q| (item, q)))
        .collect()
}

fn apply_captures(_item: &Item, edit_query: &str) -> Result<String> {
    // TODO: §8 {n} 展開（別フェーズ）
    Ok(edit_query.to_string())
}
