mod confirm;
mod fs_operate;
mod lens_schema;
pub mod modify;
mod search_and_apply_captures;
pub(crate) mod sql;
mod tag_filter;
pub mod write;

use crate::db::Store;
use crate::tag::TagRegistry;
use anyhow::Result;

pub use crate::tag::{Edit, EditStrategy};

#[derive(Clone, Copy)]
pub enum QueryType {
    Tag,
    Untag,
}

#[derive(Default)]
pub struct WriteOptions {
    pub yes: bool,
}

pub struct EditResponse {
    pub updated: usize,
    pub deleted: usize,
    pub fs_ops: usize,
}

pub fn edit(
    store: &Store,
    registry: &TagRegistry,
    search_query: &str,
    edit_query: Option<&str>,
    query_type: QueryType,
    tag_condition: Option<&str>,
    options: WriteOptions,
) -> Result<EditResponse> {
    let item_edits = search_and_apply_captures::search_and_apply_captures(
        store,
        registry,
        search_query,
        edit_query,
    )?;
    let (fs_ops_list, actions) = plan(
        store,
        registry,
        item_edits.clone(),
        query_type,
        tag_condition,
    )?;
    if !confirm::confirm(&item_edits, &actions, &options)? {
        return Ok(EditResponse {
            updated: 0,
            deleted: 0,
            fs_ops: 0,
        });
    }
    let fs_count = fs_operate::fs_operate(fs_ops_list, registry)?;
    let resp = write::write_and_refresh(store, registry, actions)?;
    Ok(EditResponse {
        updated: resp.updated,
        deleted: resp.deleted,
        fs_ops: fs_count,
    })
}

fn plan(
    _store: &Store,
    registry: &TagRegistry,
    item_edits: Vec<(crate::response::Item, Option<String>)>,
    query_type: QueryType,
    tag_condition: Option<&str>,
) -> Result<(
    Vec<(crate::response::Item, String)>,
    Vec<write::WriteAction>,
)> {
    let condition = tag_condition
        .map(tag_filter::parse_tag_condition)
        .transpose()?;

    let mut actions = Vec::new();
    for (item, tag_query) in &item_edits {
        if let Some(ref node) = condition {
            // TODO: Store からエントリを取得し eval_tag_predicate で条件評価（別フェーズ）
            // TypedTag: 満たすエントリがなければ modify をスキップ
            // Projection: 満たす具体ラベルを取得して per-entry で modify
            if tag_filter::eval_tag_predicate(node, None)? {
                actions.extend(modify::modify(
                    item,
                    tag_query.as_deref(),
                    query_type,
                    registry,
                )?);
            }
        } else {
            actions.extend(modify::modify(
                item,
                tag_query.as_deref(),
                query_type,
                registry,
            )?);
        }
        // TODO: Relocate / SetFileAttr は fs_operate へ振り分け（別フェーズ）
    }
    Ok((vec![], actions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::TagRegistry;

    #[test]
    fn forbidden_tags_return_none() {
        let reg = TagRegistry::with_standard();
        assert!(reg.get("size").and_then(|f| f.edit()).is_none());
        assert!(reg.get("hash").and_then(|f| f.edit()).is_none());
        assert!(reg.get("is_dir").and_then(|f| f.edit()).is_none());
        assert!(reg.get("file_id").and_then(|f| f.edit()).is_none());
    }

    #[test]
    fn editable_tags_have_correct_strategies() {
        let reg = TagRegistry::with_standard();
        let strategy = |name: &str| {
            reg.get(name).and_then(|f| f.edit()).map(|e| e.strategy())
        };
        assert!(matches!(strategy("rank"), Some(EditStrategy::Replace)));
        assert!(matches!(strategy("name"), Some(EditStrategy::Replace)));
        assert!(matches!(strategy("filename"), Some(EditStrategy::Relocate)));
        assert!(matches!(strategy("path"), Some(EditStrategy::Relocate)));
        assert!(matches!(
            strategy("extension"),
            Some(EditStrategy::Relocate)
        ));
        assert!(matches!(
            strategy("parentdir"),
            Some(EditStrategy::Relocate)
        ));
        assert!(matches!(strategy("mtime"), Some(EditStrategy::SetFileAttr)));
        assert!(matches!(
            strategy("item_kind"),
            Some(EditStrategy::ModifyInjection)
        ));
        assert!(matches!(
            strategy("content"),
            Some(EditStrategy::ModifyInjection)
        ));
    }
}
