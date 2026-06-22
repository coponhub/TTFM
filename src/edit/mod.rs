mod lens_schema;
pub(crate) mod sql;
pub mod write;
pub mod modify;
mod search_and_apply_captures;
mod confirm;
mod fs_operate;
mod tag_filter;

use anyhow::Result;
use crate::cache::CacheManager;
use crate::db::Store;
use crate::tag::TagRegistry;
use crate::types::Label;

pub enum EditStrategy {
    Append,
    Replace,
    ModifyInjection,
    Relocate,
    SetFileAttr,
}

#[derive(Clone, Copy)]
pub enum QueryType {
    Tag,
    Untag,
}

pub trait Edit: Send + Sync {
    fn strategy(&self) -> EditStrategy;
    fn validate(&self, new: &Label) -> Result<Label> {
        Ok(new.clone())
    }
}

pub struct AppendEdit;
pub struct ReplaceEdit;
pub struct RelocateEdit;
pub struct SetFileAttrEdit;
pub struct ModifyInjectionEdit;

impl Edit for AppendEdit        { fn strategy(&self) -> EditStrategy { EditStrategy::Append } }
impl Edit for ReplaceEdit       { fn strategy(&self) -> EditStrategy { EditStrategy::Replace } }
impl Edit for RelocateEdit      { fn strategy(&self) -> EditStrategy { EditStrategy::Relocate } }
impl Edit for SetFileAttrEdit   { fn strategy(&self) -> EditStrategy { EditStrategy::SetFileAttr } }
impl Edit for ModifyInjectionEdit { fn strategy(&self) -> EditStrategy { EditStrategy::ModifyInjection } }

#[derive(Default)]
pub struct WriteOptions {
    pub yes: bool,
}

pub struct EditResponse {
    pub added: usize,
    pub deleted: usize,
    pub fs_ops: usize,
}

pub fn edit(
    store: &Store,
    registry: &TagRegistry,
    cache: &CacheManager,
    search_query: &str,
    edit_query: &str,
    query_type: QueryType,
    tag_condition: Option<&str>,
    options: WriteOptions,
) -> Result<EditResponse> {
    let item_edits =
        search_and_apply_captures::search_and_apply_captures(store, registry, cache, search_query, edit_query)?;
    let (fs_ops_list, actions) =
        plan(store, registry, item_edits.clone(), query_type, tag_condition)?;
    if !confirm::confirm(&item_edits, &actions, &options)? {
        return Ok(EditResponse { added: 0, deleted: 0, fs_ops: 0 });
    }
    let fs_count = fs_operate::fs_operate(fs_ops_list, registry)?;
    let resp = write::write_and_refresh(store, registry, actions)?;
    Ok(EditResponse { added: resp.added, deleted: resp.deleted, fs_ops: fs_count })
}

fn plan(
    _store: &Store,
    registry: &TagRegistry,
    item_edits: Vec<(crate::response::Item, String)>,
    query_type: QueryType,
    tag_condition: Option<&str>,
) -> Result<(Vec<(crate::response::Item, String)>, Vec<write::WriteAction>)> {
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
                actions.extend(modify::modify(item, tag_query, query_type, registry)?);
            }
        } else {
            actions.extend(modify::modify(item, tag_query, query_type, registry)?);
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
    fn edit_strategies_are_correct() {
        assert!(matches!(AppendEdit.strategy(), EditStrategy::Append));
        assert!(matches!(ReplaceEdit.strategy(), EditStrategy::Replace));
        assert!(matches!(RelocateEdit.strategy(), EditStrategy::Relocate));
    }

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
        let strategy = |name: &str| reg.get(name).and_then(|f| f.edit()).map(|e| e.strategy());
        assert!(matches!(strategy("rank"),      Some(EditStrategy::Replace)));
        assert!(matches!(strategy("name"),      Some(EditStrategy::Replace)));
        assert!(matches!(strategy("filename"),  Some(EditStrategy::Relocate)));
        assert!(matches!(strategy("path"),      Some(EditStrategy::Relocate)));
        assert!(matches!(strategy("extension"), Some(EditStrategy::Relocate)));
        assert!(matches!(strategy("parentdir"), Some(EditStrategy::Relocate)));
        assert!(matches!(strategy("mtime"),     Some(EditStrategy::SetFileAttr)));
        assert!(matches!(strategy("item_kind"), Some(EditStrategy::ModifyInjection)));
        assert!(matches!(strategy("content"),   Some(EditStrategy::ModifyInjection)));
    }
}
