use crate::db::{Col, QueryResultCol, SqlType};
use crate::query::ast::{ArithmeticAggOp, ComparisonOp};
use crate::query::lens_resolver::{
    LabelSetOpKind, NestMatchCondition, NestMatchOp, ResolvedAggregationNode,
    ResolvedCalculationNode, ResolvedNode, ResolvedOperand,
};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{ItemKind, Label};
use sea_query::{Alias, Condition, Expr, Func, IntoIden, Query, SelectStatement, SimpleExpr};
use super::{
    apply_arithmetic_agg, build_calculation_eav_expr, build_calculation_expr,
    build_calculation_subquery, build_column_match_sql, build_merged_nest_match_sql,
    build_nest_pivot_cte, build_nvalue_standalone_subquery,
    build_agg, build_resolved_operand_eav_row_expr, build_tag_value_eav_row_expr,
    build_resolved_and_sql, build_resolved_comp_sql, build_resolved_diff_sql,
    build_resolved_match_sql, build_resolved_or_sql, build_resolved_projection_sql,
    build_resolved_tag_tag_match_sql, build_storage_column_expr,
    build_tag_calc_match_eav_sql, label_to_unit_aware_expr, subquery,
};

/// CalculationNodeに含まれるRowTagのtypeフィルタをWHERE句に追加します。
fn add_type_filters(
    stmt: &mut SelectStatement,
    calc: &ResolvedCalculationNode,
) {
    // 左オペランドのチェック
    match &calc.left {
        ResolvedOperand::TagRef { storage, .. } => {
            if let StorageMapping::RowTag { tag_type, .. } = storage {
                stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
            }
        }
        ResolvedOperand::Calculation(nested) => {
            add_type_filters(stmt, nested);
        }
        _ => {}
    }

    // 右オペランドのチェック
    match &calc.right {
        ResolvedOperand::TagRef { storage, .. } => {
            if let StorageMapping::RowTag { tag_type, .. } = storage {
                stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
            }
        }
        ResolvedOperand::Calculation(nested) => {
            add_type_filters(stmt, nested);
        }
        _ => {}
    }
}

/// 物理マッピング解決済みの構造から SQL を生成します。
pub fn build_pick_sql(node: &ResolvedNode, view: &str) -> SelectStatement {
    match node {
        ResolvedNode::And(nodes) => build_resolved_and_sql(nodes, view),
        ResolvedNode::Or(nodes) => build_resolved_or_sql(nodes, view),
        ResolvedNode::Difference(l, r) => build_resolved_diff_sql(l, r, view),
        ResolvedNode::Nest { keys, nvalue: _, context } => {
            build_nest_sql(keys, context, view)
        }
        ResolvedNode::MergedNestMatch { keys, matches, is_or } => {
            build_merged_nest_match_sql(keys, matches, *is_or, view)
        }
        ResolvedNode::ColumnMatch { tag, label } => {
            build_column_match_sql(*tag, label, view)
        }
        ResolvedNode::Match { storage, sql_type, op, label, .. } => {
            build_resolved_match_sql(storage, *sql_type, *op, label, view)
        }
        ResolvedNode::Aggregation(agg) => build_agg(agg, view),
        ResolvedNode::AggregationMatch { agg, op, label } => {
            build_agg_match(agg, *op, label, view)
        }
        ResolvedNode::CalculationMatch { calc, op, label } => {
            build_calculation_match_sql(calc, *op, label, view)
        }
        ResolvedNode::TagCalculationMatch { storage, sql_type, op, calc, .. } => {
            build_tag_calculation_match_sql(storage, *sql_type, *op, calc, view)
        }
        ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
            build_agg_calc_match(agg, *op, calc, view)
        }
        ResolvedNode::CalculationCalculationMatch { left_calc, op, right_calc } => {
            build_calculation_calculation_match_sql(left_calc, *op, right_calc, view)
        }
        ResolvedNode::AggregationAggregationMatch { left, op, right } => {
            build_agg_agg_match(left, *op, right, view)
        }
        ResolvedNode::TagTagMatch {
            left_storage, left_sql_type, op, right_storage, right_sql_type,
        } => build_resolved_tag_tag_match_sql(
            left_storage, *left_sql_type, *op, right_storage, *right_sql_type, view,
        ),
        ResolvedNode::AggregationTagMatch { agg, op, storage, sql_type, .. } => {
            build_agg_tag_match(agg, *op, storage, *sql_type, view)
        }
        ResolvedNode::NestMatch { keys, nvalue, op, label, context } => {
            build_nest_match_sql(keys, nvalue, *op, label, context, view)
        }
        ResolvedNode::NestNestMatch {
            left_keys, left_nvalue, left_context,
            op,
            right_keys, right_nvalue, right_context,
        } => build_nest_nest_match_sql(
            left_keys, left_nvalue, left_context,
            op,
            right_keys, right_nvalue, right_context,
            view,
        ),
        ResolvedNode::ScalarMatch { left, op, right } => {
            build_scalar_match_sql(left, *op, right, view)
        }
        ResolvedNode::Complement(c) => build_resolved_comp_sql(c, view),
        ResolvedNode::LabelSetOp { op, operands } => {
            build_label_set_op_pick_sql(op, operands, view)
        }
    }
}

fn build_nest_sql(
    keys: &[ResolvedOperand],
    context: &Option<Box<ResolvedNode>>,
    view: &str,
) -> SelectStatement {
    let mut stmt = build_resolved_projection_sql(keys.first().unwrap(), view);
    // 2番目以降のキーも AND 条件として追加（例: tagA: &: tagB: → tagA AND tagB）
    for key in keys.iter().skip(1) {
        let key_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(build_resolved_projection_sql(key, view), Alias::new("_key"))
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(key_sub));
    }
    if let Some(ctx) = context {
        // build_pick_sql returns 3 columns (item_id, rank, item_kind);
        // wrap it to select only item_id for use in IN subquery.
        let ctx_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(build_pick_sql(ctx, view), Alias::new("_ctx"))
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(ctx_sub));
    }
    stmt
}

fn build_calculation_match_sql(
    calc: &ResolvedCalculationNode,
    op: ComparisonOp,
    label: &Label,
    view: &str,
) -> SelectStatement {
    if calc.contains_row_tag() {
        // RowTag を含む場合は GROUP BY item_id + HAVING で集約計算する。
        // EAV モデル上、type='size' と type='mtime' は別行に存在するため、
        // WHERE で両方同時にフィルタすると必ず空になる。HAVING で解決する。
        let mut stmt = Query::select();
        stmt.column(Col::ItemId)
            .from(Alias::new(view))
            .group_by_col(Col::ItemId);
        let calc_expr = build_calculation_eav_expr(calc, view);
        let label_expr = label_to_unit_aware_expr(label);
        stmt.and_having(Expr::expr(calc_expr).binary(to_bin_op(op), label_expr));
        stmt
    } else {
        // RowTag を含まない純粋なスカラー/集約計算
        let mut stmt = Query::select();
        stmt.from(Alias::new(view));
        stmt.column(Col::ItemId);
        let calc_expr = if calc.contains_aggregation() {
            build_calculation_subquery(calc, view)
        } else {
            build_calculation_expr(calc, view)
        };
        let label_expr = label_to_unit_aware_expr(label);
        let cond = Expr::expr(calc_expr).binary(to_bin_op(op), label_expr);
        stmt.cond_where(cond);
        if !calc.contains_aggregation() {
            add_type_filters(&mut stmt, calc);
        }
        stmt
    }
}

fn build_tag_calculation_match_sql(
    storage: &StorageMapping,
    sql_type: SqlType,
    op: ComparisonOp,
    calc: &ResolvedCalculationNode,
    view: &str,
) -> SelectStatement {
    // RowTag が関与する場合は GROUP BY HAVING で集約計算を行う
    let needs_eav = calc.contains_row_tag()
        || matches!(storage, StorageMapping::RowTag { .. });
    if needs_eav {
        build_tag_calc_match_eav_sql(storage, sql_type, op, calc, view)
    } else {
        let mut stmt = Query::select();
        stmt.from(Alias::new(view));
        stmt.column(Col::ItemId);
        let tag_expr = build_storage_column_expr(storage, sql_type);
        let calc_expr = if calc.contains_aggregation() {
            build_calculation_subquery(calc, view)
        } else {
            build_calculation_expr(calc, view)
        };
        let cond = Expr::expr(tag_expr).binary(to_bin_op(op), calc_expr);
        stmt.cond_where(cond);
        if !calc.contains_aggregation() {
            add_type_filters(&mut stmt, calc);
        }
        stmt
    }
}

fn build_agg_calc_match(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    calc: &ResolvedCalculationNode,
    view: &str,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));
    stmt.column(Col::ItemId);
    let agg_expr = subquery(build_agg(agg, view));
    let calc_expr = if calc.contains_aggregation() {
        build_calculation_subquery(calc, view)
    } else {
        build_calculation_expr(calc, view)
    };
    stmt.cond_where(Expr::expr(agg_expr).binary(to_bin_op(op), calc_expr));
    stmt
}

fn build_calculation_calculation_match_sql(
    left_calc: &ResolvedCalculationNode,
    op: ComparisonOp,
    right_calc: &ResolvedCalculationNode,
    view: &str,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.column(Col::ItemId)
        .from(Alias::new(view))
        .group_by_col(Col::ItemId);
    let left_expr = build_calculation_eav_expr(left_calc, view);
    let right_expr = build_calculation_eav_expr(right_calc, view);
    stmt.and_having(Expr::expr(left_expr).binary(to_bin_op(op), right_expr));
    stmt
}

fn build_agg_agg_match(
    left: &ResolvedAggregationNode,
    op: ComparisonOp,
    right: &ResolvedAggregationNode,
    view: &str,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));
    stmt.column(Col::ItemId);
    let left_expr = subquery(build_agg(left, view));
    let right_expr = subquery(build_agg(right, view));
    stmt.cond_where(Expr::expr(left_expr).binary(to_bin_op(op), right_expr));
    stmt
}

fn build_agg_tag_match(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    storage: &StorageMapping,
    sql_type: SqlType,
    view: &str,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));
    stmt.column(Col::ItemId);
    let agg_expr = subquery(build_agg(agg, view));
    let tag_expr = build_storage_column_expr(storage, sql_type);
    stmt.cond_where(Expr::expr(agg_expr).binary(to_bin_op(op), tag_expr));
    if let StorageMapping::RowTag { tag_type, .. } = storage {
        stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
    }
    stmt
}

fn decompose_agg(
    agg: &ResolvedAggregationNode,
    view: &str,
) -> (SimpleExpr, Option<ResolvedNode>, Option<String>) {
    match agg {
        ResolvedAggregationNode::Count(node) => {
            let (storage, cond, _) = node.extract_agg_parts();
            let tag_type;
            let expr = if let Some(s) = storage {
                let col = match s {
                    StorageMapping::Column(c) => {
                        tag_type = None;
                        *c
                    }
                    StorageMapping::RowTag { column, tag_type: key } => {
                        tag_type = Some(key.clone());
                        *column
                    }
                    _ => {
                        tag_type = None;
                        Col::LabelInt
                    }
                };
                Expr::col(col).count_distinct().into()
            } else {
                tag_type = None;
                Expr::col(Col::ItemId).count_distinct().into()
            };
            (expr, cond, tag_type)
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let (storage, cond, operand) = inner.extract_agg_parts();
            let tag_type = match &storage {
                Some(StorageMapping::RowTag { tag_type: key, .. }) => Some(key.clone()),
                _ => None,
            };
            let expr: SimpleExpr = if let Some(operand) = operand {
                let is_string = operand.is_string_type();
                let inner_expr = build_resolved_operand_eav_row_expr(operand, view);
                apply_arithmetic_agg(op, inner_expr, is_string)
            } else {
                let tag_row_expr = build_tag_value_eav_row_expr(
                    &storage.unwrap(),
                    SqlType::DOUBLE,
                );
                apply_arithmetic_agg(op, tag_row_expr, false)
            };
            (expr, cond, tag_type)
        }
    }
}

fn build_agg_match(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    label: &Label,
    view: &str,
) -> SelectStatement {
    let (agg_expr, cond, tag_type) = decompose_agg(agg, view);
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));

    let op_bin = to_bin_op(op);
    let rhs = Expr::val(label.as_i64()); // TODO: 型に応じた変換
    let condition = Expr::expr(agg_expr).binary(op_bin, rhs);

    // 集計対象（Nest）がある場合、その型(type)自体で行を絞り込む必要がある
    let _target_type = match agg {
        ResolvedAggregationNode::Count(inner) => inner.get_projection(),
        ResolvedAggregationNode::Arithmetic { inner, .. } => inner.get_projection(),
    };

    let mut final_cond = Condition::all();
    if let Some(key) = tag_type {
        // RowTag の場合は実際の tag_type でフィルタする
        // エイリアス名 "type" との競合を避けるためテーブル名を明示
        final_cond =
            final_cond.add(Expr::col((Alias::new(view), Col::Type)).eq(key));
    }

    // 検索条件 (cond) がある場合は、それを ItemId の絞り込みとして IN サブクエリに送る
    if let Some(filter_node) = cond {
        let pick_sql = build_pick_sql(&filter_node, view);
        let sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(pick_sql, Alias::new("sub"))
            .to_owned();
        final_cond = final_cond.add(Expr::col(Col::ItemId).in_subquery(sub));
    }
    stmt.cond_where(final_cond);

    // 集計結果の比較条件を HAVING 句に追加
    // これにより、条件を満たさない場合は行が返らなくなり、INTERSECT 等の結合が正しく機能する
    stmt.cond_having(condition);

    stmt.expr_as(Expr::cust("ANY_VALUE(item_id)"), Col::ItemId);
    stmt.expr_as(
        Expr::val(<&'static str>::from(ItemKind::Volatile)),
        Col::ItemKind,
    );
    stmt.expr_as(
        Expr::val(<&'static str>::from(ItemKind::Volatile)),
        Col::Type,
    );
    stmt.expr_as(Expr::val(0i64), Col::Rank);
    // tags カラムが必要（fetch_items で decode_item_from_row が呼ばれるため）
    stmt.expr_as(Expr::cust("[]"), QueryResultCol::Tags);
    stmt.limit(1);

    stmt.to_owned()
}

fn build_nest_match_sql(
    keys: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    comparison_op: ComparisonOp,
    label: &Label,
    context: &Option<Box<ResolvedNode>>,
    view: &str,
) -> SelectStatement {
    if keys.len() == 1 {
        // 単一キーの場合は従来の build_nvalue_standalone_subquery アプローチを使用
        let nvalue_sub = build_nvalue_standalone_subquery(
            &keys[0], nvalue, context.as_deref(), view, false,
        );
        let bin_op = to_bin_op(comparison_op);
        let label_expr = label_to_unit_aware_expr(label);

        // nvalue フィルタを適用した group_label サブクエリ
        let mut nfilter = Query::select();
        nfilter.column(Alias::new("group_label"));
        nfilter.from_subquery(nvalue_sub, Alias::new("nfilter"));
        nfilter.and_where(
            Expr::col(Alias::new("nvalue")).binary(bin_op, label_expr),
        );

        // 外側: キーの type フィルタ + label_str IN (group_label)
        let (proj_col, proj_tag_type) = match keys[0].get_storage() {
            Some(StorageMapping::RowTag { column, tag_type }) => (*column, Some(tag_type.as_str())),
            Some(StorageMapping::Column(col)) => (*col, None),
            _ => panic!("NestMatch key must have RowTag or Column storage"),
        };
        let mut stmt = Query::select();
        stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        stmt.distinct();
        stmt.from(Alias::new(view));
        if let Some(tag_type) = proj_tag_type {
            stmt.and_where(Expr::col(Col::Type).eq(tag_type));
        }
        stmt.and_where(Expr::col(proj_col).in_subquery(nfilter));
        stmt
    } else {
        // Level 3+ のネスト比較: Pivot CTE + ウィンドウ関数
        let pivot_sub = build_nest_pivot_cte(keys, Some(nvalue), view);
        let pivot_alias = Alias::new("pivot_match");

        let mut stmt = Query::select();
        stmt.column(Col::ItemId);
        stmt.column(Alias::new("rank"));
        stmt.column(Alias::new("item_kind"));

        let partition_keys: Vec<SimpleExpr> = (0..keys.len())
            .map(|i| Expr::col(Alias::new(&format!("key{}", i))).into())
            .collect();

        let bin_op = to_bin_op(comparison_op);
        let label_expr = label_to_unit_aware_expr(label);

        let agg_func = match nvalue {
            ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_)) => {
                Func::sum(Expr::col(Alias::new("nvalue")))
            }
            ResolvedOperand::Aggregation(ResolvedAggregationNode::Arithmetic { op, .. }) => {
                match op {
                    ArithmeticAggOp::Sum => Func::sum(Expr::col(Alias::new("nvalue"))),
                    ArithmeticAggOp::Avg => Func::avg(Expr::col(Alias::new("nvalue"))),
                    ArithmeticAggOp::Max => Func::max(Expr::col(Alias::new("nvalue"))),
                    ArithmeticAggOp::Min => Func::min(Expr::col(Alias::new("nvalue"))),
                }
            }
            _ => Func::cust(Alias::new("MAX")).arg(Expr::col(Alias::new("nvalue"))),
        };

        use sea_query::{OverStatement, SelectExpr, WindowSelectType, WindowStatement};
        let mut window = WindowStatement::new();
        for pk in &partition_keys {
            window.add_partition_by(pk.clone());
        }
        stmt.expr(SelectExpr {
            expr: agg_func.into(),
            alias: Some(Alias::new("group_nvalue").into_iden()),
            window: Some(WindowSelectType::Query(window)),
        });

        if let Some(ctx) = context {
            let mut psub = Query::select();
            psub.column(Col::ItemId)
                .from_subquery(build_pick_sql(ctx, view), Alias::new("ctx_p"));
            stmt.and_where(Expr::col(Col::ItemId).in_subquery(psub));
        }
        stmt.from_subquery(pivot_sub, pivot_alias);

        let mut final_stmt = Query::select();
        final_stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        final_stmt.distinct();
        final_stmt.from_subquery(stmt, Alias::new("filtered_items"));
        final_stmt.and_where(
            Expr::col(Alias::new("group_nvalue")).binary(bin_op, label_expr),
        );
        final_stmt
    }
}

fn build_nest_nest_match_sql(
    left_keys: &[ResolvedOperand],
    left_nvalue: &ResolvedOperand,
    left_context: &Option<Box<ResolvedNode>>,
    op: &NestMatchOp,
    right_keys: &[ResolvedOperand],
    right_nvalue: &ResolvedOperand,
    right_context: &Option<Box<ResolvedNode>>,
    view: &str,
) -> SelectStatement {
    match op {
        NestMatchOp::Comparison(cmp_op) => {
            // right_nvalue が Aggregation/Calculation の場合（agg vs agg/calc 比較）：
            // MergedNestMatch の HAVING アプローチを再利用する。
            let is_agg_or_calc = matches!(
                right_nvalue,
                ResolvedOperand::Aggregation(_) | ResolvedOperand::Calculation(_)
            );
            if is_agg_or_calc {
                let cond = NestMatchCondition {
                    nvalue: left_nvalue.clone(),
                    op: NestMatchOp::Comparison(*cmp_op),
                    right: right_nvalue.clone(),
                    context: left_context.clone(),
                };
                return build_merged_nest_match_sql(left_keys, &[cond], false, view);
            }

            // right_nvalue が Literal の場合: プロジェクション同士の比較
            let mut stmt = build_resolved_projection_sql(&left_keys[0], view);
            let sub_l = build_nvalue_standalone_subquery(
                &left_keys[0], left_nvalue, left_context.as_deref(), view,
                true, // include_item_id: 不同キー同士の結合には item_id が必要
            );
            let sub_r = build_nvalue_standalone_subquery(
                &right_keys[0], right_nvalue, right_context.as_deref(), view,
                true,
            );
            let bin_op = to_bin_op(*cmp_op);
            let join_sql = Query::select()
                .column((Alias::new("L"), Alias::new("group_label")))
                .from_subquery(sub_l, Alias::new("L"))
                .join_subquery(
                    sea_query::JoinType::InnerJoin,
                    sub_r,
                    Alias::new("R"),
                    Expr::col((Alias::new("L"), Col::ItemId))
                        .eq(Expr::col((Alias::new("R"), Col::ItemId))),
                )
                .and_where(
                    Expr::col((Alias::new("L"), Alias::new("nvalue"))).binary(
                        bin_op,
                        Expr::col((Alias::new("R"), Alias::new("nvalue"))),
                    ),
                )
                .to_owned();
            let proj_col = match left_keys[0].get_storage() {
                Some(StorageMapping::RowTag { column, .. }) => column,
                Some(StorageMapping::Column(col)) => col,
                _ => panic!("unexpected NestNestMatch with non-TagRef keys: {:?}", left_keys),
            };
            stmt.and_where(Expr::col(*proj_col).in_subquery(join_sql));
            stmt
        }
    }
}

fn build_scalar_match_sql(
    left: &Label,
    op: ComparisonOp,
    right: &Label,
    view: &str,
) -> SelectStatement {
    // リテラル同士のスカラー比較: is_boolean_result() 経由で
    // build_boolean_sql が使われるため通常ここには到達しないが、
    // wrap_boolean_collider 等から呼ばれる場合に備える。
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));
    stmt.column(Col::ItemId);
    let cond = Expr::expr(label_to_unit_aware_expr(left))
        .binary(to_bin_op(op), label_to_unit_aware_expr(right));
    stmt.cond_where(cond);
    stmt.limit(1);
    stmt
}

fn build_label_set_op_pick_sql(
    op: &LabelSetOpKind,
    operands: &[ResolvedNode],
    view: &str,
) -> SelectStatement {
    // LabelSetOp がフィルタコンテキスト（Nest の右辺等）で呼ばれる場合:
    // item-level の集合演算として処理する（ラベル値集合演算ではなくアイテム ID 集合）
    match op {
        LabelSetOpKind::Union => build_resolved_or_sql(operands, view),
        LabelSetOpKind::Intersect => build_resolved_and_sql(operands, view),
        LabelSetOpKind::Except => {
            if let (Some(left), Some(right)) = (operands.first(), operands.get(1)) {
                build_resolved_diff_sql(left, right, view)
            } else {
                Query::select().to_owned()
            }
        }
    }
}
