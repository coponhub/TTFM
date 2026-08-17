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
    label_to_simple_expr, needs_nest_context,
};
use crate::db::{
    BiticalType, Col, CustomFunc, Pronoun::*, QueryResultCol, Src,
};
use crate::query::ast::ComparisonOp;
use crate::query::lens_resolver::ResolvedOperand;
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{Label, SType};
use sea_query::{
    CaseStatement, Condition, Expr, ExprTrait, Query, SelectStatement,
    SimpleExpr,
};

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

/// 区間形の `DateTimeRange` に op（Eq/Ne/Gt/Ge/Lt/Le）を適用した Condition を作る。
/// 旧 `DateTime::to_range` / `mtime_range_op`（tag.rs、削除済み）の op 行列をここに集約。
fn date_time_interval_condition_expr(
    expr: &SimpleExpr,
    op: crate::query::ast::BasicOp,
    start: i64,
    end: i64,
) -> Condition {
    use crate::query::ast::BasicOp;
    match op {
        BasicOp::Eq => Condition::all()
            .add(expr.clone().gte(start))
            .add(expr.clone().lte(end)),
        BasicOp::Ne => Condition::any()
            .add(expr.clone().lt(start))
            .add(expr.clone().gt(end)),
        BasicOp::Gt => Condition::all().add(expr.clone().gt(end)),
        BasicOp::Ge => Condition::all().add(expr.clone().gte(start)),
        BasicOp::Lt => Condition::all().add(expr.clone().lt(start)),
        BasicOp::Le => Condition::all().add(expr.clone().lte(end)),
    }
}

/// `EXTRACT(field FROM ts)` を作る。
fn date_time_extract_field(field: &str, ts: &SimpleExpr) -> SimpleExpr {
    Expr::cust_with_exprs(format!("EXTRACT({field} FROM $1)"), [ts.clone()])
}

/// 値を持つフィールドを `(EXTRACT 式, 値)` の列へ落とす（`fields` の順を保つ）。
/// 自由なフィールドは条件にならないので除く。
fn date_time_slot_pairs(
    local_ts: &SimpleExpr,
    fields: &[crate::types::DateField],
    slots: &[crate::types::DateSlot],
) -> Vec<(SimpleExpr, i64)> {
    use crate::types::DateSlot;
    fields
        .iter()
        .zip(slots)
        .filter_map(|(field, slot)| match slot {
            DateSlot::Free => None,
            DateSlot::Value(v) => Some((
                date_time_extract_field(field.extract_name(), local_ts),
                *v,
            )),
        })
        .collect()
}

/// フィールドの列（出現順）が全て等しいことを表す Condition。
fn date_time_tuple_eq(fields: &[(SimpleExpr, i64)]) -> Condition {
    let mut c = Condition::all();
    for (expr, v) in fields {
        c = c.add(expr.clone().eq(*v));
    }
    c
}

/// フィールドの列を辞書式に比較する Condition（`cmp` は各要素の狭義比較）。
/// `OR_i [ AND_{j<i} (field_j == v_j) AND cmp(field_i, v_i) ]` の形。
fn date_time_tuple_cmp<F>(fields: &[(SimpleExpr, i64)], cmp: F) -> Condition
where
    F: Fn(SimpleExpr, i64) -> SimpleExpr,
{
    let mut disjuncts = Condition::any();
    for i in 0..fields.len() {
        let mut conj = Condition::all();
        for (expr, v) in &fields[..i] {
            conj = conj.add(expr.clone().eq(*v));
        }
        conj = conj.add(cmp(fields[i].0.clone(), fields[i].1));
        disjuncts = disjuncts.add(conj);
    }
    disjuncts
}

/// 順序演算子（Gt/Ge/Lt/Le）のスロットへの周期内適用。
/// 先頭から連続する値フィールドは外側の等値制限として切り出し、最初の自由フィールド
/// 以降にある値フィールドを出現順に集めて辞書式比較のタプルを作る
/// （自由なフィールドは位置を問わずタプルから除外する）。
fn date_time_slot_order_condition(
    local_ts: &SimpleExpr,
    op: crate::query::ast::BasicOp,
    slots: &[crate::types::DateSlot; crate::types::DateField::COUNT],
) -> Condition {
    use crate::query::ast::BasicOp;
    use crate::types::{DateField, DateSlot};

    let first_free = slots
        .iter()
        .position(|s| matches!(s, DateSlot::Free))
        .unwrap_or(DateField::COUNT);
    let (leading, rest) = slots.split_at(first_free);

    let outer = date_time_tuple_eq(&date_time_slot_pairs(
        local_ts,
        &DateField::ALL[..first_free],
        leading,
    ));
    let pairs =
        date_time_slot_pairs(local_ts, &DateField::ALL[first_free..], rest);

    let inner = match op {
        BasicOp::Gt => date_time_tuple_cmp(&pairs, |e, v| e.gt(v)),
        BasicOp::Ge => Condition::any()
            .add(date_time_tuple_cmp(&pairs, |e, v| e.gt(v)))
            .add(date_time_tuple_eq(&pairs)),
        BasicOp::Lt => date_time_tuple_cmp(&pairs, |e, v| e.lt(v)),
        BasicOp::Le => Condition::any()
            .add(date_time_tuple_cmp(&pairs, |e, v| e.lt(v)))
            .add(date_time_tuple_eq(&pairs)),
        BasicOp::Eq | BasicOp::Ne => unreachable!("Eq/Ne はこの関数を通らない"),
    };

    if first_free > 0 {
        Condition::all().add(outer).add(inner)
    } else {
        inner
    }
}

fn to_microseconds(seconds: SimpleExpr) -> SimpleExpr {
    seconds.mul(1_000_000)
}

fn date_time_slot_condition_expr(
    expr: &SimpleExpr,
    op: crate::query::ast::BasicOp,
    slots: &[crate::types::DateSlot; crate::types::DateField::COUNT],
) -> Condition {
    use crate::query::ast::BasicOp;
    let offset_secs = crate::types::DateTime::local_utc_offset_secs();
    let int_secs: SimpleExpr = Expr::expr(expr.clone())
        .cast_as(BiticalType::Integer)
        .into();
    let local_epoch_secs = int_secs.add(offset_secs);
    let local_ts: SimpleExpr = Expr::cust_with_exprs(
        "make_timestamp($1)",
        [to_microseconds(local_epoch_secs)],
    );
    match op {
        BasicOp::Eq | BasicOp::Ne => {
            // 全フィールドが自由なら条件は空 = 常に真（そのタグを持つ全アイテム）。
            let positive = date_time_tuple_eq(&date_time_slot_pairs(
                &local_ts,
                &crate::types::DateField::ALL,
                slots,
            ));
            if op == BasicOp::Ne {
                positive.not()
            } else {
                positive
            }
        }
        BasicOp::Gt | BasicOp::Ge | BasicOp::Lt | BasicOp::Le => {
            date_time_slot_order_condition(&local_ts, op, slots)
        }
    }
}

/// `DateTimeRange` に op を適用した Condition を組む（区間/スロットいずれも対応）。
pub(super) fn date_time_condition_expr(
    expr: &SimpleExpr,
    op: crate::query::ast::BasicOp,
    range: &crate::types::DateTimeRange,
) -> Condition {
    match range {
        crate::types::DateTimeRange::Interval { start, end } => {
            date_time_interval_condition_expr(expr, op, *start, *end)
        }
        crate::types::DateTimeRange::Slots(slots) => {
            date_time_slot_condition_expr(expr, op, slots)
        }
    }
}

fn date_time_condition(
    col: Col,
    op: crate::query::ast::BasicOp,
    range: &crate::types::DateTimeRange,
) -> Condition {
    date_time_condition_expr(&Expr::col(col).into(), op, range)
}

pub(super) fn build_resolved_date_time_match_sql(
    src: &Src,
    storage: &StorageMapping,
    op: crate::query::ast::BasicOp,
    range: &crate::types::DateTimeRange,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(src);
    let cond = match storage {
        StorageMapping::Fixed(col) => date_time_condition(*col, op, range),
        StorageMapping::Basic { column, tag_type } => Condition::all()
            .add(crate::query::lens_schema::check_tag_match(tag_type))
            .add(date_time_condition(*column, op, range)),
        StorageMapping::Composite => Condition::any(),
    };
    q.cond_where(cond);
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
    let cond = Expr::expr(label_to_simple_expr(left))
        .binary(to_bin_op(op), label_to_simple_expr(right));
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

fn cast_union(sv: &SimpleExpr, bitical_type: BiticalType) -> SimpleExpr {
    CustomFunc::union_value(
        bitical_type,
        Expr::expr(sv.clone()).cast_as(bitical_type),
    )
}

fn typeof_eq(sv: &SimpleExpr, type_str: &str) -> SimpleExpr {
    Expr::expr(CustomFunc::type_of(sv.clone()))
        .eq(Expr::val(type_str.to_owned()))
}

/// `typeof(sv)` の値によって分岐する SQL 式を、typeof 文字列を持つ `BiticalType`
/// 全種について組み立てる。候補のいずれにも一致しない場合は `default` を返す
/// （`SUM(BIGINT)` が `HUGEINT` を返すなど、整数系は typeof 名を固定できないため、
/// Integer 相当がこの既定枝に落ちる想定）。`arm` は各候補の `BiticalType` から
/// 分岐値を組み立てるコールバック。
fn to_expr<F>(sv: &SimpleExpr, default: SimpleExpr, mut arm: F) -> SimpleExpr
where
    F: FnMut(BiticalType) -> SimpleExpr,
{
    use strum::IntoEnumIterator;
    let mut case = CaseStatement::new();
    for bt in BiticalType::iter() {
        if let Some(typeof_str) = bt.to_typeofstr() {
            case = case.case(typeof_eq(sv, typeof_str), arm(bt));
        }
    }
    case.finally(default).into()
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
            .case(
                typeof_eq(
                    &sv,
                    BiticalType::Boolean
                        .to_typeofstr()
                        .expect("Boolean has a typeof string"),
                ),
                bool_name,
            )
            .finally(Expr::expr(sv.clone()).cast_as(BiticalType::String))
            .into();

    // NULL → 'numeric' (value is NULL regardless of declared type); typeof 文字列 →
    // BiticalType の判定・分岐の組み立ては to_expr に集約している
    let type_expr: SimpleExpr =
        Expr::case(Expr::expr(sv.clone()).is_null(), Expr::val("numeric"))
            .finally(to_expr(
                &sv,
                Expr::val(BiticalType::Integer.to_string()).into(),
                |bt| Expr::val(bt.to_string()).into(),
            ))
            .into();

    let value_expr: SimpleExpr =
        to_expr(&sv, cast_union(&sv, BiticalType::Integer), |bt| {
            cast_union(&sv, bt)
        });

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

    #[test]
    fn test_date_time_match_sql_eq_uses_ge_le_on_basic_column() {
        use crate::query::ast::BasicOp;
        use crate::types::DateTimeRange;
        let storage = StorageMapping::Basic {
            column: Col::LabelInt,
            tag_type: "mtime".to_string(),
        };
        let range = DateTimeRange::interval(1000, 2000);
        let sql = build_resolved_date_time_match_sql(
            &Src::OneView,
            &storage,
            BasicOp::Eq,
            &range,
        )
        .to_string(PostgresQueryBuilder);

        assert!(
            sql.contains("label_int"),
            "should filter label_int: {}",
            sql
        );
        assert!(sql.contains("1000"), "should reference start: {}", sql);
        assert!(sql.contains("2000"), "should reference end: {}", sql);
        assert!(
            sql.contains("'mtime'"),
            "should filter by tag_type: {}",
            sql
        );
    }

    #[test]
    fn test_date_time_match_sql_gt_uses_end_only() {
        use crate::query::ast::BasicOp;
        use crate::types::DateTimeRange;
        let storage = StorageMapping::Basic {
            column: Col::LabelInt,
            tag_type: "mtime".to_string(),
        };
        let range = DateTimeRange::interval(1000, 2000);
        let sql = build_resolved_date_time_match_sql(
            &Src::OneView,
            &storage,
            BasicOp::Gt,
            &range,
        )
        .to_string(PostgresQueryBuilder);

        assert!(
            sql.contains("> 2000"),
            "Gt should compare against end: {}",
            sql
        );
        assert!(
            !sql.contains(">= 1000") && !sql.contains("> 1000"),
            "Gt should not reference start: {}",
            sql
        );
    }

    #[test]
    fn test_date_time_match_sql_eq_slots_uses_extract_month_day() {
        // mtime:*-02-01 → 年をまたいで各年の2月1日
        let sql = slot_eq_sql("*-02-01");

        assert!(
            sql.to_uppercase().contains("EXTRACT"),
            "should use EXTRACT: {}",
            sql
        );
        assert!(sql.contains("MONTH"), "should extract month: {}", sql);
        assert!(sql.contains("DAY"), "should extract day: {}", sql);
        assert!(
            !sql.contains("YEAR"),
            "year is free, should not extract year: {}",
            sql
        );
        assert!(
            sql.contains("label_int"),
            "should filter label_int: {}",
            sql
        );
        assert!(
            sql.contains("'mtime'"),
            "should filter by tag_type: {}",
            sql
        );
    }

    #[test]
    fn test_date_time_match_sql_ne_slots_negates() {
        let sql = slot_sql(crate::query::ast::BasicOp::Ne, "*-02-01");
        assert!(
            sql.to_uppercase().contains("NOT"),
            "Ne should negate the slot match: {}",
            sql
        );
    }

    /// パターン文字列からスロット制約の SQL を組む（フィールド単位 glob の確認用）。
    fn slot_sql(op: crate::query::ast::BasicOp, pattern: &str) -> String {
        let storage = StorageMapping::Basic {
            column: Col::LabelInt,
            tag_type: "mtime".to_string(),
        };
        let range = crate::types::DateTimeRange::parse_slot_glob(pattern)
            .unwrap_or_else(|| panic!("受理されるべきパターン: {pattern}"));
        build_resolved_date_time_match_sql(&Src::OneView, &storage, op, &range)
            .to_string(PostgresQueryBuilder)
    }

    fn slot_eq_sql(pattern: &str) -> String {
        slot_sql(crate::query::ast::BasicOp::Eq, pattern)
    }

    #[test]
    fn test_date_time_match_sql_eq_slots_year_field_only() {
        // mtime:2026-* → 2026年全体（月日は自由なので抽出しない）
        let sql = slot_eq_sql("2026-*");
        assert!(sql.contains("YEAR"), "should extract year: {}", sql);
        assert!(sql.contains("2026"), "should restrict to 2026: {}", sql);
        assert!(!sql.contains("MONTH"), "month is free: {}", sql);
        assert!(!sql.contains("DAY"), "day is free: {}", sql);
    }

    #[test]
    fn test_date_time_match_sql_eq_slots_time_fields() {
        // mtime:12:* → 各日の12時台（日付は自由）
        let sql = slot_eq_sql("12:*");
        assert!(sql.contains("HOUR"), "should extract hour: {}", sql);
        assert!(sql.contains("12"), "should restrict to hour 12: {}", sql);
        assert!(!sql.contains("YEAR"), "year is free: {}", sql);
        assert!(!sql.contains("MINUTE"), "minute is free: {}", sql);
    }

    #[test]
    fn test_date_time_match_sql_eq_slots_date_and_time() {
        // mtime:*-02-01T12:* → 各年2月1日の12時台
        let sql = slot_eq_sql("*-02-01T12:*");
        assert!(sql.contains("MONTH"), "should extract month: {}", sql);
        assert!(sql.contains("DAY"), "should extract day: {}", sql);
        assert!(sql.contains("HOUR"), "should extract hour: {}", sql);
        assert!(!sql.contains("YEAR"), "year is free: {}", sql);
    }

    #[test]
    fn test_date_time_match_sql_eq_slots_all_free_has_no_extract() {
        // 全フィールド自由な Slots（parse_slot_glob 経由では裸の * は辞退されるため直接構築）
        use crate::types::{DateField, DateSlot, DateTimeRange};
        let storage = StorageMapping::Basic {
            column: Col::LabelInt,
            tag_type: "mtime".to_string(),
        };
        let range = DateTimeRange::Slots([DateSlot::Free; DateField::COUNT]);
        let sql = build_resolved_date_time_match_sql(
            &Src::OneView,
            &storage,
            crate::query::ast::BasicOp::Eq,
            &range,
        )
        .to_string(PostgresQueryBuilder);
        assert!(
            !sql.to_uppercase().contains("EXTRACT"),
            "全フィールド自由なら EXTRACT は出ない: {}",
            sql
        );
        assert!(sql.contains("'mtime'"), "型の絞り込みは残る: {}", sql);
    }

    #[test]
    fn test_date_time_match_sql_gt_slots_lexicographic_month_day() {
        use crate::query::ast::BasicOp;
        // mtime: > *-02-01 → 各年の2月1日より後
        let sql = slot_sql(BasicOp::Gt, "*-02-01");
        assert!(sql.contains("MONTH"), "should extract month: {}", sql);
        assert!(sql.contains("DAY"), "should extract day: {}", sql);
        assert!(!sql.contains("YEAR"), "year is free: {}", sql);
        assert!(sql.contains("> 2"), "should compare month > 2: {}", sql);
        assert!(sql.contains("> 1"), "should compare day > 1: {}", sql);
    }

    #[test]
    fn test_date_time_match_sql_lt_slots_lexicographic_month_day() {
        use crate::query::ast::BasicOp;
        // mtime: < *-02-01 → 各年の1月中
        let sql = slot_sql(BasicOp::Lt, "*-02-01");
        assert!(sql.contains("< 2"), "should compare month < 2: {}", sql);
        assert!(sql.contains("< 1"), "should compare day < 1: {}", sql);
    }

    #[test]
    fn test_date_time_match_sql_gt_slots_day_only_when_year_month_free() {
        use crate::query::ast::BasicOp;
        // mtime: > *-*-15 → 各月の16日以降（year/month は自由なので抽出しない）
        let sql = slot_sql(BasicOp::Gt, "*-*-15");
        assert!(!sql.contains("YEAR"), "year is free: {}", sql);
        assert!(!sql.contains("MONTH"), "month is free: {}", sql);
        assert!(sql.contains("DAY"), "should extract day: {}", sql);
        assert!(sql.contains("> 15"), "should compare day > 15: {}", sql);
    }

    #[test]
    fn test_date_time_match_sql_ge_slots_outer_year_restriction() {
        use crate::query::ast::BasicOp;
        // mtime: >= 2026-*-01 → 2026年内で各月の1日以降（year は外側の等値制限）
        let sql = slot_sql(BasicOp::Ge, "2026-*-01");
        assert!(sql.contains("YEAR"), "should extract year: {}", sql);
        assert!(
            sql.contains("2026"),
            "should restrict to year 2026: {}",
            sql
        );
        assert!(!sql.contains("MONTH"), "month is free: {}", sql);
        assert!(sql.contains("DAY"), "should extract day: {}", sql);
    }

    #[test]
    fn test_date_time_match_sql_gt_slots_month_only_when_day_free() {
        use crate::query::ast::BasicOp;
        // mtime: > *-02-* → 各年の3月以降（day は自由なのでタプルから落ち、
        // month 単独の比較になる）
        let sql = slot_sql(BasicOp::Gt, "*-02-*");
        assert!(!sql.contains("YEAR"), "year is free: {}", sql);
        assert!(sql.contains("MONTH"), "should extract month: {}", sql);
        assert!(!sql.contains("DAY"), "day is free: {}", sql);
        assert!(sql.contains("> 2"), "should compare month > 2: {}", sql);
    }
}
