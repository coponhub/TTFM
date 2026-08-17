mod confirm;
pub(crate) mod fs_operate;
pub(crate) mod glob_capture;
mod lens_schema;
pub mod modify;
pub mod parse;
mod search_and_apply_captures;
pub(crate) mod sql;
mod tag_filter;
pub mod write;

use crate::db::Store;
use crate::query::error::WarningSink;
use crate::tag::TagRegistry;
use crate::types::TagType;
use anyhow::Result;

pub use crate::tag::{Edit, EditStrategy};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    Tag,
    Untag,
}

#[derive(Default)]
pub struct WriteOptions {
    pub yes: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct EditResponse {
    pub updated: usize,
    pub deleted: usize,
    pub fs_ops: usize,
    pub has_skipped: bool,
}

fn static_tag_types(parsed: Option<&parse::EditQuery>) -> Vec<TagType> {
    parsed
        .map(|q| q.nodes.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter(|n| !n.tag_type.has_braced())
        .map(|n| TagType::from(n.tag_type.value().as_str()))
        .collect()
}

pub fn edit(
    store: &Store,
    registry: &TagRegistry,
    search_query: &str,
    edit_query: Option<&str>,
    query_type: QueryType,
    tag_condition: Option<&str>,
    options: WriteOptions,
    sink: &mut dyn WarningSink,
) -> Result<EditResponse> {
    let parsed = edit_query
        .map(|q| parse::parse_edit_query(q, query_type, registry))
        .transpose()?;
    fs_operate::check_location_tag(
        &static_tag_types(parsed.as_ref()),
        registry,
    )?;
    let item_edits = search_and_apply_captures::search_and_apply_captures(
        store,
        registry,
        search_query,
        parsed.as_ref(),
        sink,
    )?;
    let (mut fs_plan, actions) = plan(
        store,
        registry,
        item_edits.clone(),
        query_type,
        tag_condition,
    )?;
    let has_skipped = fs_plan.warn_unsupported(sink);
    if !confirm::confirm(&item_edits, &actions, &fs_plan, &options)? {
        return Ok(EditResponse {
            updated: 0,
            deleted: 0,
            fs_ops: 0,
            has_skipped: false,
        });
    }
    let outcome = fs_operate::apply(store, registry, fs_plan)?;
    let resp = write::write_and_refresh(store, registry, actions)?;
    Ok(EditResponse {
        updated: resp.updated,
        deleted: resp.deleted,
        fs_ops: outcome.count(),
        has_skipped,
    })
}

fn plan(
    _store: &Store,
    registry: &TagRegistry,
    item_edits: Vec<(crate::response::Item, Option<parse::EditQuery>)>,
    query_type: QueryType,
    tag_condition: Option<&str>,
) -> Result<(fs_operate::FsPlan, Vec<write::WriteAction>)> {
    let condition = tag_condition
        .map(tag_filter::parse_tag_condition)
        .transpose()?;

    let mut fs_inputs = Vec::new();
    let mut actions = Vec::new();
    for (item, tag_query) in item_edits {
        let all_nodes =
            modify::resolve_nodes(tag_query.as_ref(), query_type, registry)?;
        let mut fs_nodes = Vec::new();
        let mut tag_nodes = Vec::new();
        for n in all_nodes {
            if fs_operate::is_fs_strategy(n.strategy) {
                fs_nodes.push(n);
            } else {
                tag_nodes.push(n);
            }
        }
        if !fs_nodes.is_empty() {
            fs_inputs.push((item.clone(), fs_nodes));
        }
        if let Some(ref node) = condition {
            if tag_filter::eval_tag_predicate(node, None)? {
                actions.extend(modify::modify(
                    &item, &tag_nodes, query_type, registry,
                )?);
            }
        } else {
            actions.extend(modify::modify(
                &item, &tag_nodes, query_type, registry,
            )?);
        }
    }
    let fs_plan = fs_operate::plan_fs(registry, fs_inputs, query_type)?;
    Ok((fs_plan, actions))
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
