use crate::db::{Col, QueryResultCol, SqlType};
use crate::query::ast::{ArithmeticAggOp, ComparisonOp};
use crate::query::lens_resolver::{
    LabelSetOpKind, NestMatchCondition, NestMatchOp, ResolvedAggregationNode,
    ResolvedCalculationNode, ResolvedNode, ResolvedOperand,
};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{ItemKind, Label, SType, TagType};
use sea_query::{Alias, BinOper, Condition, Expr, Func, IntoIden, Query, SelectStatement, SimpleExpr};
use super::{
    apply_arithmetic_agg, apply_arithmetic_op, agg_expr,
    build_calculation_eav_expr, build_calculation_expr,
    build_calculation_subquery, build_nest_pivot_cte, build_nvalue_standalone_subquery,
    build_agg, build_resolved_literal_expr, build_resolved_operand_eav_expr,
    build_storage_column_expr, build_tag_value_agg_expr,
    label_to_simple_expr, label_to_unit_aware_expr, subquery, wrap_in_subquery,
};

/// CalculationNodeに含まれるRowTagのtypeフィルタをWHERE句に追加します。
fn add_type_filters(
    stmt: &mut SelectStatement,
    calc: &ResolvedCalculationNode,
) {
    for op in calc.left.walk().into_iter().chain(calc.right.walk()) {
        if let ResolvedOperand::TagRef {
            storage: StorageMapping::RowTag { tag_type, .. },
            ..
        } = op
        {
            stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
        }
    }
}

/// 物理マッピング解決済みの構造から SQL を生成します。
pub fn build_pick_sql(node: &ResolvedNode, view: &str) -> SelectStatement {
    node.fold(&|node, child_sqls: Vec<SelectStatement>| {
        match node {
            // ── 集合演算（子SQLを fold で受け取る）────────────────────────
            ResolvedNode::And(_) => build_resolved_and_sql(child_sqls, view),
            ResolvedNode::Or(_) => build_resolved_or_sql(child_sqls, view),
            ResolvedNode::Difference(_, _) => {
                let [l, r]: [SelectStatement; 2] = child_sqls.try_into().unwrap();
                build_resolved_diff_sql(l, r)
            }
            ResolvedNode::Complement(c) => {
                let [c_sql]: [SelectStatement; 1] = child_sqls.try_into().unwrap();
                build_resolved_comp_sql(c.is_boolean_result(), c_sql, view)
            }
            ResolvedNode::LabelSetOp { op, .. } => {
                build_label_set_op_pick_sql(op, child_sqls)
            }
            // ── Nest（context SQL が child_sqls[0] に入る）───────────────
            ResolvedNode::Nest { keys, .. } => {
                build_nest_sql(keys, child_sqls.into_iter().next(), view)
            }
            // ── 葉ノード（child_sqls は常に空）───────────────────────────
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
        }
    })
}

fn build_nest_sql(
    keys: &[ResolvedOperand],
    ctx_sql: Option<SelectStatement>,
    view: &str,
) -> SelectStatement {
    let mut stmt = build_resolved_projection_sql(keys.first().unwrap(), view);
    for key in keys.iter().skip(1) {
        let key_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(build_resolved_projection_sql(key, view), Alias::new("_key"))
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(key_sub));
    }
    if let Some(ctx) = ctx_sql {
        let ctx_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(ctx, Alias::new("_ctx"))
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


fn build_resolved_tag_tag_match_sql(
    left_storage: &StorageMapping,
    left_sql_type: SqlType,
    op: ComparisonOp,
    right_storage: &StorageMapping,
    right_sql_type: SqlType,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.column(Col::ItemId)
        .from(Alias::new(view))
        .group_by_col(Col::ItemId);
    let left_expr = build_tag_value_agg_expr(left_storage, left_sql_type);
    let right_expr = build_tag_value_agg_expr(right_storage, right_sql_type);
    q.and_having(left_expr.binary(to_bin_op(op), right_expr));
    q
}

/// EAV 構造における Tag vs Calculation 比較用の SQL を生成します。
fn build_tag_calc_match_eav_sql(
    left_storage: &StorageMapping,
    left_sql_type: SqlType,
    op: ComparisonOp,
    calc: &ResolvedCalculationNode,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.column(Col::ItemId)
        .from(Alias::new(view))
        .group_by_col(Col::ItemId);
    let left_expr = build_tag_value_agg_expr(left_storage, left_sql_type);
    let calc_expr = build_calculation_eav_expr(calc, view);
    q.and_having(left_expr.binary(to_bin_op(op), calc_expr));
    q
}

fn build_resolved_operand_eav_row_expr(
    operand: &ResolvedOperand,
    view: &str,
) -> SimpleExpr {
    operand.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef { storage, sql_type, .. } => {
            build_tag_value_eav_row_expr(storage, *sql_type)
        }
        ResolvedOperand::Calculation(calc) => {
            let [left, right]: [SimpleExpr; 2] = child_results.try_into().unwrap();
            let is_string = calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(agg) => agg_expr(agg, view),
    })
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
    child_sqls: Vec<SelectStatement>,
) -> SelectStatement {
    // LabelSetOp がフィルタコンテキスト（Nest の右辺等）で呼ばれる場合:
    // item-level の集合演算として処理する（ラベル値集合演算ではなくアイテム ID 集合）
    match op {
        LabelSetOpKind::Union => {
            reduce_with_union(child_sqls, sea_query::UnionType::Distinct, Query::select().to_owned())
        }
        LabelSetOpKind::Intersect => {
            reduce_with_union(child_sqls, sea_query::UnionType::Intersect, Query::select().to_owned())
        }
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

// ========== Moved from mod.rs ==========

fn get_required_row_tags(node: &ResolvedNode) -> Vec<String> {
    node.walk().into_iter().filter_map(|n| {
        if let ResolvedNode::Match {
            storage: StorageMapping::RowTag { tag_type, .. }, ..
        } = n {
            Some(tag_type.clone())
        } else {
            None
        }
    }).collect()
}

fn resolve_simple_filter_condition(
    node: &ResolvedNode,
    table: Alias,
) -> Option<Condition> {
    node.fold(&|n, child_results: Vec<Option<Condition>>| match n {
        ResolvedNode::Match { storage, label, .. } => {
            if let StorageMapping::RowTag { tag_type, .. } = storage {
                let s_val = label.as_str();
                let mut cond = Condition::all();
                if tag_type.as_str() != "*" {
                    cond = cond.add(
                        Expr::col((table.clone(), Col::Type)).eq(tag_type.as_str()),
                    );
                }
                if s_val != "*" && s_val != "" {
                    cond = cond.add(Expr::col((table.clone(), Col::LabelStr)).eq(s_val));
                }
                Some(cond)
            } else {
                None
            }
        }
        ResolvedNode::ColumnMatch { tag, label } => {
            let s_val = label.as_str();
            Some(Condition::all().add(Expr::col((table.clone(), *tag)).eq(s_val)))
        }
        ResolvedNode::And(_) => {
            let mut required_tags = get_required_row_tags(n);
            required_tags.sort();
            required_tags.dedup();
            if required_tags.len() > 1 {
                return None;
            }
            child_results
                .into_iter()
                .try_fold(Condition::all(), |acc, c| c.map(|cond| acc.add(cond)))
        }
        _ => None,
    })
}

fn build_merged_nvalue_agg_expr(
    nvalue: &ResolvedOperand,
    tbl_alias: &str,
    view: &str,
) -> SimpleExpr {
    nvalue.fold(&|op, child_results: Vec<SimpleExpr>| match op {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            let (inner_tag, inner_filter, _) = inner.extract_agg_parts();
            if let Some(filter_node) = inner_filter {
                let case_expr: SimpleExpr = if let Some(cond) =
                    resolve_simple_filter_condition(&filter_node, Alias::new(tbl_alias))
                {
                    if cond.is_empty() {
                        Expr::col((Alias::new(tbl_alias), Col::ItemId)).into()
                    } else {
                        Expr::case(cond, Expr::col((Alias::new(tbl_alias), Col::ItemId)))
                            .finally(Expr::val(None::<i32>))
                            .into()
                    }
                } else {
                    let filter_sub = Query::select()
                        .column(Col::ItemId)
                        .from_subquery(build_pick_sql(&filter_node, view), Alias::new("nv_filter"))
                        .to_owned();
                    let in_expr = Expr::col((Alias::new(tbl_alias), Col::ItemId))
                        .in_subquery(filter_sub);
                    Expr::case(in_expr, Expr::col((Alias::new(tbl_alias), Col::ItemId)))
                        .finally(Expr::val(None::<i32>))
                        .into()
                };
                Expr::expr(case_expr).count_distinct().into()
            } else if let Some(StorageMapping::RowTag { tag_type, .. }) = inner_tag {
                let cond = Condition::all().add(
                    Expr::col((Alias::new(tbl_alias), Col::Type)).eq(tag_type.as_str()),
                );
                let case_expr = Expr::case(cond, Expr::col((Alias::new(tbl_alias), Col::ItemId)))
                    .finally(Expr::val(None::<i32>));
                Expr::expr(case_expr).count_distinct().into()
            } else {
                Expr::col((Alias::new(tbl_alias), Col::ItemId)).count_distinct().into()
            }
        }
        ResolvedOperand::Aggregation(agg @ ResolvedAggregationNode::Arithmetic { op, inner }) => {
            let is_string = agg.is_string_type();
            let (inner_tag, inner_filter, _operand) = inner.extract_agg_parts();

            let val_expr: SimpleExpr = if is_string {
                Expr::col((Alias::new(tbl_alias), Col::LabelStr)).into()
            } else {
                Expr::cust_with_exprs(
                    "COALESCE($1, $2, TRY_CAST($3 AS DOUBLE))",
                    [
                        Expr::col((Alias::new(tbl_alias), Col::LabelInt)).into(),
                        Expr::col((Alias::new(tbl_alias), Col::LabelDouble)).into(),
                        Expr::col((Alias::new(tbl_alias), Col::LabelStr)).into(),
                    ],
                )
            };

            if let Some(filter_node) = inner_filter {
                let filter_sub = Query::select()
                    .column(Col::ItemId)
                    .from_subquery(build_pick_sql(&filter_node, view), Alias::new("nv_filter"))
                    .to_owned();
                let in_expr =
                    Expr::col((Alias::new(tbl_alias), Col::ItemId)).in_subquery(filter_sub);
                let case_expr: SimpleExpr =
                    if let Some(StorageMapping::RowTag { tag_type, .. }) = inner_tag {
                        let combined_cond = Condition::all()
                            .add(Expr::col((Alias::new(tbl_alias), Col::Type)).eq(tag_type.as_str()))
                            .add(in_expr);
                        Expr::case(combined_cond, val_expr.clone())
                            .finally(Expr::val(None::<f64>))
                            .into()
                    } else {
                        Expr::case(in_expr, val_expr.clone())
                            .finally(Expr::val(None::<f64>))
                            .into()
                    };
                apply_arithmetic_agg(op, case_expr, is_string)
            } else if let Some(StorageMapping::RowTag { tag_type, .. }) = inner_tag {
                let cond = Condition::all().add(
                    Expr::col((Alias::new(tbl_alias), Col::Type)).eq(tag_type.as_str()),
                );
                let case_expr = Expr::case(cond, val_expr).finally(Expr::val(None::<f64>));
                apply_arithmetic_agg(op, Expr::expr(case_expr).into(), is_string)
            } else {
                apply_arithmetic_agg(op, Expr::expr(val_expr).into(), is_string)
            }
        }
        ResolvedOperand::Calculation(calc) => {
            let [left_expr, right_expr]: [SimpleExpr; 2] = child_results.try_into().unwrap();
            let left_val = Expr::expr(left_expr).cast_as(crate::db::SqlType::DOUBLE).into();
            let right_val = Expr::expr(right_expr).cast_as(crate::db::SqlType::DOUBLE).into();
            apply_arithmetic_op(&calc.op, left_val, right_val, false)
        }
        ResolvedOperand::Literal(l) => label_to_simple_expr(l),
        ResolvedOperand::TagRef { .. } => Expr::val(1i32).into(),
    })
}

/// EAV 構造において、特定のタグの値を（集計せずに）行レベルで取得する式を構築します。
fn build_tag_value_eav_row_expr(
    storage: &StorageMapping,
    sql_type: crate::db::SqlType,
) -> SimpleExpr {
    match storage {
        StorageMapping::RowTag { tag_type, .. } => {
            let col_expr = build_storage_column_expr(storage, sql_type);
            Expr::case(Expr::col(Col::Type).eq(tag_type.as_str()), col_expr)
                .finally(Expr::val(None::<f64>))
                .into()
        }
        StorageMapping::Column(col) => Expr::col(*col).into(),
        StorageMapping::Virtual => Expr::val(None::<f64>).into(),
    }
}

/// `child_sqls` を `union_type` で結合します。空の場合は `empty_fallback` を返します。
fn reduce_with_union(
    child_sqls: Vec<SelectStatement>,
    union_type: sea_query::UnionType,
    empty_fallback: SelectStatement,
) -> SelectStatement {
    child_sqls.into_iter()
        .map(wrap_in_subquery)
        .reduce(|mut acc, next| { acc.union(union_type, next); acc })
        .unwrap_or(empty_fallback)
}

fn build_resolved_and_sql(
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

fn build_resolved_or_sql(
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

fn build_resolved_diff_sql(
    l: SelectStatement,
    r: SelectStatement,
) -> SelectStatement {
    let mut q = wrap_in_subquery(l);
    q.union(sea_query::UnionType::Except, wrap_in_subquery(r));
    q
}

fn build_resolved_comp_sql(
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
        .from_subquery(c_sql, crate::db::Tbl::NotSide);
    q.union(sea_query::UnionType::Except, eq);
    q
}

fn build_resolved_match_sql(
    storage: &StorageMapping,
    sql_type: crate::db::SqlType,
    op: ComparisonOp,
    label: &Label,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));

    q.cond_where(storage.to_condition(op, label, sql_type));
    q
}

fn build_column_match_sql(
    tag: SType,
    label: &Label,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));

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
    }
    q
}

fn build_resolved_projection_sql(
    op: &ResolvedOperand,
    view: &str,
) -> SelectStatement {
    op.fold(&|op, child_results: Vec<SelectStatement>| match op {
        ResolvedOperand::TagRef { tag_type, .. } => {
            let mut q = Query::select();
            q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
                .distinct()
                .from(Alias::new(view));
            let cond = ResolvedNode::Nest {
                keys: vec![op.clone()],
                nvalue: None,
                context: None,
            }
            .to_condition();
            q.cond_where(cond);
            if let TagType::Base(SType::TypedTag) = tag_type {
                q.and_where(Expr::col(Col::TypedTag).is_not_null());
            } else if let TagType::Base(SType::Origin) = tag_type {
                q.and_where(Expr::col(Col::Origin).is_not_null());
            }
            q
        }
        ResolvedOperand::Calculation(_) => {
            let [mut l, r]: [SelectStatement; 2] = child_results.try_into().unwrap();
            l.union(sea_query::UnionType::Intersect, r);
            l
        }
        _ => {
            Query::select()
                .columns([Col::ItemId, Col::Rank, Col::ItemKind])
                .distinct()
                .from(Alias::new(view))
                .to_owned()
        }
    })
}

fn build_merged_nest_match_sql(
    keys: &[ResolvedOperand],
    matches: &[NestMatchCondition],
    is_or: bool,
    view: &str,
) -> SelectStatement {
    if keys.len() == 1 {
        let (proj_col, proj_tag_type) = match keys[0].get_storage() {
            Some(StorageMapping::RowTag { column, tag_type }) => {
                (*column, Some(tag_type.as_str()))
            }
            Some(StorageMapping::Column(col)) => (*col, None),
            _ => {
                panic!("MergedNestMatch key must have RowTag or Column storage")
            }
        };

        let mut nfilter = Query::select();
        nfilter.expr_as(
            Expr::col((Alias::new("proj"), proj_col)),
            Alias::new("group_label"),
        );
        nfilter.from_as(Alias::new(view), Alias::new("proj"));
        nfilter.join_as(
            sea_query::JoinType::InnerJoin,
            Alias::new(view),
            Alias::new("c"),
            Expr::col((Alias::new("proj"), Col::ItemId))
                .equals((Alias::new("c"), Col::ItemId)),
        );
        if let Some(tag_type) = proj_tag_type {
            nfilter.and_where(
                Expr::col((Alias::new("proj"), Col::Type)).eq(tag_type),
            );
        }
        nfilter.group_by_col((Alias::new("proj"), proj_col));

        let mut having_cond = if is_or {
            Condition::any()
        } else {
            Condition::all()
        };
        for m in matches {
            let NestMatchOp::Comparison(cmp_op) = m.op;
            let bin_op = to_bin_op(cmp_op);
            let lhs = build_merged_nvalue_agg_expr(&m.nvalue, "c", view);
            let rhs = build_merged_nvalue_agg_expr(&m.right, "c", view);
            having_cond = having_cond.add(Expr::expr(lhs).binary(bin_op, rhs));
        }
        nfilter.cond_having(having_cond);

        let mut group_label_sub = Query::select();
        group_label_sub.column(Alias::new("group_label"));
        group_label_sub.from_subquery(nfilter, Alias::new("nfilter"));

        let mut stmt = Query::select();
        stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        stmt.distinct();
        stmt.from(Alias::new(view));
        if let Some(tag_type) = proj_tag_type {
            stmt.and_where(Expr::col(Col::Type).eq(tag_type));
        }
        stmt.and_where(Expr::col(proj_col).in_subquery(group_label_sub));
        stmt
    } else {
        let mut all_nv_ops: Vec<&ResolvedOperand> = Vec::new();
        for m in matches {
            if !all_nv_ops.iter().any(|&o| o == &m.nvalue) {
                all_nv_ops.push(&m.nvalue);
            }
            if let ResolvedOperand::Aggregation(_) = &m.right {
                if !all_nv_ops.iter().any(|&o| o == &m.right) {
                    all_nv_ops.push(&m.right);
                }
            }
        }

        let pivot_sub = build_nest_pivot_multi_nv_cte(keys, &all_nv_ops, view);
        let pivot_alias = Alias::new("pivot_merged");

        let mut stmt = Query::select();
        stmt.column(Col::ItemId);
        stmt.column(Alias::new("rank"));
        stmt.column(Alias::new("item_kind"));

        let mut partition_keys: Vec<SimpleExpr> = Vec::new();
        for i in 0..keys.len() {
            partition_keys
                .push(Expr::col(Alias::new(&format!("key{}", i))).into());
        }

        use sea_query::{
            OverStatement, SelectExpr, WindowSelectType, WindowStatement,
        };

        let mut group_nv_aliases = Vec::new();
        for (idx, op) in all_nv_ops.iter().enumerate() {
            let nval_pivot_alias = format!("nv{}", idx);
            let group_nv_alias = format!("group_nv_{}", idx);

            let agg_func = match op {
                ResolvedOperand::Aggregation(
                    ResolvedAggregationNode::Count(_),
                ) => Func::sum(Expr::col(Alias::new(&nval_pivot_alias))),
                ResolvedOperand::Aggregation(
                    ResolvedAggregationNode::Arithmetic { op, .. },
                ) => match op {
                    ArithmeticAggOp::Sum => {
                        Func::sum(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    ArithmeticAggOp::Avg => {
                        Func::avg(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    ArithmeticAggOp::Max => {
                        Func::max(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    ArithmeticAggOp::Min => {
                        Func::min(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                },
                _ => Func::cust(Alias::new("MAX"))
                    .arg(Expr::col(Alias::new(&nval_pivot_alias))),
            };

            let mut window = WindowStatement::new();
            for pk in &partition_keys {
                window.add_partition_by(pk.clone());
            }

            stmt.expr(SelectExpr {
                expr: agg_func.into(),
                alias: Some(Alias::new(&group_nv_alias).into_iden()),
                window: Some(WindowSelectType::Query(window)),
            });
            group_nv_aliases.push(group_nv_alias);
        }

        let mut filter_cond = if is_or {
            Condition::any()
        } else {
            Condition::all()
        };

        for m in matches {
            let left_idx =
                all_nv_ops.iter().position(|&o| o == &m.nvalue).unwrap();
            let left_group_nv = &group_nv_aliases[left_idx];

            let NestMatchOp::Comparison(cmp_op) = m.op;
            let bin_op = to_bin_op(cmp_op);

            let right_expr =
                if let ResolvedOperand::Aggregation(_) = &m.right {
                    let right_idx = all_nv_ops
                        .iter()
                        .position(|&o| o == &m.right)
                        .unwrap();
                    Expr::col(Alias::new(&group_nv_aliases[right_idx])).into()
                } else {
                    build_resolved_operand_eav_expr(&m.right, view)
                };

            filter_cond = filter_cond.add(
                Expr::col(Alias::new(left_group_nv)).binary(bin_op, right_expr),
            );
        }

        stmt.from_subquery(pivot_sub, pivot_alias);

        let mut final_stmt = Query::select();
        final_stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
        final_stmt.distinct();
        final_stmt.from_subquery(stmt, Alias::new("merged_items"));
        final_stmt.and_where(filter_cond.into());
        final_stmt
    }
}

fn build_nest_pivot_multi_nv_cte(
    keys: &[ResolvedOperand],
    nvalues: &[&ResolvedOperand],
    view: &str,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.column(Col::ItemId);
    stmt.expr_as(
        crate::db::CustomFunc::any_value(Expr::col(Col::Rank)),
        Alias::new("rank"),
    );
    stmt.expr_as(
        crate::db::CustomFunc::any_value(Expr::col(Col::ItemKind)),
        Alias::new("item_kind"),
    );
    stmt.from(Alias::new(view));

    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef { storage, .. } => match storage {
                StorageMapping::RowTag { tag_type, column } => {
                    let case_expr = Expr::case(
                        Expr::col(Col::Type).eq(tag_type.as_str()),
                        Expr::col(*column),
                    );
                    stmt.expr_as(
                        Expr::cust_with_exprs("MAX($1)", [case_expr.into()]),
                        Alias::new(&format!("key{}", i)),
                    );
                }
                StorageMapping::Column(col) => {
                    stmt.expr_as(
                        Expr::col(*col).max(),
                        Alias::new(&format!("key{}", i)),
                    );
                }
                _ => {}
            },
            ResolvedOperand::Calculation(calc) => {
                let calc_expr = build_calculation_eav_expr(calc, view);
                stmt.expr_as(calc_expr, Alias::new(&format!("key{}", i)));
            }
            _ => {}
        }
    }

    for (i, nv) in nvalues.iter().enumerate() {
        let nv_expr = build_resolved_operand_eav_expr(nv, view);
        stmt.expr_as(nv_expr, Alias::new(&format!("nv{}", i)));
    }

    stmt.group_by_col(Col::ItemId);
    stmt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{BasicOp, ComparisonOp};
    use crate::query::lens_resolver::ResolvedNode;
    use crate::query::lens_schema::StorageMapping;
    use crate::types::{Label, LabelValue, TagType};

    #[test]
    fn test_resolve_simple_filter_condition_multiple_row_tags() {
        let node1 = ResolvedNode::Match {
            tag_type: TagType::from("extension"),
            storage: StorageMapping::RowTag {
                column: crate::db::Col::LabelStr,
                tag_type: "extension".to_string(),
            },
            sql_type: crate::db::SqlType::VARCHAR,
            op: ComparisonOp::Label(BasicOp::Eq),
            label: Label::resolve(
                TagType::from("extension"),
                LabelValue::String("jpg".to_string()),
            ),
        };

        let node2 = ResolvedNode::Match {
            tag_type: TagType::from("is_dir"),
            storage: StorageMapping::RowTag {
                column: crate::db::Col::LabelBool,
                tag_type: "is_dir".to_string(),
            },
            sql_type: crate::db::SqlType::BOOLEAN,
            op: ComparisonOp::Label(BasicOp::Eq),
            label: Label::resolve(
                TagType::from("is_dir"),
                LabelValue::Boolean(false),
            ),
        };

        let and_node = ResolvedNode::And(vec![node1, node2]);

        let result =
            resolve_simple_filter_condition(&and_node, Alias::new("tbl"));
        assert!(
            result.is_none(),
            "Multiple different RowTags in AND should return None to avoid impossible EAV conditions"
        );
    }
}
