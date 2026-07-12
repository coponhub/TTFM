// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use super::agg_pieces::{
    build_agg, build_agg_nest, build_agg_operand_subquery,
    build_agg_operand_subquery_nest,
};
use super::{
    build_aggregation_context_for_operand, build_nest_context,
    build_nest_context_for_operand, build_tag_value_agg_expr,
    label_to_unit_aware_expr, needs_nest_context,
};
use crate::db::{
    BiticalType, Col, CustomFunc, Pronoun::*, QueryResultCol, Src,
};
use crate::query::ast::ComparisonOp;
use crate::query::lens_resolver::ResolvedOperand;
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{Label, SType};
use sea_query::{Expr, Query, SelectStatement, SimpleExpr};

pub(super) fn build_resolved_match_sql(
    src: &Src,
    storage: &StorageMapping,
    bitical_type: BiticalType,
    op: ComparisonOp,
    label: &Label,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(src);
    q.cond_where(storage.to_condition(op, label, bitical_type));
    q
}

pub(super) fn build_column_match_sql(
    src: &Src,
    tag: SType,
    label: &Label,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(src);
    // `Label::Literal`（quoted）は完全一致検索、それ以外の String は GLOB検索。
    // `.value()` 経由だと Literal 性が失われる（Bitical に Literal 変種が無い）ため、
    // まず `label` 自体で判定する。
    if let Label::Literal(_, s) = label {
        let t = if matches!(tag, SType::Label) {
            Col::LabelStr.into()
        } else {
            tag
        };
        q.and_where(Expr::col(t).eq(s.as_str()));
        return q;
    }
    q.and_where(label.value().to_column_match_expr(tag));
    q
}

pub(super) fn build_resolved_tag_tag_match_sql(
    src: &Src,
    left_storage: &StorageMapping,
    left_sql_type: BiticalType,
    op: ComparisonOp,
    right_storage: &StorageMapping,
    right_sql_type: BiticalType,
) -> SelectStatement {
    let mut q = Query::select();
    q.column(Col::ItemId).from(src).group_by_col(Col::ItemId);
    let left_expr = build_tag_value_agg_expr(left_storage, left_sql_type);
    let right_expr = build_tag_value_agg_expr(right_storage, right_sql_type);
    q.and_having(left_expr.binary(to_bin_op(op), right_expr));
    q
}

pub(super) fn build_scalar_match_sql(
    src: &Src,
    left: &Label,
    op: ComparisonOp,
    right: &Label,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(src);
    stmt.column(Col::ItemId);
    let cond = Expr::expr(label_to_unit_aware_expr(left))
        .binary(to_bin_op(op), label_to_unit_aware_expr(right));
    stmt.cond_where(cond);
    stmt.limit(1);
    stmt
}

pub(super) fn build_resolved_scalar_sql(
    src: &Src,
    op: &ResolvedOperand,
) -> SelectStatement {
    let agg_ctx = build_aggregation_context_for_operand(src, op);
    let inner = match op {
        ResolvedOperand::Aggregation(agg) => {
            if needs_nest_context(agg.inner_node()) {
                let nest_ctx = build_nest_context(src, agg.inner_node());
                build_agg_nest(src, agg, &agg_ctx, &nest_ctx)
            } else {
                build_agg(src, agg, &agg_ctx)
            }
        }
        _ => {
            let needs_nest = op.walk().into_iter().any(|o| {
                if let ResolvedOperand::Aggregation(agg) = o {
                    needs_nest_context(agg.inner_node())
                } else {
                    false
                }
            });
            let scalar_expr = if needs_nest {
                let nest_ctx = build_nest_context_for_operand(src, op);
                build_agg_operand_subquery_nest(src, op, &agg_ctx, &nest_ctx)
            } else {
                build_agg_operand_subquery(src, op, &agg_ctx)
            };
            let mut stmt = Query::select();
            stmt.from(src);
            stmt.expr_as(scalar_expr, Scalar);
            stmt.limit(1);
            stmt
        }
    };
    scalar_to_volatile_row(inner)
}

fn typeof_eq(sv: &SimpleExpr, type_str: &str) -> SimpleExpr {
    Expr::expr(CustomFunc::type_of(sv.clone()))
        .eq(Expr::val(type_str.to_owned()))
}

fn cast_union(sv: &SimpleExpr, bitical_type: BiticalType) -> SimpleExpr {
    CustomFunc::union_value(
        bitical_type,
        Expr::expr(sv.clone()).cast_as(bitical_type),
    )
}

fn scalar_to_volatile_row(inner: SelectStatement) -> SelectStatement {
    let sv: SimpleExpr = Expr::col((Sub, Scalar)).into();

    let bool_name: SimpleExpr = Expr::case(
        Expr::expr(sv.clone()).cast_as(BiticalType::Boolean),
        Expr::val("TRUE"),
    )
    .finally(Expr::val("FALSE"))
    .into();
    let name_expr: SimpleExpr =
        Expr::case(Expr::expr(sv.clone()).is_null(), Expr::val("NULL"))
            .case(typeof_eq(&sv, "BOOLEAN"), bool_name)
            .finally(Expr::expr(sv.clone()).cast_as(BiticalType::String))
            .into();

    // NULL → 'numeric' (value is NULL regardless of declared type)
    // BOOLEAN → boolean, DOUBLE/FLOAT → double, VARCHAR → string, ELSE → integer
    // (SUM(BIGINT) returns HUGEINT in DuckDB, so 'BIGINT' cannot be used as a fixed check;
    // VARCHAR and float types are the only named exclusions; everything else falls to integer)
    let type_expr: SimpleExpr =
        Expr::case(Expr::expr(sv.clone()).is_null(), Expr::val("numeric"))
            .case(typeof_eq(&sv, "BOOLEAN"), Expr::val("boolean"))
            .case(typeof_eq(&sv, "DOUBLE"), Expr::val("double"))
            .case(typeof_eq(&sv, "FLOAT"), Expr::val("double"))
            .case(typeof_eq(&sv, "VARCHAR"), Expr::val("string"))
            .finally(Expr::val("integer"))
            .into();

    let value_expr: SimpleExpr = Expr::case(
        typeof_eq(&sv, "BOOLEAN"),
        cast_union(&sv, BiticalType::Boolean),
    )
    .case(
        typeof_eq(&sv, "DOUBLE"),
        cast_union(&sv, BiticalType::Double),
    )
    .case(
        typeof_eq(&sv, "FLOAT"),
        cast_union(&sv, BiticalType::Double),
    )
    .case(
        typeof_eq(&sv, "VARCHAR"),
        cast_union(&sv, BiticalType::String),
    )
    .finally(cast_union(&sv, BiticalType::Integer))
    .into();

    let tags = CustomFunc::list_value([
        CustomFunc::struct_pack_tag(
            Expr::val("name").into(),
            CustomFunc::union_value(BiticalType::String, name_expr),
            Expr::val("system").into(),
        ),
        CustomFunc::struct_pack_tag(
            Expr::val("bitical_type").into(),
            CustomFunc::union_value(BiticalType::String, type_expr),
            Expr::val("system").into(),
        ),
        CustomFunc::struct_pack_tag(
            Expr::val("value").into(),
            value_expr,
            Expr::val("system").into(),
        ),
    ]);

    let mut q = Query::select();
    // 揮発 id は SQL 側では NULL とし、fetch 後に Rust 側で採番する。
    q.expr_as(Expr::val(None::<i64>), Col::ItemId)
        .expr_as(Expr::val(0i64), Col::Rank)
        .expr_as(Expr::val("volatile"), Col::ItemKind)
        .expr_as(tags, QueryResultCol::Tags)
        .from_subquery(inner, Sub);
    q
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::PostgresQueryBuilder;

    #[test]
    fn test_scalar_to_volatile_row_structure() {
        let inner = Query::select()
            .expr_as(Expr::val(123i64), Scalar)
            .to_owned();
        let sql = scalar_to_volatile_row(inner).to_string(PostgresQueryBuilder);

        assert!(sql.contains("item_id"), "should have item_id: {}", sql);
        assert!(sql.contains("item_kind"), "should have item_kind: {}", sql);
        assert!(sql.contains("tags"), "should have tags: {}", sql);
        assert!(sql.contains("typeof"), "should have typeof: {}", sql);
        assert!(
            sql.contains("union_value"),
            "should have union_value: {}",
            sql
        );
        assert!(sql.contains("tag_type"), "should have tag_type: {}", sql);
        assert!(sql.contains("'name'"), "should have name tag: {}", sql);
        assert!(
            sql.contains("'bitical_type'"),
            "should have bitical_type tag: {}",
            sql
        );
        assert!(sql.contains("'value'"), "should have value tag: {}", sql);
    }
}
