use crate::db::{Col, CustomFunc, QueryResultCol, Tbl};
use crate::query::ast::ComparisonOp;
use crate::query::lens_resolver::LabelSetOpKind;
use crate::query::lens_schema::to_bin_op;
use crate::types::ItemKind;
use sea_query::{Alias, Expr, ExprTrait, Query, SelectStatement, SimpleExpr};
use super::wrap_in_subquery;

pub(super) fn build_direct_boolean_select(
    left: SimpleExpr,
    op: ComparisonOp,
    right: SimpleExpr,
    _view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    let bin_op = to_bin_op(op);
    let comparison = Expr::expr(left).binary(bin_op, right);
    q.expr_as(
        Expr::case(comparison.clone(), Expr::val(1i64))
            .case(comparison.is_null(), Expr::cust("NULL"))
            .finally(Expr::val(0i64)),
        Col::ItemId,
    )
    .expr_as(
        Expr::val(<&'static str>::from(ItemKind::Volatile)),
        Col::ItemKind,
    )
    .expr_as(
        Expr::val(<&'static str>::from(ItemKind::Volatile)),
        Col::Type,
    )
    .expr_as(Expr::val(0i64), Col::Rank)
    .expr_as(Expr::cust("[]"), QueryResultCol::Tags);
    q
}

pub(super) fn wrap_boolean_collider(sql: SelectStatement) -> SelectStatement {
    let mut q = Query::select();
    q.expr_as(
        Expr::case(
            CustomFunc::any_value(Expr::col((Alias::new("pk"), Col::ItemId))).is_not_null(),
            Expr::val(1i64),
        )
        .finally(Expr::val(0i64)),
        Col::ItemId,
    )
    .expr_as(
        Expr::val(<&'static str>::from(ItemKind::Volatile)),
        Col::ItemKind,
    )
    .expr_as(
        Expr::val(<&'static str>::from(ItemKind::Volatile)),
        Col::Type,
    )
    .expr_as(Expr::val(0i64), Col::Rank)
    .expr_as(Expr::cust("[]"), QueryResultCol::Tags)
    .from_subquery(sql, Alias::new("pk"));
    q
}

pub(super) fn reduce_with_union(
    child_sqls: Vec<SelectStatement>,
    union_type: sea_query::UnionType,
    empty_fallback: SelectStatement,
) -> SelectStatement {
    child_sqls
        .into_iter()
        .map(wrap_in_subquery)
        .reduce(|mut acc, next| {
            acc.union(union_type, next);
            acc
        })
        .unwrap_or(empty_fallback)
}

pub(super) fn build_resolved_and_sql(
    child_sqls: Vec<SelectStatement>,
    view: &str,
) -> SelectStatement {
    let fallback = Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view))
        .to_owned();
    reduce_with_union(child_sqls, sea_query::UnionType::Intersect, fallback)
}

pub(super) fn build_resolved_or_sql(
    child_sqls: Vec<SelectStatement>,
    view: &str,
) -> SelectStatement {
    let fallback = Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from(Alias::new(view))
        .and_where(Expr::val(1).eq(0))
        .to_owned();
    reduce_with_union(child_sqls, sea_query::UnionType::Distinct, fallback)
}

pub(super) fn build_resolved_diff_sql(
    l: SelectStatement,
    r: SelectStatement,
) -> SelectStatement {
    let mut q = wrap_in_subquery(l);
    q.union(sea_query::UnionType::Except, wrap_in_subquery(r));
    q
}

pub(super) fn build_resolved_comp_sql(
    is_boolean: bool,
    c_sql: SelectStatement,
    view: &str,
) -> SelectStatement {
    let mut q = if is_boolean {
        Query::select()
            .expr_as(Expr::val(1i64), Col::ItemId)
            .expr_as(Expr::val(0i64), Col::Rank)
            .expr_as(
                Expr::val(<&'static str>::from(ItemKind::Volatile)),
                Col::ItemKind,
            )
            .to_owned()
    } else {
        Query::select()
            .columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .distinct()
            .from(Alias::new(view))
            .and_where(Expr::col(Col::ItemKind).is_not_in(vec!["type", "tag"]))
            .to_owned()
    };
    let mut eq = Query::select();
    eq.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from_subquery(c_sql, Tbl::NotSide);
    q.union(sea_query::UnionType::Except, eq);
    q
}

pub(super) fn build_label_set_op_pick_sql(
    op: &LabelSetOpKind,
    child_sqls: Vec<SelectStatement>,
) -> SelectStatement {
    match op {
        LabelSetOpKind::Union => reduce_with_union(
            child_sqls,
            sea_query::UnionType::Distinct,
            Query::select().to_owned(),
        ),
        LabelSetOpKind::Intersect => reduce_with_union(
            child_sqls,
            sea_query::UnionType::Intersect,
            Query::select().to_owned(),
        ),
        LabelSetOpKind::Except => {
            let mut it = child_sqls.into_iter();
            if let (Some(l), Some(r)) = (it.next(), it.next()) {
                build_resolved_diff_sql(l, r)
            } else {
                Query::select().to_owned()
            }
        }
    }
}
