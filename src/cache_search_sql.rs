use crate::db::{Col, Pronoun::*, SqlType, Tbl, VCol};
use crate::query::lens_schema::StorageMapping;
use crate::types::{Label, LabelValue, TagType};
use sea_query::{Expr, Query, SelectStatement};

/// 指定されたタグタイプについて、ユニークなラベル値と件数を取得します。
pub fn build_label_counts(
    proj_type: &TagType,
    storage: &StorageMapping,
    from_table: bool,
    path_str: Option<&str>,
    n: usize,
    offset: usize,
) -> SelectStatement {
    let mut q = Query::select();

    let (col_str, col_int, col_double, col_bool) = match storage {
        StorageMapping::Fixed(col) => match col.sql_type() {
            SqlType::BIGINT => (
                Expr::val(Option::<String>::None),
                Expr::col(*col),
                Expr::val(Option::<f64>::None),
                Expr::val(Option::<bool>::None),
            ),
            SqlType::BOOLEAN => (
                Expr::val(Option::<String>::None),
                Expr::val(Option::<i64>::None),
                Expr::val(Option::<f64>::None),
                Expr::col(*col),
            ),
            SqlType::DOUBLE => (
                Expr::val(Option::<String>::None),
                Expr::val(Option::<i64>::None),
                Expr::col(*col),
                Expr::val(Option::<bool>::None),
            ),
            _ => (
                Expr::col(*col),
                Expr::val(Option::<i64>::None),
                Expr::val(Option::<f64>::None),
                Expr::val(Option::<bool>::None),
            ),
        },
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

    if let StorageMapping::Basic { tag_type, .. } = storage {
        q.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
    }

    match storage {
        StorageMapping::Fixed(col) => {
            q.group_by_col(*col);
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

    let _ = proj_type;
    q
}

/// 特定ラベルを持つアイテムのIDを取得するクエリを生成します。
pub fn build_label_expansion_sql(
    proj_type: &TagType,
    label: &Label,
    storage: &StorageMapping,
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

    match storage {
        StorageMapping::Fixed(col) => {
            apply_label_filter(&mut q, *col, label);
        }
        StorageMapping::Basic { tag_type, column } => {
            q.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
            apply_label_filter(&mut q, *column, label);
        }
        StorageMapping::Composite => {
            q.and_where(Expr::val(1).eq(0));
        }
    }

    let _ = proj_type;
    q
}

fn apply_label_filter(q: &mut SelectStatement, col: Col, label: &Label) {
    match label.value() {
        LabelValue::Integer(i) => {
            q.and_where(Expr::col(col).eq(i));
        }
        LabelValue::Boolean(b) => {
            q.and_where(Expr::col(col).eq(b));
        }
        LabelValue::Double(bits) => {
            q.and_where(Expr::col(col).eq(f64::from_bits(bits)));
        }
        LabelValue::Null => {
            q.and_where(Expr::col(col).is_null());
        }
        LabelValue::String(s) | LabelValue::Literal(s) => {
            q.and_where(Expr::col(col).eq(s));
        }
    }
}
