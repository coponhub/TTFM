mod lens_schema;
pub(crate) mod sql;
pub mod write;
pub mod modify;

use anyhow::Result;
use crate::types::Label;

pub enum EditStrategy {
    Append,
    Replace,
    ModifyInjection,
    Relocate,
    SetFileAttr,
}

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
