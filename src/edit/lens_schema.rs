use crate::types::LabelValue;
use sea_query::Expr;

pub(crate) use crate::db::{ItemRefRow, UserTagsRow};

pub(crate) fn item_ref_row(
    item_id: i64,
    item_kind: String,
    content: String,
) -> ItemRefRow {
    ItemRefRow {
        item_id: Expr::val(item_id).into(),
        item_kind: Expr::val(item_kind).into(),
        content: Expr::val(content).into(),
        ..Default::default()
    }
}

pub(crate) fn user_tags_row(
    item_id: i64,
    tag_type: String,
    value: LabelValue,
) -> UserTagsRow {
    let (ls, li, ld, lb) = label_value_to_eav_columns(value);
    UserTagsRow {
        item_id: Expr::val(item_id).into(),
        tag_type: Expr::val(tag_type).into(),
        label_str: Expr::val(ls).into(),
        label_int: Expr::val(li).into(),
        label_dbl: Expr::val(ld).into(),
        label_bool: Expr::val(lb).into(),
    }
}

pub fn label_value_to_eav_columns(
    v: LabelValue,
) -> (Option<String>, Option<i64>, Option<f64>, Option<bool>) {
    match v {
        LabelValue::String(s) | LabelValue::Literal(s) => {
            (Some(s), None, None, None)
        }
        LabelValue::Integer(i) => (None, Some(i), None, None),
        LabelValue::Double(bits) => {
            (None, None, Some(f64::from_bits(bits)), None)
        }
        LabelValue::Boolean(b) => (None, None, None, Some(b)),
        LabelValue::Null | LabelValue::Date(_) => (None, None, None, None),
    }
}
