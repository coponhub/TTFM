use super::{write::WriteAction, WriteOptions};
use crate::response::Item;
use anyhow::Result;

pub fn confirm(
    _item_edits: &[(Item, Option<String>)],
    _actions: &[WriteAction],
    _options: &WriteOptions,
) -> Result<bool> {
    // stub: 常に確認OK（実装は別フェーズ）
    Ok(true)
}
