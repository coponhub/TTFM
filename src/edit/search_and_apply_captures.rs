use crate::db::Store;
use crate::query::error::WarningSink;
use crate::response::Item;
use crate::search::SearchOptions;
use crate::tag::TagRegistry;
use anyhow::Result;

pub fn search_and_apply_captures(
    store: &Store,
    registry: &TagRegistry,
    search_query: &str,
    edit_query: Option<&str>,
    sink: &mut dyn WarningSink,
) -> Result<Vec<(Item, Option<String>)>> {
    let mut resp = crate::search::search(
        store,
        registry,
        search_query,
        SearchOptions::default(),
        sink,
    )?;
    resp.query_into_tags();
    resp.results
        .into_iter()
        .map(|item| apply_captures(&item, edit_query).map(|q| (item, q)))
        .collect()
}

fn apply_captures(
    _item: &Item,
    edit_query: Option<&str>,
) -> Result<Option<String>> {
    // TODO: §8 {n} 展開（別フェーズ）
    Ok(edit_query.map(|q| q.to_string()))
}
