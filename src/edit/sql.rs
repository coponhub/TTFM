use super::lens_schema;
use crate::db::{Col, Tbl};
use crate::types::{Label, LabelValue};
use crate::util::parquet_query;
use sea_query::{
    CaseStatement, Expr, Order, Query, SelectStatement, SimpleExpr, UnionType,
    UpdateStatement,
};

pub(crate) struct UserTagDelete {
    pub(crate) item_id: i64,
    pub(crate) tag_type: String,
    pub(crate) value: Option<LabelValue>,
}

pub(crate) fn item_references_write(
    path: &str,
    inserts: Vec<(i64, String, String)>,
    cascade_ids: &[i64],
) -> SelectStatement {
    let mut q = parquet_query(path);
    if !cascade_ids.is_empty() {
        q.and_where(Expr::col(Col::ItemId).is_not_in(cascade_ids.to_vec()));
    }
    for (item_id, item_kind, content) in inserts {
        q.union(
            UnionType::All,
            lens_schema::item_ref_row(item_id, item_kind, content).select(),
        );
    }
    q.order_by(Col::ItemId, Order::Asc);
    q
}

pub(crate) fn user_tags_write(
    path: &str,
    inserts: Vec<(i64, Label)>,
    deletes: Vec<UserTagDelete>,
    cascade_ids: &[i64],
) -> SelectStatement {
    let mut q = parquet_query(path);

    if !cascade_ids.is_empty() {
        q.and_where(Expr::col(Col::ItemId).is_not_in(cascade_ids.to_vec()));
    }

    for d in &deletes {
        let mut row_match = Expr::col(Col::ItemId)
            .eq(d.item_id)
            .and(Expr::col(Col::Type).eq(d.tag_type.clone()));
        if let Some(ref v) = d.value {
            if let Some((col, val_expr)) = Col::for_label_value(v) {
                row_match = row_match.and(Expr::col(col).eq(val_expr));
            }
        }
        q.and_where(row_match.not());
    }

    for (item_id, label) in inserts {
        let tag_type = label.tag_type().to_string();
        q.union(
            UnionType::All,
            lens_schema::user_tags_row(item_id, tag_type, label.value())
                .select(),
        );
    }

    q.order_by(Col::Type, Order::Asc)
        .order_by(Col::LabelInt, Order::Asc)
        .order_by(Col::LabelStr, Order::Asc)
        .order_by(Col::ItemId, Order::Asc);
    q
}

pub(crate) fn rank_case_update(
    tmp: Tbl,
    updates: &[(i64, i64)],
) -> UpdateStatement {
    let rank_case: SimpleExpr = updates
        .iter()
        .fold(CaseStatement::new(), |acc, (id, rank)| {
            acc.case(Expr::col(Col::ItemId).eq(*id), *rank)
        })
        .finally(Expr::col(Col::Rank))
        .into();

    Query::update()
        .table(tmp)
        .value(Col::Rank, rank_case)
        .and_where(
            Expr::col(Col::ItemId).is_in(
                updates
                    .iter()
                    .map(|(id, _)| sea_query::Value::from(*id))
                    .collect::<Vec<_>>(),
            ),
        )
        .to_owned()
}
