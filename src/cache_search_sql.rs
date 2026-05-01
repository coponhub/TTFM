use crate::db::{Col, Pronoun::*, Tbl, VCol};
use crate::types::{Label, SType, TagType};
use sea_query::{Expr, Query, SelectStatement};

/// 指定されたタグタイプについて、ユニークなラベル値と件数を取得します。
pub fn build_label_counts(
    proj_type: &TagType,
    from_table: bool,
    path_str: Option<&str>,
    n: usize,
    offset: usize,
) -> SelectStatement {
    let mut q = Query::select();

    // SType に応じて、どのカラムを LabelStr/Int 等にマッピングするかを決定
    let (col_str, col_int, col_double, col_bool) = match proj_type {
        TagType::Base(SType::TypedTag) => (
            Expr::col(Col::TypedTag),
            Expr::val(Option::<i64>::None),
            Expr::val(Option::<f64>::None),
            Expr::val(Option::<bool>::None),
        ),
        TagType::Base(SType::Origin) => (
            Expr::col(Col::Origin),
            Expr::val(Option::<i64>::None),
            Expr::val(Option::<f64>::None),
            Expr::val(Option::<bool>::None),
        ),
        TagType::Base(SType::Rank) => (
            Expr::val(Option::<String>::None),
            Expr::col(Col::Rank),
            Expr::val(Option::<f64>::None),
            Expr::val(Option::<bool>::None),
        ),
        _ => (
            Expr::col(Col::LabelStr),
            Expr::col(Col::LabelInt),
            Expr::col(Col::LabelDouble),
            Expr::col(Col::LabelBool),
        ),
    };

    q.expr_as(col_str, Col::LabelStr)
        .expr_as(col_int, Col::LabelInt)
        .expr_as(col_double, Col::LabelDouble)
        .expr_as(col_bool, Col::LabelBool)
        .expr_as(Expr::cust("COUNT(*)"), VCol::Total);

    if from_table {
        q.from(Tbl::OneView)
            .and_where(Expr::col(Col::ItemId).in_subquery(
                Query::select().column(Col::ItemId).from(Sub).to_owned(),
            ));
    } else if let Some(path) = path_str {
        q.from_function(
            sea_query::Func::cust(crate::db::DuckDbFunc::ReadParquet)
                .arg(Expr::val(path)),
            Diff,
        );
    }

    match proj_type {
        TagType::Base(SType::TypedTag)
        | TagType::Base(SType::Origin)
        | TagType::Base(SType::Rank)
        | TagType::Base(SType::Label) => {}
        _ => {
            q.and_where(Expr::col(Col::Type).eq(proj_type.as_str()));
        }
    }

    match proj_type {
        TagType::Base(SType::TypedTag) => {
            q.group_by_col(Col::TypedTag);
        }
        TagType::Base(SType::Origin) => {
            q.group_by_col(Col::Origin);
        }
        TagType::Base(SType::Rank) => {
            q.group_by_col(Col::Rank);
        }
        _ => {
            q.group_by_columns([
                Col::LabelStr,
                Col::LabelInt,
                Col::LabelDouble,
                Col::LabelBool,
            ]);
        }
    }

    q.order_by(Col::LabelStr, sea_query::Order::Asc);

    if n > 0 {
        q.limit((n + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }

    q
}

/// 特定ラベルを持つアイテムのIDを取得するクエリを生成します。
pub fn build_label_expansion_sql(
    proj_type: &TagType,
    label: &Label,
    from_table: bool,
    path_str: Option<&str>,
) -> SelectStatement {
    let mut q = Query::select();
    q.distinct().column(Col::ItemId);

    if from_table {
        q.from(Tbl::OneView)
            .and_where(Expr::col(Col::ItemId).in_subquery(
                Query::select().column(Col::ItemId).from(Sub).to_owned(),
            ));
    } else if let Some(path) = path_str {
        q.from_function(
            sea_query::Func::cust(crate::db::DuckDbFunc::ReadParquet)
                .arg(Expr::val(path)),
            Diff,
        );
    }

    match proj_type {
        TagType::Base(SType::TypedTag) => {
            q.and_where(Expr::col(Col::TypedTag).eq(label.as_str()));
        }
        TagType::Base(SType::Origin) => {
            q.and_where(Expr::col(Col::Origin).eq(label.as_str()));
        }
        TagType::Base(SType::Rank) => match label.value() {
            crate::types::LabelValue::Integer(i) => {
                q.and_where(Expr::col(Col::Rank).eq(i));
            }
            _ => {
                q.and_where(Expr::val(1).eq(0));
            }
        },
        TagType::Base(SType::Label) => match label.value() {
            crate::types::LabelValue::String(s)
            | crate::types::LabelValue::Literal(s) => {
                q.and_where(Expr::col(Col::LabelStr).eq(s));
            }
            crate::types::LabelValue::Integer(i) => {
                q.and_where(Expr::col(Col::LabelInt).eq(i));
            }
            crate::types::LabelValue::Boolean(b) => {
                q.and_where(Expr::col(Col::LabelBool).eq(b));
            }
            crate::types::LabelValue::Double(bits) => {
                q.and_where(
                    Expr::col(Col::LabelDouble).eq(f64::from_bits(bits)),
                );
            }
            crate::types::LabelValue::Null => {
                q.and_where(Expr::col(Col::LabelStr).is_null());
            }
        },
        _ => {
            q.and_where(Expr::col(Col::Type).eq(proj_type.as_str()));
            match label.value() {
                crate::types::LabelValue::String(s)
                | crate::types::LabelValue::Literal(s) => {
                    q.and_where(Expr::col(Col::LabelStr).eq(s));
                }
                crate::types::LabelValue::Integer(i) => {
                    q.and_where(Expr::col(Col::LabelInt).eq(i));
                }
                crate::types::LabelValue::Boolean(b) => {
                    q.and_where(Expr::col(Col::LabelBool).eq(b));
                }
                crate::types::LabelValue::Double(bits) => {
                    q.and_where(
                        Expr::col(Col::LabelDouble).eq(f64::from_bits(bits)),
                    );
                }
                crate::types::LabelValue::Null => {
                    q.and_where(Expr::col(Col::LabelStr).is_null());
                }
            }
        }
    }

    q
}
