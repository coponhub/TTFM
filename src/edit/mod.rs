mod lens_schema;
pub(crate) mod sql;
pub mod write;

pub enum EditStrategy {
    Append,
    Replace,
    ModifyInjection,
    Relocate,
    SetFileAttr,
}
