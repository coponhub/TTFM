use super::{
    parse::EditQuery,
    write::{TagOp, WriteAction},
    WriteOptions,
};
use crate::response::Item;
use crate::types::{ItemId, Label, TagType};
use anyhow::{bail, Result};
use std::collections::HashMap;

fn group_replace_adds_by(
    actions: &[WriteAction],
) -> HashMap<(ItemId, TagType), Vec<Label>> {
    actions
        .iter()
        .filter_map(|a| match a {
            WriteAction::Add { item, tags } => Some((item, tags)),
            WriteAction::Delete { .. } => None,
        })
        .flat_map(|(item, tags)| {
            tags.iter().filter_map(move |op| match op {
                TagOp::Replace(t) => Some(((item.clone(), t.tag_type()), t.label.clone())),
                TagOp::Append(_) => None,
            })
        })
        .fold(HashMap::new(), |mut groups, (key, label)| {
            groups.entry(key).or_default().push(label);
            groups
        })
}

// Label は Hash を持たないため distinct count は線形探索で数える。
fn distinct_count(labels: &[Label]) -> usize {
    labels
        .iter()
        .fold(Vec::<&Label>::new(), |mut seen, l| {
            if !seen.contains(&l) {
                seen.push(l);
            }
            seen
        })
        .len()
}

pub fn confirm(
    _item_edits: &[(Item, Option<EditQuery>)],
    actions: &[WriteAction],
    _options: &WriteOptions,
) -> Result<bool> {
    for ((item, tag_type), labels) in group_replace_adds_by(actions) {
        if distinct_count(&labels) > 1 {
            bail!(
                "ambiguous Replace for tag type '{tag_type}' on item {item}: \
                 multiple distinct values from capture expansion"
            );
        }
    }
    Ok(true)
}
