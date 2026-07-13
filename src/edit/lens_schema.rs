use crate::types::{Bitical, SType};
use sea_query::Expr;

pub(crate) use crate::db::{ItemRefRow, UserTagsRow};

impl Bitical {
    /// 書込境界: 保存先の EAV 4 カラムへ分解します。`None` は全カラム `None`。
    /// 書込先カラムの決定と保存形への収束（Uuid→文字列）は `to_col_value` に
    /// 集約されており、ここでは型付きタプルへの振り分けのみ行う。
    pub fn to_eav_columns(
        v: Option<Bitical>,
    ) -> (Option<String>, Option<i64>, Option<f64>, Option<bool>) {
        let Some((col, value)) = v.map(|b| b.to_col_value()) else {
            return (None, None, None, None);
        };
        match (col, value) {
            (SType::LabelStr, Bitical::String(s)) => {
                (Some(s), None, None, None)
            }
            (SType::LabelInt, Bitical::Integer(i)) => {
                (None, Some(i), None, None)
            }
            (SType::LabelDouble, Bitical::Double(d)) => {
                (None, None, Some(d), None)
            }
            (SType::LabelBool, Bitical::Boolean(b)) => {
                (None, None, None, Some(b))
            }
            _ => (None, None, None, None),
        }
    }
}

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
    value: Bitical,
) -> UserTagsRow {
    let (ls, li, ld, lb) = Bitical::to_eav_columns(Some(value));
    UserTagsRow {
        item_id: Expr::val(item_id).into(),
        tag_type: Expr::val(tag_type).into(),
        label_str: Expr::val(ls).into(),
        label_int: Expr::val(li).into(),
        label_dbl: Expr::val(ld).into(),
        label_bool: Expr::val(lb).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitical_to_eav_columns() {
        assert_eq!(
            Bitical::to_eav_columns(Some(Bitical::String("a".to_string()))),
            (Some("a".to_string()), None, None, None)
        );
        assert_eq!(
            Bitical::to_eav_columns(Some(Bitical::Integer(1))),
            (None, Some(1), None, None)
        );
        assert_eq!(
            Bitical::to_eav_columns(Some(Bitical::Double(1.5))),
            (None, None, Some(1.5), None)
        );
        assert_eq!(
            Bitical::to_eav_columns(Some(Bitical::Boolean(true))),
            (None, None, None, Some(true))
        );
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            Bitical::to_eav_columns(Some(Bitical::Uuid(id))),
            (Some(id.to_string()), None, None, None)
        );
        assert_eq!(Bitical::to_eav_columns(None), (None, None, None, None));
    }
}
