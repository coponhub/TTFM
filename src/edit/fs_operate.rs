use crate::response::Item;
use crate::tag::TagRegistry;
use anyhow::Result;

pub fn fs_operate(
    fs_ops: Vec<(Item, String)>,
    _registry: &TagRegistry,
) -> Result<usize> {
    if !fs_ops.is_empty() {
        anyhow::bail!("fs_operate: Relocate / SetFileAttr は別フェーズ");
    }
    Ok(0)
}
