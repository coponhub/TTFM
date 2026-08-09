use crate::types::{Bitical, SType};

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
