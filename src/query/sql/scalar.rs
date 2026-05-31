// Copyright (C) 2026 coponhub
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
use crate::db::{Col, CustomFunc, Pronoun::*, QueryResultCol, Src, SqlType};
use crate::query::ast::ComparisonOp;
use crate::query::lens_resolver::ResolvedOperand;
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{Label, SType};
use sea_query::{Alias, BinOper, Expr, Query, SelectStatement, SimpleExpr};

pub(super) fn build_resolved_match_sql(src: &Src, 
    storage: &StorageMapping,
    sql_type: SqlType,
    op: ComparisonOp,
    label: &Label,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(src);
    q.cond_where(storage.to_condition(op, label, sql_type));
    q
}

pub(super) fn build_column_match_sql(src: &Src, 
    tag: SType,
    label: &Label,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(src);
    match label.value() {
        crate::types::LabelValue::Integer(i) => {
            let t = if matches!(tag, SType::Label) {
                Col::LabelInt.into()
            } else {
                tag
            };
            q.and_where(Expr::col(t).eq(i));
        }
        crate::types::LabelValue::String(s) => {
            let t = if matches!(tag, SType::Label) {
                Col::LabelStr.into()
            } else {
                tag
            };
            let val_str = if s.starts_with('^') {
                format!("{}*", &s[1..])
            } else {
                s.clone()
            };
            q.and_where(
                Expr::col(t)
                    .binary(BinOper::Custom("GLOB"), Expr::val(val_str)),
            );
        }
        crate::types::LabelValue::Literal(s) => {
            let t = if matches!(tag, SType::Label) {
                Col::LabelStr.into()
            } else {
                tag
            };
            q.and_where(Expr::col(t).eq(s.as_str()));
        }
        crate::types::LabelValue::Boolean(b) => {
            q.and_where(Expr::col(Col::LabelBool).eq(b));
        }
        crate::types::LabelValue::Double(bits) => {
            q.and_where(Expr::col(Col::LabelDouble).eq(f64::from_bits(bits)));
        }
        crate::types::LabelValue::Null => {
            q.and_where(Expr::col(Col::LabelStr).is_null());
        }
        crate::types::LabelValue::Date(dt) => {
            let t = if matches!(tag, SType::Label) { Col::LabelInt.into() } else { tag };
            q.and_where(Expr::col(t).eq(dt.to_timestamp()));
        }
    }
    q
}

pub(super) fn build_resolved_tag_tag_match_sql(src: &Src, 
    left_storage: &StorageMapping,
    left_sql_type: SqlType,
    op: ComparisonOp,
    right_storage: &StorageMapping,
    right_sql_type: SqlType,
) -> SelectStatement {
    let mut q = Query::select();
    q.column(Col::ItemId)
        .from(src)
        .group_by_col(Col::ItemId);
    let left_expr = build_tag_value_agg_expr(left_storage, left_sql_type);
    let right_expr = build_tag_value_agg_expr(right_storage, right_sql_type);
    q.and_having(left_expr.binary(to_bin_op(op), right_expr));
    q
}

pub(super) fn build_scalar_match_sql(src: &Src, 
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

pub(super) fn build_resolved_scalar_sql(src: &Src,
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

fn cast_union(sv: &SimpleExpr, sql_type: SqlType) -> SimpleExpr {
    CustomFunc::union_value(sql_type, Expr::expr(sv.clone()).cast_as(sql_type))
}

fn scalar_to_volatile_row(inner: SelectStatement) -> SelectStatement {
    let sv: SimpleExpr = Expr::col((Alias::new("s"), Scalar)).into();

    let bool_name: SimpleExpr = Expr::case(
        Expr::expr(sv.clone()).cast_as(SqlType::BOOLEAN),
        Expr::val("TRUE"),
    )
    .finally(Expr::val("FALSE"))
    .into();
    let name_expr: SimpleExpr =
        Expr::case(Expr::expr(sv.clone()).is_null(), Expr::val("NULL"))
            .case(typeof_eq(&sv, "BOOLEAN"), bool_name)
            .finally(Expr::expr(sv.clone()).cast_as(SqlType::VARCHAR))
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
        cast_union(&sv, SqlType::BOOLEAN),
    )
    .case(typeof_eq(&sv, "DOUBLE"), cast_union(&sv, SqlType::DOUBLE))
    .case(typeof_eq(&sv, "FLOAT"), cast_union(&sv, SqlType::DOUBLE))
    .case(typeof_eq(&sv, "VARCHAR"), cast_union(&sv, SqlType::VARCHAR))
    .finally(cast_union(&sv, SqlType::BIGINT))
    .into();

    let tags = CustomFunc::list_value([
        CustomFunc::struct_pack_tag(
            Expr::val("name").into(),
            CustomFunc::union_value(SqlType::VARCHAR, name_expr),
            Expr::val("system").into(),
        ),
        CustomFunc::struct_pack_tag(
            Expr::val("type").into(),
            CustomFunc::union_value(SqlType::VARCHAR, type_expr),
            Expr::val("system").into(),
        ),
        CustomFunc::struct_pack_tag(
            Expr::val("value").into(),
            value_expr,
            Expr::val("system").into(),
        ),
    ]);

    let mut q = Query::select();
    q.expr_as(Expr::val(0i64), Col::ItemId)
        .expr_as(Expr::val(0i64), Col::Rank)
        .expr_as(Expr::val("volatile"), Col::ItemKind)
        .expr_as(tags, QueryResultCol::Tags)
        .from_subquery(inner, Alias::new("s"));
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
        assert!(sql.contains("'type'"), "should have type tag: {}", sql);
        assert!(sql.contains("'value'"), "should have value tag: {}", sql);
    }
}
