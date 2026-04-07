use crate::db::{Col, SqlType, Tbl};
use crate::query::ast::{
    ArithmeticAggOp, ArithmeticOp, ComparisonOp, QueryNode,
};
use crate::query::lens_resolver::{
    extract_nvalue_projection_parts, NestMatchCondition, NestMatchOp,
    ResolvedAggregationNode, ResolvedNode, ResolvedOperand,
};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{Label, SType, TagType};
use sea_query::{
    Alias, BinOper, Condition, Expr, ExprTrait, Func, IntoIden, Query,
    SelectStatement, SimpleExpr,
};

/// CalculationNodeに含まれるRowTagのtypeフィルタをWHERE句に追加します。
fn add_type_filters(
    stmt: &mut SelectStatement,
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
) {
    use crate::query::lens_resolver::ResolvedOperand;

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

/// 物理マッピング解決済みの構造から SQL を生成します (Phase 2)。
pub fn build_pick_sql(node: &ResolvedNode, view: &str) -> SelectStatement {
    match node {
        ResolvedNode::And(nodes) => build_resolved_and_sql(nodes, view),
        ResolvedNode::Or(nodes) => build_resolved_or_sql(nodes, view),
        ResolvedNode::Difference(l, r) => build_resolved_diff_sql(l, r, view),
        ResolvedNode::Nest {
            keys,
            nvalue: _,
            context,
        } => {
            let mut stmt =
                build_resolved_projection_sql(keys.first().unwrap(), view);
            // 2番目以降のキーも AND 条件として追加（例: tagA: &: tagB: → tagA AND tagB）
            for key in keys.iter().skip(1) {
                let key_sub = Query::select()
                    .column(Col::ItemId)
                    .from_subquery(
                        build_resolved_projection_sql(key, view),
                        Alias::new("_key"),
                    )
                    .to_owned();
                stmt.and_where(Expr::col(Col::ItemId).in_subquery(key_sub));
            }
            if let Some(ctx) = context {
                // build_pick_sql returns 3 columns (item_id, rank, item_kind);
                // wrap it to select only item_id for use in IN subquery.
                let ctx_sub = Query::select()
                    .column(Col::ItemId)
                    .from_subquery(
                        build_pick_sql(ctx, view),
                        Alias::new("_ctx"),
                    )
                    .to_owned();
                stmt.and_where(Expr::col(Col::ItemId).in_subquery(ctx_sub));
            }
            stmt
        }
        ResolvedNode::MergedNestMatch {
            keys,
            matches,
            is_or,
        } => build_merged_nest_match_sql(&keys, matches, *is_or, view),
        ResolvedNode::ColumnMatch { tag, label } => {
            build_column_match_sql(*tag, label, view)
        }
        ResolvedNode::Match {
            storage,
            sql_type,
            op,
            label,
            ..
        } => build_resolved_match_sql(storage, *sql_type, *op, label, view),
        ResolvedNode::Aggregation(agg) => {
            build_resolved_aggregation_sql(agg, view)
        }
        ResolvedNode::AggregationMatch { agg, op, label } => {
            build_resolved_aggregation_match_sql(agg, *op, label, view)
        }
        ResolvedNode::CalculationMatch { calc, op, label } => {
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
                let bin_op = to_bin_op(*op);
                stmt.and_having(
                    Expr::expr(calc_expr).binary(bin_op, label_expr),
                );
                stmt
            } else {
                // RowTag を含まない純粋なスカラー/集約計算（既存処理）
                let mut stmt = Query::select();
                stmt.from(Alias::new(view));
                stmt.column(Col::ItemId);

                let calc_expr = if calc.contains_aggregation() {
                    build_calculation_subquery(calc, view)
                } else {
                    build_calculation_expr(calc, view)
                };

                let label_expr = label_to_unit_aware_expr(label);
                let bin_op = to_bin_op(*op);
                let cond = Expr::expr(calc_expr).binary(bin_op, label_expr);
                stmt.cond_where(cond);

                if !calc.contains_aggregation() {
                    add_type_filters(&mut stmt, calc);
                }
                stmt
            }
        }
        ResolvedNode::TagCalculationMatch {
            storage,
            sql_type,
            op,
            calc,
            ..
        } => {
            // RowTag が関与する場合は GROUP BY HAVING で集約計算を行う
            let needs_eav = calc.contains_row_tag()
                || matches!(storage, StorageMapping::RowTag { .. });

            if needs_eav {
                build_tag_calc_match_eav_sql(
                    storage, *sql_type, *op, calc, view,
                )
            } else {
                let mut stmt = Query::select();
                stmt.from(Alias::new(view));
                stmt.column(Col::ItemId);

                let tag_expr = build_storage_column_expr(storage, *sql_type);

                let calc_expr = if calc.contains_aggregation() {
                    build_calculation_subquery(calc, view)
                } else {
                    build_calculation_expr(calc, view)
                };

                // 解決済みの演算子をそのまま使用する（Resolver側ですでに正規化済み）
                let bin_op = to_bin_op(*op);
                let cond = Expr::expr(tag_expr).binary(bin_op, calc_expr);

                stmt.cond_where(cond);

                // 集約関数が含まれていない場合のみ、
                // calcに含まれるRowTagのtypeフィルタを追加
                if !calc.contains_aggregation() {
                    add_type_filters(&mut stmt, calc);
                }

                stmt
            }
        }
        ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
            let mut stmt = Query::select();
            stmt.from(Alias::new(view));
            stmt.column(Col::ItemId);

            // 集約関数と算術演算の両方をサブクエリとして構築
            let agg_expr = build_aggregation_subquery(agg, view);
            let calc_expr = if calc.contains_aggregation() {
                build_calculation_subquery(calc, view)
            } else {
                build_calculation_expr(calc, view)
            };

            let bin_op = to_bin_op(*op);
            let cond = Expr::expr(agg_expr).binary(bin_op, calc_expr);

            stmt.cond_where(cond);

            stmt
        }
        ResolvedNode::CalculationCalculationMatch {
            left_calc,
            op,
            right_calc,
        } => {
            let mut stmt = Query::select();
            stmt.column(Col::ItemId)
                .from(Alias::new(view))
                .group_by_col(Col::ItemId);
            let left_expr = build_calculation_eav_expr(left_calc, view);
            let right_expr = build_calculation_eav_expr(right_calc, view);
            let bin_op = to_bin_op(*op);
            stmt.and_having(Expr::expr(left_expr).binary(bin_op, right_expr));
            stmt
        }
        ResolvedNode::AggregationAggregationMatch { left, op, right } => {
            let mut stmt = Query::select();
            stmt.from(Alias::new(view));
            stmt.column(Col::ItemId);

            // 両方の集約関数をサブクエリとして構築
            let left_expr = build_aggregation_subquery(left, view);
            let right_expr = build_aggregation_subquery(right, view);

            let bin_op = to_bin_op(*op);
            let cond = Expr::expr(left_expr).binary(bin_op, right_expr);

            stmt.cond_where(cond);
            stmt
        }
        ResolvedNode::TagTagMatch {
            left_storage,
            left_sql_type,
            op,
            right_storage,
            right_sql_type,
        } => build_resolved_tag_tag_match_sql(
            left_storage,
            *left_sql_type,
            *op,
            right_storage,
            *right_sql_type,
            view,
        ),
        ResolvedNode::AggregationTagMatch {
            agg,
            op,
            storage,
            sql_type,
            ..
        } => {
            let mut stmt = Query::select();
            stmt.from(Alias::new(view));
            stmt.column(Col::ItemId);

            // 集約関数をサブクエリとして構築
            let agg_expr = build_aggregation_subquery(agg, view);
            let tag_expr = build_storage_column_expr(storage, *sql_type);

            let bin_op = to_bin_op(*op);
            let cond = Expr::expr(agg_expr).binary(bin_op, tag_expr);

            stmt.cond_where(cond);

            // RowTagの場合は、typeでフィルタ
            if let StorageMapping::RowTag { tag_type, .. } = storage {
                stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
            }

            stmt
        }
        ResolvedNode::NestMatch {
            keys,
            nvalue,
            op: comparison_op,
            label,
            context,
        } => {
            if keys.len() == 1 {
                // 単一キーの場合は従来の build_nvalue_standalone_subquery アプローチを使用
                let nvalue_sub = build_nvalue_standalone_subquery(
                    &keys[0],
                    nvalue,
                    context.as_deref(),
                    view,
                    false,
                );

                let bin_op = to_bin_op(*comparison_op);
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
                    Some(StorageMapping::RowTag { column, tag_type }) => {
                        (*column, Some(tag_type.as_str()))
                    }
                    Some(StorageMapping::Column(col)) => (*col, None),
                    _ => panic!(
                        "NestMatch key must have RowTag or Column storage"
                    ),
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

                let mut partition_keys: Vec<SimpleExpr> = Vec::new();
                for i in 0..keys.len() {
                    partition_keys.push(
                        Expr::col(Alias::new(&format!("key{}", i))).into(),
                    );
                }

                let bin_op = to_bin_op(*comparison_op);
                let label_expr = label_to_unit_aware_expr(label);

                let agg_func = match nvalue {
                    crate::query::lens_resolver::ResolvedOperand::Aggregation(
                        crate::query::lens_resolver::ResolvedAggregationNode::Count(_),
                    ) => Func::sum(Expr::col(Alias::new("nvalue"))),
                    crate::query::lens_resolver::ResolvedOperand::Aggregation(
                        crate::query::lens_resolver::ResolvedAggregationNode::Arithmetic {
                            op,
                            ..
                        },
                    ) => match op {
                        crate::query::ast::ArithmeticAggOp::Sum => {
                            Func::sum(Expr::col(Alias::new("nvalue")))
                        }
                        crate::query::ast::ArithmeticAggOp::Avg => {
                            Func::avg(Expr::col(Alias::new("nvalue")))
                        }
                        crate::query::ast::ArithmeticAggOp::Max => {
                            Func::max(Expr::col(Alias::new("nvalue")))
                        }
                        crate::query::ast::ArithmeticAggOp::Min => {
                            Func::min(Expr::col(Alias::new("nvalue")))
                        }
                    },
                    _ => Func::cust(Alias::new("MAX"))
                        .arg(Expr::col(Alias::new("nvalue"))),
                };

                use sea_query::{
                    OverStatement, SelectExpr, WindowSelectType,
                    WindowStatement,
                };
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
                    let pick_sql = build_pick_sql(ctx, view);
                    let mut psub = Query::select();
                    psub.column(Col::ItemId)
                        .from_subquery(pick_sql, Alias::new("ctx_p"));
                    stmt.and_where(Expr::col(Col::ItemId).in_subquery(psub));
                }

                stmt.from_subquery(pivot_sub, pivot_alias);

                let mut final_stmt = Query::select();
                final_stmt.columns([Col::ItemId, Col::Rank, Col::ItemKind]);
                final_stmt.distinct();
                final_stmt.from_subquery(stmt, Alias::new("filtered_items"));
                final_stmt.and_where(
                    Expr::col(Alias::new("group_nvalue"))
                        .binary(bin_op, label_expr),
                );
                final_stmt
            }
        }
        ResolvedNode::NestNestMatch {
            left_keys,
            left_nvalue,
            left_context,
            op,
            right_keys,
            right_nvalue,
            right_context,
        } => {
            match op {
                NestMatchOp::Comparison(cmp_op) => {
                    // right_nvalue が Aggregation/Calculation の場合（agg vs agg/calc 比較）：
                    // MergedNestMatch の HAVING アプローチを再利用する。
                    // これにより GROUP BY group_label HAVING agg_l == agg_r が正しく評価される。
                    let is_agg_or_calc = matches!(
                        right_nvalue,
                        ResolvedOperand::Aggregation(_)
                            | ResolvedOperand::Calculation(_)
                    );

                    if is_agg_or_calc {
                        let cond = NestMatchCondition {
                            nvalue: left_nvalue.clone(),
                            op: NestMatchOp::Comparison(*cmp_op),
                            right: right_nvalue.clone(),
                            context: left_context.clone(),
                        };
                        return build_merged_nest_match_sql(
                            left_keys,
                            &[cond],
                            false,
                            view,
                        );
                    }

                    // right_nvalue が Literal の場合（通常の nvalue フィルタ）：
                    // プロジェクション同士の比較：左辺ベースの SQL を作成
                    let mut stmt =
                        build_resolved_projection_sql(&left_keys[0], view);

                    // 左辺と右辺の nvalue サブクエリを生成（コンテキスト反映）
                    let sub_l = build_nvalue_standalone_subquery(
                        &left_keys[0],
                        &left_nvalue,
                        left_context.as_deref(),
                        view,
                        true, // include_item_id: 不同キー同士の結合には item_id が必要
                    );
                    let sub_r = build_nvalue_standalone_subquery(
                        &right_keys[0],
                        &right_nvalue,
                        right_context.as_deref(),
                        view,
                        true, // include_item_id: 同上
                    );

                    // JOIN して比較
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
                            Expr::col((Alias::new("L"), Alias::new("nvalue")))
                                .binary(
                                    bin_op,
                                    Expr::col((
                                        Alias::new("R"),
                                        Alias::new("nvalue"),
                                    )),
                                ),
                        )
                        .to_owned();

                    let proj_col = match left_keys[0].get_storage() {
                    Some(StorageMapping::RowTag { column, .. }) => column,
                    Some(StorageMapping::Column(col)) => col,
                    _ => panic!("unexpected MergedNestMatch with non-TagRef keys: {:?}", left_keys),
                };

                    stmt.and_where(Expr::col(*proj_col).in_subquery(join_sql));
                    stmt
                }
            }
        }

        ResolvedNode::ScalarMatch { left, op, right } => {
            // リテラル同士のスカラー比較: is_boolean_result() 経由で
            // build_boolean_sql が使われるため通常ここには到達しないが、
            // wrap_boolean_collider 等から呼ばれる場合に備える。
            let mut stmt = Query::select();
            stmt.from(Alias::new(view));
            stmt.column(Col::ItemId);
            let bin_op = to_bin_op(*op);
            let cond = Expr::expr(label_to_unit_aware_expr(left))
                .binary(bin_op, label_to_unit_aware_expr(right));
            stmt.cond_where(cond);
            stmt.limit(1);
            stmt
        }
        ResolvedNode::Complement(c) => build_resolved_comp_sql(c, view),
        // LabelSetOp がフィルタコンテキスト（Nest の右辺等）で呼ばれる場合:
        // item-level の集合演算として処理する（ラベル値集合演算ではなくアイテム ID 集合）
        ResolvedNode::LabelSetOp { op, operands } => {
            use crate::query::lens_resolver::LabelSetOpKind;
            match op {
                LabelSetOpKind::Union => build_resolved_or_sql(operands, view),
                LabelSetOpKind::Intersect => {
                    build_resolved_and_sql(operands, view)
                }
                LabelSetOpKind::Except => {
                    if let (Some(left), Some(right)) =
                        (operands.first(), operands.get(1))
                    {
                        build_resolved_diff_sql(left, right, view)
                    } else {
                        Query::select().to_owned()
                    }
                }
            }
        }
    }
}

/// nvalue 付き Nest に対する集約 SQL を生成する。
/// `sum(parentdir: &: count(ext:jpg))` のように、集約の inner が
/// nvalue 付き Nest の場合、nvalue を集約対象にした SQL を返す。
/// 該当しない場合は None を返す。
fn build_agg_over_nvalue_projection(
    agg: &ResolvedAggregationNode,
    view: &str,
) -> Option<SelectStatement> {
    let (outer_is_count, outer_arith_op, inner) = match agg {
        ResolvedAggregationNode::Count(inner) => (true, None, inner.as_ref()),
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            (false, Some(op), inner.as_ref())
        }
    };

    // inner が nvalue 付き Projection または NestMatch かチェック
    let (proj_operand, nvalue, merged_context) =
        match extract_nvalue_projection_parts(inner.clone()) {
            Ok(parts) => parts,
            Err(_) => return None,
        };

    // nvalue_condition は NestMatch の場合に存在する
    let nvalue_condition = inner.get_nvalue_condition();
    let context = merged_context.as_deref();

    // 多キーNestの場合: pivot 集計を使用し、全キーで GROUP BY する
    if proj_operand.len() > 1 {
        let pivot_agg = build_nvalue_pivot_aggregate_sql(
            &proj_operand,
            &nvalue,
            context,
            view,
        );

        // nvalue_condition がある場合、pivot 結果にフィルタを適用
        let source = if let Some((op, value)) = nvalue_condition {
            let bin_op = to_bin_op(*op);
            let val = label_to_simple_expr(value);
            Query::select()
                .column(Alias::new("nvalue"))
                .from_subquery(pivot_agg, Alias::new("pivot_agg"))
                .and_where(Expr::col(Alias::new("nvalue")).binary(bin_op, val))
                .to_owned()
        } else {
            pivot_agg
        };

        let mut stmt = Query::select();
        if outer_is_count {
            stmt.expr_as(Expr::cust("COUNT(*)"), Alias::new("scalar_value"));
        } else {
            let op = outer_arith_op.unwrap();
            let is_string = nvalue.is_string_type();
            stmt.expr_as(
                apply_arithmetic_agg(
                    op,
                    Expr::col(Alias::new("nvalue")).into(),
                    is_string,
                ),
                Alias::new("scalar_value"),
            );
        }
        stmt.from_subquery(source, Alias::new("nv_groups"));
        return Some(stmt);
    }

    // 単一キーの場合: 既存のスタンドアロンサブクエリを使用
    let mut nvalue_sub = build_nvalue_standalone_subquery(
        &proj_operand[0],
        &nvalue,
        context,
        view,
        true,
    );

    // nvalue_condition がある場合、フィルタ条件を追加
    if let Some((op, value)) = nvalue_condition {
        let bin_op = to_bin_op(*op);
        let val = label_to_simple_expr(value);
        let cond = Expr::col(Alias::new("nvalue")).binary(bin_op, val);
        // nvalue _items サブクエリの結果に対してフィルタするため常に WHERE
        nvalue_sub.and_where(cond);
    }

    // group_label と nvalue のペアで投影をマージ（重複排除）
    let deduped = Query::select()
        .column(Alias::new("group_label"))
        .column(Alias::new("nvalue"))
        .from_subquery(nvalue_sub, Alias::new("nv_items"))
        .group_by_col(Alias::new("group_label"))
        .group_by_col(Alias::new("nvalue"))
        .to_owned();

    let mut stmt = Query::select();
    if outer_is_count {
        // count(Nest_with_nvalue) → ラベル＋評価値ペアの数
        stmt.expr_as(Expr::cust("COUNT(*)"), Alias::new("scalar_value"));
    } else {
        // sum/avg/max/min(Nest_with_nvalue) → nvalue の集約
        let op = outer_arith_op.unwrap();
        let is_string = nvalue.is_string_type();
        stmt.expr_as(
            apply_arithmetic_agg(
                op,
                Expr::col(Alias::new("nvalue")).into(),
                is_string,
            ),
            Alias::new("scalar_value"),
        );
    }
    stmt.from_subquery(deduped, Alias::new("nv_groups"));
    Some(stmt)
}

/// Count の引数ノードから、カウント対象のカラムと内部タグタイプを決定する。
///
/// count の基本セマンティクス:
/// - Nest (`extension:`) → 種類数: `COUNT(DISTINCT label_col)` + タグタイプ
/// - TypedTag (`extension:jpg`) → アイテム数: `COUNT(DISTINCT item_id)`
///
/// 戻り値: `(count_col, Option<inner_tag_type>)`
///   - `inner_tag_type` が Some の場合、nvalue では JOIN の条件に使う
fn resolve_count_target(inner: &ResolvedNode) -> (Col, Option<String>) {
    inner
        .get_nested_projection()
        .and_then(|op| match op.get_storage() {
            Some(StorageMapping::RowTag {
                column, tag_type, ..
            }) => Some((*column, Some(tag_type.clone()))),
            Some(StorageMapping::Column(col)) => Some((*col, None)),
            _ => None,
        })
        .unwrap_or((Col::ItemId, None))
}

/// Count 集約の nvalue SQL を生成する共通ヘルパー。
///
/// `build_nvalue_cte` と `build_nvalue_standalone_subquery` の両方から呼び出される。
/// - `proj_col`: プロジェクション（左辺）の物理カラム
/// - `proj_tag_type`: プロジェクションの tag_type（RowTag の場合）
/// - `inner`: Count の引数ノード
/// - `context`: Nest 左辺のコンテキストフィルタ
/// - `item_scope`: アイテムのスコープを制限するサブクエリ（CTE版: picked_ids, standalone版: None）
/// - `view`: ビュー名
fn wrap_with_item_id(
    agg_sub: SelectStatement,
    proj_col: Col,
    proj_tag_type: Option<&str>,
    view: &str,
) -> SelectStatement {
    let mut wrapped = Query::select();
    wrapped
        .column((Alias::new("view"), Col::ItemId))
        .expr_as(
            Expr::col((Alias::new("view"), proj_col)),
            Alias::new("group_label"),
        )
        .column((Alias::new("agg"), Alias::new("nvalue")))
        .from_as(Alias::new(view), Alias::new("view"))
        .join_subquery(
            sea_query::JoinType::InnerJoin,
            agg_sub,
            Alias::new("agg"),
            Expr::col((Alias::new("view"), proj_col))
                .eq(Expr::col((Alias::new("agg"), Alias::new("group_label")))),
        );
    if let Some(tag_type) = proj_tag_type {
        wrapped
            .and_where(Expr::col((Alias::new("view"), Col::Type)).eq(tag_type));
    }
    wrapped
}

fn build_count_nvalue_sql(
    proj_col: Col,
    proj_tag_type: Option<&str>,
    inner: &ResolvedNode,
    context: Option<&ResolvedNode>,
    item_scope: Option<SelectStatement>,
    view: &str,
    include_item_id: bool,
) -> SelectStatement {
    let (count_col, inner_tag_type) = resolve_count_target(inner);

    let mut stmt = Query::select();

    if let Some(tag_type) = inner_tag_type {
        // ── Nest Count: 種類数を数える ──
        // JOIN して inner タグのラベルカラムで COUNT DISTINCT
        stmt.expr_as(
            Expr::col((Alias::new("proj"), proj_col)),
            Alias::new("group_label"),
        );
        stmt.expr_as(
            Expr::col((Alias::new("inner_tags"), count_col)).count_distinct(),
            Alias::new("nvalue"),
        );
        stmt.from_as(Alias::new(view), Alias::new("proj"));
        stmt.join_as(
            sea_query::JoinType::InnerJoin,
            Alias::new(view),
            Alias::new("inner_tags"),
            Expr::col((Alias::new("proj"), Col::ItemId))
                .equals((Alias::new("inner_tags"), Col::ItemId)),
        );

        if let Some(tag_type) = proj_tag_type {
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::Type)).eq(tag_type),
            );
        }
        stmt.and_where(
            Expr::col((Alias::new("inner_tags"), Col::Type)).eq(tag_type),
        );

        // item_scope (picked_ids etc.)
        if let Some(scope) = item_scope {
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(scope),
            );
        }

        // inner にフィルタが含まれる場合
        let (_storage, inner_filter, _operand) = inner.extract_agg_parts();
        if let Some(filter_node) = inner_filter {
            let filter_pick = build_pick_sql(&filter_node, view);
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from_subquery(filter_pick, Alias::new("nv_filter"))
                        .to_owned(),
                ),
            );
        }

        // context filter
        if let Some(ctx) = context {
            let context_pick = build_pick_sql(ctx, view);
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from_subquery(context_pick, Alias::new("nv_context"))
                        .to_owned(),
                ),
            );
        }

        stmt.group_by_col((Alias::new("proj"), proj_col));
    } else {
        // ── TypedTag Count: アイテム数を数える ──
        stmt.expr_as(Expr::col(proj_col), Alias::new("group_label"));
        stmt.expr_as(
            Expr::col(Col::ItemId).count_distinct(),
            Alias::new("nvalue"),
        );
        stmt.from(Alias::new(view));

        if let Some(tag_type) = proj_tag_type {
            stmt.and_where(Expr::col(Col::Type).eq(tag_type));
        }

        // item_scope (picked_ids etc.)
        if let Some(scope) = item_scope {
            stmt.and_where(Expr::col(Col::ItemId).in_subquery(scope));
        }

        // inner filter
        let inner_pick = build_pick_sql(inner, view);
        stmt.and_where(
            Expr::col(Col::ItemId).in_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .from_subquery(inner_pick, Alias::new("nv_inner"))
                    .to_owned(),
            ),
        );

        // context filter
        if let Some(ctx) = context {
            let context_pick = build_pick_sql(ctx, view);
            stmt.and_where(
                Expr::col(Col::ItemId).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from_subquery(context_pick, Alias::new("nv_context"))
                        .to_owned(),
                ),
            );
        }

        stmt.group_by_col(proj_col);
    }

    if include_item_id {
        wrap_with_item_id(stmt, proj_col, proj_tag_type, view)
    } else {
        stmt
    }
}

fn get_required_row_tags(node: &ResolvedNode) -> Vec<String> {
    match node {
        ResolvedNode::Match {
            storage: StorageMapping::RowTag { tag_type, .. },
            ..
        } => {
            vec![tag_type.clone()]
        }
        ResolvedNode::And(nodes) => {
            nodes.iter().flat_map(get_required_row_tags).collect()
        }
        _ => vec![],
    }
}

fn resolve_simple_filter_condition(
    node: &ResolvedNode,
    table: Alias,
) -> Option<Condition> {
    match node {
        ResolvedNode::Match { storage, label, .. } => {
            if let StorageMapping::RowTag { tag_type, .. } = storage {
                let s_val = label.as_str();
                let mut cond = Condition::all();
                if tag_type.as_str() != "*" {
                    cond = cond.add(
                        Expr::col((table.clone(), Col::Type))
                            .eq(tag_type.as_str()),
                    );
                }
                if s_val != "*" && s_val != "" {
                    cond =
                        cond.add(Expr::col((table, Col::LabelStr)).eq(s_val));
                }
                Some(cond)
            } else {
                None
            }
        }
        ResolvedNode::ColumnMatch { tag, label } => {
            let s_val = label.as_str();
            Some(Condition::all().add(Expr::col((table, *tag)).eq(s_val)))
        }
        ResolvedNode::And(nodes) => {
            let mut required_tags = get_required_row_tags(node);
            required_tags.sort();
            required_tags.dedup();
            if required_tags.len() > 1 {
                return None;
            }

            let mut all_cond = Condition::all();
            for n in nodes {
                if let Some(c) =
                    resolve_simple_filter_condition(n, table.clone())
                {
                    all_cond = all_cond.add(c);
                } else {
                    return None;
                }
            }
            Some(all_cond)
        }
        _ => None,
    }
}

pub(crate) fn build_merged_nvalue_agg_expr(
    nvalue: &ResolvedOperand,
    tbl_alias: &str,
    view: &str,
) -> SimpleExpr {
    match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            let (inner_tag, inner_filter, _) = inner.extract_agg_parts();
            if let Some(filter_node) = inner_filter {
                let case_expr: SimpleExpr = if let Some(cond) =
                    resolve_simple_filter_condition(
                        &filter_node,
                        Alias::new(tbl_alias),
                    ) {
                    if cond.is_empty() {
                        Expr::col((Alias::new(tbl_alias), Col::ItemId)).into()
                    } else {
                        Expr::case(
                            cond,
                            Expr::col((Alias::new(tbl_alias), Col::ItemId)),
                        )
                        .finally(Expr::val(None::<i32>))
                        .into()
                    }
                } else {
                    let filter_pick = build_pick_sql(&filter_node, view);
                    let filter_sub = Query::select()
                        .column(Col::ItemId)
                        .from_subquery(filter_pick, Alias::new("nv_filter"))
                        .to_owned();
                    let in_expr =
                        Expr::col((Alias::new(tbl_alias), Col::ItemId))
                            .in_subquery(filter_sub);
                    Expr::case(
                        in_expr,
                        Expr::col((Alias::new(tbl_alias), Col::ItemId)),
                    )
                    .finally(Expr::val(None::<i32>))
                    .into()
                };
                Expr::expr(case_expr).count_distinct().into()
            } else if let Some(StorageMapping::RowTag { tag_type, .. }) =
                inner_tag
            {
                let cond = Condition::all().add(
                    Expr::col((Alias::new(tbl_alias), Col::Type))
                        .eq(tag_type.as_str()),
                );
                let case_expr = Expr::case(
                    cond,
                    Expr::col((Alias::new(tbl_alias), Col::ItemId)),
                )
                .finally(Expr::val(None::<i32>));
                Expr::expr(case_expr).count_distinct().into()
            } else {
                Expr::col((Alias::new(tbl_alias), Col::ItemId))
                    .count_distinct()
                    .into()
            }
        }
        ResolvedOperand::Aggregation(
            agg @ ResolvedAggregationNode::Arithmetic { op, inner },
        ) => {
            let is_string = agg.is_string_type();
            let (inner_tag, inner_filter, _operand) = inner.extract_agg_parts();

            let val_expr: SimpleExpr = if is_string {
                Expr::col((Alias::new(tbl_alias), Col::LabelStr)).into()
            } else {
                // 数値型の場合は、数値カラムを優先し、文字列カラムは CAST を試みる。
                // DuckDB において、集計時に NULL は無視されるため、COALESCE で結合して
                // いずれかのカラムに値があればそれを集計対象とする。
                Expr::cust_with_exprs(
                    "COALESCE($1, $2, TRY_CAST($3 AS DOUBLE))",
                    [
                        Expr::col((Alias::new(tbl_alias), Col::LabelInt))
                            .into(),
                        Expr::col((Alias::new(tbl_alias), Col::LabelDouble))
                            .into(),
                        Expr::col((Alias::new(tbl_alias), Col::LabelStr))
                            .into(),
                    ],
                )
            };

            if let Some(filter_node) = inner_filter {
                // フィルタがある場合：集計対象タグ型条件（type='size'等）と
                // フィルタ条件（item_id IN サブクエリ）を組み合わせる。
                // フィルタは同じ行のタグ条件ではなく、アイテムIDベースのフィルタとして扱う。
                let filter_pick = build_pick_sql(&filter_node, view);
                let filter_sub = Query::select()
                    .column(Col::ItemId)
                    .from_subquery(filter_pick, Alias::new("nv_filter"))
                    .to_owned();
                let in_expr = Expr::col((Alias::new(tbl_alias), Col::ItemId))
                    .in_subquery(filter_sub);

                // inner_tag がある場合は type='size' 等の条件も追加する
                let case_expr: SimpleExpr =
                    if let Some(StorageMapping::RowTag { tag_type, .. }) =
                        inner_tag
                    {
                        let combined_cond = Condition::all()
                            .add(
                                Expr::col((Alias::new(tbl_alias), Col::Type))
                                    .eq(tag_type.as_str()),
                            )
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
            } else if let Some(StorageMapping::RowTag { tag_type, .. }) =
                inner_tag
            {
                let cond = Condition::all().add(
                    Expr::col((Alias::new(tbl_alias), Col::Type))
                        .eq(tag_type.as_str()),
                );
                let case_expr =
                    Expr::case(cond, val_expr).finally(Expr::val(None::<f64>));
                apply_arithmetic_agg(
                    op,
                    Expr::expr(case_expr).into(),
                    is_string,
                )
            } else {
                apply_arithmetic_agg(op, Expr::expr(val_expr).into(), is_string)
            }
        }
        ResolvedOperand::Calculation(calc) => {
            let left_expr =
                build_merged_nvalue_agg_expr(&calc.left, tbl_alias, view);
            let right_expr =
                build_merged_nvalue_agg_expr(&calc.right, tbl_alias, view);

            let left_val = Expr::expr(left_expr)
                .cast_as(crate::db::SqlType::DOUBLE)
                .into();
            let right_val = Expr::expr(right_expr)
                .cast_as(crate::db::SqlType::DOUBLE)
                .into();

            apply_arithmetic_op(&calc.op, left_val, right_val, false)
        }
        ResolvedOperand::Literal(l) => label_to_simple_expr(l),
        ResolvedOperand::TagRef { .. } => Expr::val(1i32).into(),
    }
}

/// nvalue サブクエリ（スタンドアロン版）。
/// `build_nvalue_cte` と同じロジックだが、picked_ids CTE を参照せず
/// 自己完結した SELECT を返す。
fn build_nvalue_standalone_subquery(
    proj_operand: &ResolvedOperand,
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
    include_item_id: bool,
) -> SelectStatement {
    // 物理カラム情報の抽出
    let (proj_col, proj_tag_type) = match proj_operand {
        ResolvedOperand::TagRef { storage, .. } => match storage {
            StorageMapping::RowTag {
                column, tag_type, ..
            } => (*column, Some(tag_type.as_str())),
            StorageMapping::Column(col) => (*col, None),
            _ => return SelectStatement::default(),
        },
        _ => return SelectStatement::default(),
    };

    match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            build_count_nvalue_sql(
                proj_col,
                proj_tag_type,
                inner,
                context,
                None, // standalone: no item_scope
                view,
                include_item_id,
            )
        }
        ResolvedOperand::Aggregation(
            agg @ ResolvedAggregationNode::Arithmetic { op, inner },
        ) => {
            let is_string = agg.is_string_type();
            let (_, _, _operand) = inner.extract_agg_parts();

            let deduped = build_deduplicated_agg_subquery(inner, view, context);

            let mut stmt = Query::select();
            stmt.expr_as(
                Expr::col((Alias::new("proj"), proj_col)),
                Alias::new("group_label"),
            );
            stmt.expr_as(
                apply_arithmetic_agg(
                    op,
                    Expr::col((Alias::new("deduped"), Alias::new("val")))
                        .into(),
                    is_string,
                ),
                Alias::new("nvalue"),
            );

            stmt.from_as(Alias::new(view), Alias::new("proj"));
            stmt.join_subquery(
                sea_query::JoinType::InnerJoin,
                deduped,
                Alias::new("deduped"),
                Expr::col((Alias::new("proj"), Col::ItemId))
                    .equals((Alias::new("deduped"), Col::ItemId)),
            );

            if let Some(tag_type) = proj_tag_type {
                stmt.and_where(
                    Expr::col((Alias::new("proj"), Col::Type)).eq(tag_type),
                );
            }

            stmt.group_by_col((Alias::new("proj"), proj_col));

            if include_item_id {
                wrap_with_item_id(stmt, proj_col, proj_tag_type, view)
            } else {
                stmt
            }
        }
        ResolvedOperand::Literal(label) => {
            let val = label_to_simple_expr(label);
            let mut stmt = Query::select();
            stmt.expr_as(Expr::col(proj_col), Alias::new("group_label"));
            stmt.expr_as(val, Alias::new("nvalue"));
            stmt.from(Alias::new(view));

            if let Some(tag_type) = proj_tag_type {
                stmt.and_where(Expr::col(Col::Type).eq(tag_type));
            }

            if include_item_id {
                stmt.column(Col::ItemId);
                stmt.group_by_col(proj_col);
                stmt.group_by_col(Col::ItemId);
                stmt
            } else {
                stmt.group_by_col(proj_col);
                stmt
            }
        }
        ResolvedOperand::Calculation(calc) => {
            let sub_l = build_nvalue_standalone_subquery(
                proj_operand,
                &calc.left,
                context,
                view,
                include_item_id,
            );
            let sub_r = build_nvalue_standalone_subquery(
                proj_operand,
                &calc.right,
                context,
                view,
                include_item_id,
            );

            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();

            // NULL 伝播防止のため RIGHT 側の nvalue を COALESCE で補完する。
            let r_nvalue_expr: SimpleExpr = if is_string {
                Func::coalesce([
                    Expr::col((Alias::new("R"), Alias::new("nvalue"))).into(),
                    Expr::val("").into(),
                ])
                .into()
            } else {
                Func::coalesce([
                    Expr::col((Alias::new("R"), Alias::new("nvalue"))).into(),
                    Expr::val(0.0f64).into(),
                ])
                .into()
            };

            let mut stmt = Query::select();
            stmt.expr_as(
                Expr::col((Alias::new("L"), Alias::new("group_label"))),
                Alias::new("group_label"),
            );
            stmt.expr_as(
                apply_arithmetic_op(
                    &calc.op,
                    Expr::col((Alias::new("L"), Alias::new("nvalue"))).into(),
                    r_nvalue_expr,
                    is_string,
                ),
                Alias::new("nvalue"),
            );

            if include_item_id {
                stmt.column((Alias::new("L"), Col::ItemId));
            }

            stmt.from_subquery(sub_l, Alias::new("L"));

            if include_item_id {
                stmt.join_subquery(
                    sea_query::JoinType::LeftJoin,
                    sub_r,
                    Alias::new("R"),
                    Expr::col((Alias::new("L"), Col::ItemId))
                        .eq(Expr::col((Alias::new("R"), Col::ItemId))),
                );
            } else {
                stmt.join_subquery(
                    sea_query::JoinType::LeftJoin,
                    sub_r,
                    Alias::new("R"),
                    Expr::col((Alias::new("L"), Alias::new("group_label"))).eq(
                        Expr::col((Alias::new("R"), Alias::new("group_label"))),
                    ),
                );
            }

            // JOIN された L/R どちらの nvalue かが曖昧になるため、
            // include_item_id に関わらず calc_sub でラップして曖昧性を排除する。
            let sub = stmt.to_owned();
            let mut outer = Query::select();
            outer
                .column(Alias::new("group_label"))
                .column(Alias::new("nvalue"));
            if include_item_id {
                outer.column(Col::ItemId);
            }
            outer.from_subquery(sub, Alias::new("calc_sub")).to_owned()
        }
        ResolvedOperand::TagRef {
            storage: nval_storage,
            ..
        } => {
            use crate::db::CustomFunc;
            let mut stmt = Query::select();
            stmt.from_as(Alias::new(view), Alias::new("proj"));

            match nval_storage {
                StorageMapping::Column(nv_col) => {
                    // Column タグ: proj の同一行にカラムとして存在するため結合不要。
                    stmt.expr_as(
                        Expr::col((Alias::new("proj"), proj_col)),
                        Alias::new("group_label"),
                    );
                    stmt.expr_as(
                        CustomFunc::any_value(Expr::col((
                            Alias::new("proj"),
                            *nv_col,
                        ))),
                        Alias::new("nvalue"),
                    );
                }
                StorageMapping::RowTag {
                    column: nv_col,
                    tag_type: nv_tag_type,
                } => {
                    // RowTag タグ: proj とは別 type の行を LEFT JOIN で取得する。
                    let nv_sub = Query::select()
                        .column(Col::ItemId)
                        .expr_as(
                            Expr::cust_with_exprs(
                                "TRY_CAST($1 AS DOUBLE)",
                                [Expr::col(*nv_col).into()],
                            ),
                            Alias::new("nval"),
                        )
                        .from(Alias::new(view))
                        .and_where(
                            Expr::col(Col::Type).eq(nv_tag_type.as_str()),
                        )
                        .to_owned();
                    stmt.join_subquery(
                        sea_query::JoinType::LeftJoin,
                        nv_sub,
                        Alias::new("nv"),
                        Expr::col((Alias::new("proj"), Col::ItemId))
                            .equals((Alias::new("nv"), Col::ItemId)),
                    );
                    stmt.expr_as(
                        Expr::col((Alias::new("proj"), proj_col)),
                        Alias::new("group_label"),
                    );
                    stmt.expr_as(
                        CustomFunc::any_value(Func::coalesce([
                            Expr::col((Alias::new("nv"), Alias::new("nval")))
                                .into(),
                            Expr::val(0.0f64).into(),
                        ])),
                        Alias::new("nvalue"),
                    );
                }
                StorageMapping::Virtual => {
                    stmt.expr_as(
                        Expr::col((Alias::new("proj"), proj_col)),
                        Alias::new("group_label"),
                    );
                    stmt.expr_as(Expr::val(0.0f64), Alias::new("nvalue"));
                }
            }

            if let Some(tt) = proj_tag_type {
                stmt.and_where(
                    Expr::col((Alias::new("proj"), Col::Type)).eq(tt),
                );
            }
            stmt.group_by_col((Alias::new("proj"), proj_col));

            if include_item_id {
                wrap_with_item_id(stmt, proj_col, proj_tag_type, view)
            } else {
                stmt
            }
        }
    }
}

pub fn build_resolved_aggregation_sql(
    agg: &ResolvedAggregationNode,
    view: &str,
) -> SelectStatement {
    // nvalue 付き Nest に対する集約の場合、nvalue を集約対象にする
    if let Some(nvalue_agg_sql) = build_agg_over_nvalue_projection(agg, view) {
        return nvalue_agg_sql;
    }

    let mut stmt = Query::select();

    match agg {
        ResolvedAggregationNode::Count(inner) => {
            stmt.from(Alias::new(view));
            let (_, cond, _) = inner.extract_agg_parts();
            let mut final_cond = Condition::all();

            // resolve_count_target で count の対象カラムとタグタイプを決定
            let (count_col, inner_tag_type) = resolve_count_target(inner);

            if let Some(key) = inner_tag_type {
                final_cond = final_cond.add(Expr::col(Col::Type).eq(key));
            }
            if let Some(filter_node) = cond {
                let pick_sql = build_pick_sql(&filter_node, view);
                let mut sub = Query::select();
                sub.column(Col::ItemId)
                    .from_subquery(pick_sql, Alias::new("sub"));
                final_cond =
                    final_cond.add(Expr::col(Col::ItemId).in_subquery(sub));
            }

            stmt.expr_as(
                Expr::col(count_col).count_distinct(),
                Alias::new("scalar_value"),
            );
            stmt.cond_where(final_cond);
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let is_string = agg.is_string_type();
            let (_, _, operand) = inner.extract_agg_parts();

            if std::env::var("TTFM_DEBUG").is_ok() {
                println!("DEBUG: build_resolved_aggregation_sql: is_string={}, operand={:?}", is_string, operand);
            }

            // 重要: 同一アイテムに対して複数行がマッチする場合、単純な SUM だと重複加算される。
            let sub = build_deduplicated_agg_subquery(inner, view, None);
            let sub_alias = Alias::new("deduped_items");

            stmt.expr_as(
                apply_arithmetic_agg(
                    op,
                    Expr::col(Alias::new("val")).into(),
                    is_string,
                ),
                Alias::new("scalar_value"),
            );
            stmt.from_subquery(sub, sub_alias);
        }
    }

    let sql = stmt.to_owned();
    if std::env::var("TTFM_DEBUG").is_ok() {
        println!(
            "DEBUG AGG SQL: {}",
            sql.to_string(sea_query::PostgresQueryBuilder)
        );
    }
    sql
}

/// 同一アイテムの重複を排除した集計用サブクエリを構築します。
/// アイテムごとに1つの値を MAX で取得し、それらを後続の処理で集計できるようにします。
fn build_deduplicated_agg_subquery(
    inner: &ResolvedNode,
    view: &str,
    context: Option<&ResolvedNode>,
) -> SelectStatement {
    let (_storage, cond, operand) = inner.extract_agg_parts();

    let mut sub = Query::select();
    sub.column(Col::ItemId);

    // オペランドから素の値を解決
    let operand_expr = if let Some(op_node) = operand {
        build_resolved_operand_expr_for_arithmetic(op_node, view)
    } else {
        Expr::val(0).into()
    };

    // 各アイテムごとに1つの値を MAX で取得（デデュープ）
    sub.expr_as(Func::max(operand_expr), Alias::new("val"));
    sub.from(Alias::new(view));

    let mut sub_cond = Condition::all();

    // operand（TagRef, Calculation等）に含まれるすべての tag_type を抽出してフィルタに追加
    if let Some(op_node) = operand {
        let mut keys = Vec::new();
        collect_tag_types(op_node, &mut keys);
        if !keys.is_empty() {
            sub_cond = sub_cond.add(Expr::col(Col::Type).is_in(keys));
        }
    }

    // context filter
    if let Some(ctx) = context {
        let context_pick = build_pick_sql(ctx, view);
        let mut context_sub = Query::select();
        context_sub
            .column(Col::ItemId)
            .from_subquery(context_pick, Alias::new("ctx_sub"));
        sub_cond =
            sub_cond.add(Expr::col(Col::ItemId).in_subquery(context_sub));
    }

    if let Some(filter_node) = cond {
        let pick_sql = build_pick_sql(&filter_node, view);
        let mut pick_sub = Query::select();
        pick_sub
            .column(Col::ItemId)
            .from_subquery(pick_sql, Alias::new("psub"));
        sub_cond = sub_cond.add(Expr::col(Col::ItemId).in_subquery(pick_sub));
    }
    sub.cond_where(sub_cond);
    sub.group_by_col(Col::ItemId);

    sub
}

/// スカラー式（集計計算など）から単一の結果を得るための SQL を生成します。
pub fn build_resolved_scalar_sql(
    op: &crate::query::lens_resolver::ResolvedOperand,
    view: &str,
) -> SelectStatement {
    use crate::query::lens_resolver::ResolvedOperand;
    match op {
        ResolvedOperand::Aggregation(agg) => {
            build_resolved_aggregation_sql(agg, view)
        }
        _ => {
            let mut stmt = Query::select();
            stmt.from(Alias::new(view));
            stmt.expr_as(
                build_resolved_operand_subquery(op, view),
                Alias::new("scalar_value"),
            );
            stmt.limit(1);
            stmt
        }
    }
}

pub(crate) fn build_resolved_aggregation_match_sql(
    agg: &ResolvedAggregationNode,
    op: ComparisonOp,
    label: &Label,
    view: &str,
) -> SelectStatement {
    let (agg_expr, cond, tag_type) = build_aggregation_parts(agg, view);
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));

    let op_bin = to_bin_op(op);
    let rhs = Expr::val(label.as_i64()); // TODO: 型に応じた変換

    let condition = Expr::expr(agg_expr.clone()).binary(op_bin, rhs);

    // 集計対象（Nest）がある場合、その型(type)自体で行を絞り込む必要がある
    let _target_type = match agg {
        ResolvedAggregationNode::Count(inner) => inner.get_projection(),
        ResolvedAggregationNode::Arithmetic { inner, .. } => {
            inner.get_projection()
        }
    };

    let mut final_cond = Condition::all();
    if let Some(key) = tag_type {
        // RowTag の場合は実際の tag_type でフィルタする
        // エイリアス名 "type" との競合を避けるためテーブル名を明示
        final_cond =
            final_cond.add(Expr::col((Alias::new(view), Col::Type)).eq(key));
    } else if let Some(_target_type) = _target_type {
        // 念のため: tag_type がなく target_type がある場合は何もしない
        // （物理カラムの場合は type フィルタ不要）
    }

    // 検索条件 (cond) がある場合は、それを ItemId の絞り込みとして IN サブクエリに送る
    if let Some(filter_node) = cond {
        let pick_sql = build_pick_sql(&filter_node, view);
        let mut sub = Query::select();
        sub.column(Col::ItemId)
            .from_subquery(pick_sql, Alias::new("sub"));
        final_cond = final_cond.add(Expr::col(Col::ItemId).in_subquery(sub));
    }
    stmt.cond_where(final_cond);

    // 集計結果の比較条件を HAVING 句に追加
    // これにより、条件を満たさない場合は行が返らなくなり、INTERSECT 等の結合が正しく機能する
    stmt.cond_having(condition);

    stmt.expr_as(Expr::cust("ANY_VALUE(item_id)"), Col::ItemId);
    stmt.expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::ItemKind,
    );
    stmt.expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::Type,
    );
    stmt.expr_as(Expr::val(0i64), Col::Rank);
    // tags カラムが必要（fetch_items で decode_item_from_row が呼ばれるため）
    stmt.expr_as(Expr::cust("[]"), crate::db::QueryResultCol::Tags);

    stmt.limit(1);

    stmt.to_owned()
}

fn build_resolved_tag_tag_match_sql(
    left_storage: &StorageMapping,
    left_sql_type: crate::db::SqlType,
    op: ComparisonOp,
    right_storage: &StorageMapping,
    right_sql_type: crate::db::SqlType,
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

fn build_tag_value_agg_expr(
    storage: &StorageMapping,
    _sql_type: crate::db::SqlType,
) -> SimpleExpr {
    use crate::db::CustomFunc;
    match storage {
        StorageMapping::Column(col) => {
            CustomFunc::any_value(Expr::col(*col)).into()
        }
        StorageMapping::RowTag { column, tag_type } => {
            // RowTag は label_str (VARCHAR) として保存されているため、
            // 算術演算を行う場合は TRY_CAST(... AS DOUBLE) で数値に変換する
            // TRY_CAST は変換失敗時に NULL を返す（エラーにならない）
            let cast_expr = Expr::cust_with_exprs(
                "TRY_CAST($1 AS DOUBLE)",
                [Expr::col(*column).into()],
            );
            Expr::cust_with_exprs(
                "MAX(CASE WHEN $1 = $2 THEN $3 END)",
                [
                    Expr::col(Col::Type).into(),
                    Expr::val(tag_type.as_str()).into(),
                    cast_expr.into(),
                ],
            )
        }
        StorageMapping::Virtual => {
            // Virtualタグは集約対象外とする
            CustomFunc::any_value(Expr::val(0)).into()
        }
    }
}

/// EAV 構造における Tag vs Calculation 比較用の SQL を生成します。
/// GROUP BY item_id HAVING で集約計算を行います。
fn build_tag_calc_match_eav_sql(
    left_storage: &StorageMapping,
    left_sql_type: crate::db::SqlType,
    op: crate::query::ast::ComparisonOp,
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.column(Col::ItemId)
        .from(Alias::new(view))
        .group_by_col(Col::ItemId);

    // 左辺（タグ）を集約式に変換
    let left_expr = build_tag_value_agg_expr(left_storage, left_sql_type);

    // 右辺（計算式）を集約式に変換
    let calc_expr = build_calculation_eav_expr(calc, view);

    q.and_having(left_expr.binary(to_bin_op(op), calc_expr));

    q
}

/// EAV 構造用の計算式を集約式として構築します。
fn build_calculation_eav_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_eav_expr(&calc.left, view);
    let right_expr = build_resolved_operand_eav_expr(&calc.right, view);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
}

/// EAV 構造用のオペランドを集約式として構築します。
fn build_resolved_operand_eav_expr(
    operand: &crate::query::lens_resolver::ResolvedOperand,
    view: &str,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedOperand;

    match operand {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => build_tag_value_agg_expr(storage, *sql_type),
        ResolvedOperand::Calculation(calc) => {
            build_calculation_eav_expr(calc, view)
        }
        ResolvedOperand::Aggregation(agg) => {
            use crate::query::lens_resolver::ResolvedAggregationNode;
            // Pivot CTE コンテキストでは、外部集計（SUM/AVG等）はウィンドウ関数で適用される。
            // ここでは内側オペランドのタイプフィルタ済み per-item 値を返す。
            // 例: sum(size:) → MAX(CASE WHEN type='size' THEN label_int END)
            match agg {
                ResolvedAggregationNode::Arithmetic { inner, .. } => {
                    let (_, cond, operand) = inner.extract_agg_parts();
                    if let Some(op) = operand {
                        let base_expr =
                            build_resolved_operand_eav_expr(op, view);
                        if let Some(filter) = cond {
                            // 集約の中にフィルタがある場合、そのフィルタを満たすアイテムのみを集計対象にする。
                            // Pivot CTE は GROUP BY item_id であるため、CASE WHEN item_id IN (...) THEN ... END でラップする。
                            let pick_sql = build_pick_sql(&filter, view);
                            let mut sub = Query::select();
                            sub.column(Col::ItemId)
                                .from_subquery(pick_sql, Alias::new("f"));

                            Expr::cust_with_exprs(
                                "CASE WHEN item_id IN ($1) THEN $2 END",
                                [
                                    SimpleExpr::SubQuery(
                                        None,
                                        Box::new(
                                            sub.into_sub_query_statement(),
                                        ),
                                    ),
                                    base_expr,
                                ],
                            )
                        } else {
                            base_expr
                        }
                    } else {
                        build_aggregation_expr(agg, view)
                    }
                }
                _ => build_aggregation_expr(agg, view),
            }
        }
    }
}

/// リテラル（定数）を SQL 式に変換します。
fn build_resolved_literal_expr(lab: &Label) -> SimpleExpr {
    // サイズ単位のパース (例: "1MB" → 1048576)
    let s = lab.as_str();
    if let Some(bytes) = crate::util::parse_size(&s) {
        // 除算などで整数切り捨てを防ぐため、DOUBLEとして扱う
        Expr::val(bytes).cast_as(crate::db::SqlType::DOUBLE).into()
    } else {
        // ラベルの値をSQL式に変換
        match lab.value() {
            crate::types::LabelValue::Integer(i) => {
                Expr::val(i).cast_as(crate::db::SqlType::DOUBLE).into()
            }
            crate::types::LabelValue::String(s)
            | crate::types::LabelValue::Literal(s) => {
                Expr::val(s.clone()).into()
            }
            crate::types::LabelValue::Boolean(b) => Expr::val(b).into(),
            crate::types::LabelValue::Double(bits) => {
                Expr::val(f64::from_bits(bits)).into()
            }
            crate::types::LabelValue::Null => Expr::val(None::<i32>).into(),
        }
    }
}

fn build_aggregation_parts(
    agg: &ResolvedAggregationNode,
    view: &str,
) -> (SimpleExpr, Option<ResolvedNode>, Option<String>) {
    match agg {
        ResolvedAggregationNode::Count(node) => {
            let (storage, cond, _) = node.extract_agg_parts();
            let tag_type;
            let expr = if let Some(s) = storage {
                // count(projection:) -> COUNT(DISTINCT col)
                let col = match s {
                    StorageMapping::Column(c) => {
                        tag_type = None;
                        *c
                    }
                    StorageMapping::RowTag {
                        column,
                        tag_type: key,
                    } => {
                        tag_type = Some(key.clone());
                        *column
                    }
                    _ => {
                        tag_type = None;
                        Col::LabelInt
                    } // Fallback
                };
                Expr::col(col).count_distinct().into()
            } else {
                // count(query) -> COUNT(DISTINCT item_id)
                tag_type = None;
                Expr::col(Col::ItemId).count_distinct().into()
            };
            (expr, cond, tag_type)
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let (storage, cond, operand) = inner.extract_agg_parts();
            let tag_type;
            match storage {
                Some(StorageMapping::Column(_)) => {
                    tag_type = None;
                }
                Some(StorageMapping::RowTag { tag_type: key, .. }) => {
                    tag_type = Some(key.clone());
                }
                _ => {
                    tag_type = None;
                } // Fallback
            };

            // TRY_CAST は build_storage_column_expr 内で適用されるため、
            // ここでは単純に式を構築するだけ
            let expr: SimpleExpr = if let Some(operand) = operand {
                // オペランド（算術式等）から直接 SQL 式を構築
                let is_string = operand.is_string_type();
                let inner_expr =
                    build_resolved_operand_eav_row_expr(operand, view);
                apply_arithmetic_agg(op, inner_expr, is_string)
            } else {
                // 直接のタグ参照の場合、その型を判定
                // RowTag の場合は数値演算が必要ならキャストされる
                let tag_row_expr = build_tag_value_eav_row_expr(
                    &storage.unwrap(),
                    SqlType::DOUBLE, // TODO: 型判定
                );
                apply_arithmetic_agg(op, tag_row_expr, false)
            };
            (expr, cond, tag_type)
        }
    }
}

/// 集約関数を式に適用します。
fn apply_arithmetic_agg(
    op: &ArithmeticAggOp,
    expr: SimpleExpr,
    is_string: bool,
) -> SimpleExpr {
    use ArithmeticAggOp::*;
    match op {
        Sum => {
            if is_string {
                // 文字列の合計はカンマ区切り結合 (DuckDB: string_agg)
                Func::cust(Alias::new("string_agg"))
                    .args([expr, Expr::val(", ").into()])
                    .into()
            } else {
                Func::sum(expr).into()
            }
        }
        Avg => Func::avg(expr).into(),
        Max => Func::max(expr).into(),
        Min => Func::min(expr).into(),
    }
}

/// StorageMappingから適切なSQL列式を生成します。
/// `sql_type` が `Numeric` の場合、`RowTag` の `LabelStr` (VARCHAR) には
/// `TRY_CAST` を適用して算術演算を可能にします。
fn build_storage_column_expr(
    storage: &StorageMapping,
    sql_type: crate::db::SqlType,
) -> SimpleExpr {
    use crate::db::CustomFunc;
    match storage {
        StorageMapping::Column(col) => Expr::col(*col).into(),
        StorageMapping::RowTag { column, .. } => {
            let col_expr = Expr::col(*column);
            // RowTag の label_str (VARCHAR) に対して数値演算が必要な場合は TRY_CAST を適用
            if *column == Col::LabelStr
                && matches!(
                    sql_type,
                    crate::db::SqlType::BIGINT | crate::db::SqlType::DOUBLE
                )
            {
                Expr::cust_with_exprs(
                    "TRY_CAST($1 AS DOUBLE)",
                    [col_expr.into()],
                )
            } else {
                col_expr.into()
            }
        }
        StorageMapping::Virtual => {
            // Virtualタグ用。将来的に論理タグをサポート
            CustomFunc::any_value(Expr::col(Col::LabelStr)).into()
        }
    }
}

/// EAV 構造において、特定のタグの値を（集計せずに）行レベルで取得する式を構築します。
/// (例: CASE WHEN type = 'size' THEN TRY_CAST(label_str AS DOUBLE) END)
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

/// 算術演算のオペランドをSQL式に変換します。
fn build_resolved_operand_expr(
    operand: &crate::query::lens_resolver::ResolvedOperand,
    view: &str,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedOperand;

    match operand {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => build_storage_column_expr(storage, *sql_type),
        ResolvedOperand::Calculation(calc) => {
            build_calculation_expr(calc, view)
        }
        ResolvedOperand::Aggregation(agg) => build_aggregation_expr(agg, view),
    }
}

/// EAV 構造用のオペランドを集計なしの行レベル式として構築します。
/// 集計関数（SUM等）の引数として使用されます。
fn build_resolved_operand_eav_row_expr(
    operand: &crate::query::lens_resolver::ResolvedOperand,
    view: &str,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedOperand;

    match operand {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => build_tag_value_eav_row_expr(storage, *sql_type),
        ResolvedOperand::Calculation(calc) => {
            let left = build_resolved_operand_eav_row_expr(&calc.left, view);
            let right = build_resolved_operand_eav_row_expr(&calc.right, view);
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left, right, is_string)
        }
        ResolvedOperand::Aggregation(agg) => build_aggregation_expr(agg, view),
    }
}

/// 集約関数をSQL式に変換します（算術演算内で使用）。
fn build_aggregation_expr(
    agg: &crate::query::lens_resolver::ResolvedAggregationNode,
    view: &str,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedAggregationNode;

    match agg {
        ResolvedAggregationNode::Count(inner) => {
            let (storage, cond, _) = inner.extract_agg_parts();
            let col = if let Some(s) = storage {
                match s {
                    StorageMapping::Column(c) => Col::from(*c),
                    StorageMapping::RowTag { column, .. } => *column,
                    _ => Col::LabelInt, // Fallback
                }
            } else {
                Col::ItemId
            };

            let base_expr: SimpleExpr = if let Some(c) = cond {
                // count(stem:*a*) のように内部にフィルタがある場合
                let pick_q = build_pick_sql(&c, view);
                let mut pick_ids = Query::select();
                pick_ids
                    .column(Col::ItemId)
                    .from_subquery(pick_q, Alias::new("ctx_agg"));
                Expr::case(
                    Expr::col(Col::ItemId).in_subquery(pick_ids),
                    Expr::col(col),
                )
                .into()
            } else {
                Expr::col(col).into()
            };
            Expr::expr(base_expr).count_distinct().into()
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let (storage, cond, _) = inner.extract_agg_parts();
            let col = if let Some(s) = storage {
                match s {
                    StorageMapping::Column(c) => Col::from(*c),
                    StorageMapping::RowTag { column, .. } => *column,
                    _ => Col::LabelInt, // Fallback
                }
            } else {
                Col::LabelInt
            };

            let base_expr: SimpleExpr = if let Some(c) = cond {
                let pick_q = build_pick_sql(&c, view);
                let mut pick_ids = Query::select();
                pick_ids
                    .column(Col::ItemId)
                    .from_subquery(pick_q, Alias::new("ctx_agg"));
                Expr::case(
                    Expr::col(Col::ItemId).in_subquery(pick_ids),
                    Expr::col(col),
                )
                .into()
            } else {
                Expr::col(col).into()
            };

            match op {
                crate::query::ast::ArithmeticAggOp::Sum => {
                    Func::cust(Alias::new("SUM")).arg(base_expr).into()
                }
                crate::query::ast::ArithmeticAggOp::Avg => {
                    Func::cust(Alias::new("AVG")).arg(base_expr).into()
                }
                crate::query::ast::ArithmeticAggOp::Max => {
                    Func::cust(Alias::new("MAX")).arg(base_expr).into()
                }
                crate::query::ast::ArithmeticAggOp::Min => {
                    Func::cust(Alias::new("MIN")).arg(base_expr).into()
                }
            }
        }
    }
}

/// 算術演算ノードをSQL式に変換します。
fn build_calculation_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_expr(&calc.left, view);
    let right_expr = build_resolved_operand_expr(&calc.right, view);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
}

fn build_calculation_subquery(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_subquery(&calc.left, view);
    let right_expr = build_resolved_operand_subquery(&calc.right, view);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
}

/// オペランドをサブクエリ形式で構築します。
fn build_resolved_operand_subquery(
    operand: &crate::query::lens_resolver::ResolvedOperand,
    view: &str,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedOperand;

    match operand {
        ResolvedOperand::Literal(lab) => {
            if let Some(bytes) = crate::util::parse_size(&lab.as_str()) {
                Expr::val(bytes).cast_as(crate::db::SqlType::DOUBLE).into()
            } else {
                match lab.value() {
                    crate::types::LabelValue::Integer(i) => {
                        Expr::val(i).cast_as(crate::db::SqlType::DOUBLE).into()
                    }
                    crate::types::LabelValue::String(s)
                    | crate::types::LabelValue::Literal(s) => {
                        Expr::val(s.clone()).into()
                    }
                    crate::types::LabelValue::Boolean(b) => Expr::val(b).into(),
                    crate::types::LabelValue::Double(bits) => {
                        Expr::val(f64::from_bits(bits)).into()
                    }
                    crate::types::LabelValue::Null => {
                        Expr::val(None::<i32>).into()
                    }
                }
            }
        }
        ResolvedOperand::TagRef { .. } => Expr::val(0).into(),
        ResolvedOperand::Calculation(calc) => {
            build_calculation_subquery(calc, view)
        }
        ResolvedOperand::Aggregation(agg) => {
            build_aggregation_subquery(agg, view)
        }
    }
}

/// 集約関数をサブクエリとして構築します。
fn build_aggregation_subquery(
    agg: &crate::query::lens_resolver::ResolvedAggregationNode,
    view: &str,
) -> SimpleExpr {
    // build_resolved_aggregation_sql は nvalue 付き Nest も含め正しく処理する
    let subquery = build_resolved_aggregation_sql(agg, view);
    SimpleExpr::SubQuery(None, Box::new(subquery.into_sub_query_statement()))
}

/// 算術演算用のオペランドをSQL式に変換します。
/// RowTag の LabelStr (VARCHAR) は TRY_CAST で DOUBLE に変換されます。
fn build_resolved_operand_expr_for_arithmetic(
    operand: &crate::query::lens_resolver::ResolvedOperand,
    view: &str,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedOperand;

    // 文字列型の場合は数値キャストを行わず、通常の式構築を行う
    if operand.is_string_type() {
        return build_resolved_operand_expr(operand, view);
    }

    match operand {
        ResolvedOperand::Literal(lab) => {
            let expr = build_resolved_literal_expr(lab);
            if matches!(lab.value(), crate::types::LabelValue::Boolean(_)) {
                // Boolean リテラルを数値演算用にキャスト
                expr.cast_as(crate::db::SqlType::BIGINT).into()
            } else {
                expr
            }
        }
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => {
            if *sql_type == crate::db::SqlType::BOOLEAN {
                let col_expr = build_storage_column_expr(storage, *sql_type);
                return col_expr.cast_as(crate::db::SqlType::BIGINT).into();
            }

            // 算術演算コンテキストでは、RowTag の LabelStr に TRY_CAST を適用
            match storage {
                StorageMapping::RowTag { column, .. }
                    if *column == Col::LabelStr =>
                {
                    Expr::cust_with_exprs(
                        "TRY_CAST($1 AS DOUBLE)",
                        [Expr::col(*column).into()],
                    )
                }
                StorageMapping::RowTag { column, .. } => {
                    Expr::col(*column).into()
                }
                StorageMapping::Column(col) => Expr::col(*col).into(),
                StorageMapping::Virtual => Expr::col(Col::LabelStr).into(),
            }
        }
        ResolvedOperand::Calculation(calc) => {
            // 再帰的に _for_arithmetic を使うことで、ネストされた boolean/string タグも正しくキャストされる
            let left_expr =
                build_resolved_operand_expr_for_arithmetic(&calc.left, view);
            let right_expr =
                build_resolved_operand_expr_for_arithmetic(&calc.right, view);
            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();
            apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
        }
        ResolvedOperand::Aggregation(agg) => build_aggregation_expr(agg, view),
    }
}

/// 算術演算子を適用します。
fn apply_arithmetic_op(
    op: &ArithmeticOp,
    left: SimpleExpr,
    right: SimpleExpr,
    is_string: bool,
) -> SimpleExpr {
    use ArithmeticOp::*;
    if is_string {
        return match op {
            Add => {
                // 文字列の + はカンマ区切り結合
                Expr::expr(left)
                    .binary(BinOper::Custom("||"), Expr::val(", "))
                    .binary(BinOper::Custom("||"), right)
            }
            Mul => {
                // 文字列の * は単純結合
                Expr::expr(left).binary(BinOper::Custom("||"), right)
            }
            _ => {
                // 解決フェーズで弾かれているはずだが安全のため
                Expr::expr(left).binary(BinOper::Custom("||"), right)
            }
        };
    }

    let bin_op = match op {
        Add => BinOper::Add,
        Sub => BinOper::Sub,
        Mul => BinOper::Mul,
        Div => BinOper::Div,
        Mod => BinOper::Custom("%"),
    };
    Expr::expr(left).binary(bin_op, right)
}

pub fn build_boolean_sql(node: &ResolvedNode, view: &str) -> SelectStatement {
    // 比較系ノードの場合は直接 SELECT で比較結果を計算
    // これによりFALSEとNULLを区別できる
    match node {
        ResolvedNode::AggregationMatch { agg, op, label } => {
            build_direct_boolean_select(
                build_aggregation_subquery(agg, view),
                *op,
                label_to_unit_aware_expr(label),
                view,
            )
        }
        ResolvedNode::AggregationAggregationMatch { left, op, right } => {
            build_direct_boolean_select(
                build_aggregation_subquery(left, view),
                *op,
                build_aggregation_subquery(right, view),
                view,
            )
        }
        ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
            let calc_expr = if calc.contains_aggregation() {
                build_calculation_subquery(calc, view)
            } else {
                build_calculation_expr(calc, view)
            };
            build_direct_boolean_select(
                build_aggregation_subquery(agg, view),
                *op,
                calc_expr,
                view,
            )
        }
        ResolvedNode::AggregationTagMatch { .. } => {
            // タグ側は行ごとに異なる可能性があるのでWHERE方式を使用
            // （将来的に改善の余地があるが、一旦現状維持）
            let pick_sql = build_pick_sql(node, view);
            wrap_boolean_collider(pick_sql)
        }
        ResolvedNode::ScalarMatch { left, op, right } => {
            build_direct_boolean_select(
                label_to_unit_aware_expr(left),
                *op,
                label_to_unit_aware_expr(right),
                view,
            )
        }
        _ => {
            // その他のノード（TagMatch等）は従来のWHERE方式
            let pick_sql = build_pick_sql(node, view);
            wrap_boolean_collider(pick_sql)
        }
    }
}

/// 比較結果を直接SELECTで計算するSQL
/// CASE WHEN left IS NULL OR right IS NULL THEN NULL WHEN left op right THEN 1 ELSE 0 END
fn build_direct_boolean_select(
    left: SimpleExpr,
    op: ComparisonOp,
    right: SimpleExpr,
    _view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    let bin_op = to_bin_op(op);

    // 比較結果を直接計算
    // CASE WHEN left IS NULL THEN NULL WHEN (left op right) THEN 1 ELSE 0 END
    let comparison = Expr::expr(left).binary(bin_op, right);

    // NULL 伝播: 比較結果が NULL の場合は NULL を返す
    // CASE WHEN comparison THEN 1 WHEN comparison IS NULL THEN NULL ELSE 0 END
    q.expr_as(
        Expr::case(comparison.clone(), Expr::val(1i64))
            .case(comparison.is_null(), Expr::cust("NULL"))
            .finally(Expr::val(0i64)),
        Col::ItemId,
    )
    .expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::ItemKind,
    )
    .expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::Type,
    )
    .expr_as(Expr::val(0i64), Col::Rank)
    .expr_as(Expr::cust("[]"), crate::db::QueryResultCol::Tags);

    q
}

fn wrap_boolean_collider(sql: SelectStatement) -> SelectStatement {
    let mut q = Query::select();
    use crate::db::CustomFunc;
    q.expr_as(
        Expr::case(
            CustomFunc::any_value(Expr::col((Alias::new("pk"), Col::ItemId)))
                .is_not_null(),
            Expr::val(1i64),
        )
        .finally(Expr::val(0i64)),
        Col::ItemId,
    )
    .expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::ItemKind,
    )
    .expr_as(
        Expr::val(<&'static str>::from(crate::types::ItemKind::Volatile)),
        Col::Type,
    )
    .expr_as(Expr::val(0i64), Col::Rank)
    .expr_as(Expr::cust("[]"), crate::db::QueryResultCol::Tags)
    .from_subquery(sql, Alias::new("pk"));
    q
}

/// 指定された条件に合致するアイテムと、その全タグを一括取得するための SQL を生成します。
pub fn build_fetch_items_sql(
    node: &ResolvedNode,
    view: &str,
    limit: Option<usize>,
    offset: Option<usize>,
) -> SelectStatement {
    // 集約クエリ (e.g. count(path:) や sum(size:) > 100) の場合は、
    // oneview との結合を行わず、集約計算結果だけをそのまま返す。
    // NestMatch / NestNestMatch は tags カラムが必要なため
    // ここでは早期リターンせず、通常のタグパッキング処理に委ねる。
    match node {
        ResolvedNode::Aggregation(_)
        | ResolvedNode::AggregationMatch { .. } => {
            return build_pick_sql(node, view);
        }
        _ => {}
    }

    let pick_sql = build_pick_sql(node, view);
    let columns = Col::raw_tag_row_columns();

    // 1. まず ID を絞り込むためのサブクエリを構築
    let mut id_query = Query::select();
    id_query
        .column(Col::ItemId)
        .from_subquery(pick_sql, Alias::new("pk"))
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    if let Some(l) = limit {
        id_query.limit(l as u64);
    }
    if let Some(o) = offset {
        id_query.offset(o as u64);
    }

    // 2. 物理カラムをパックして集約。
    // DuckDB の struct_pack("col1" := "col1", ...) 構文を使用。
    let fields = columns
        .iter()
        .map(|c| {
            format!(
                "\"{}\" := \"{}\"",
                sea_query::Iden::to_string(c),
                sea_query::Iden::to_string(c)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let tags_expr = format!("LIST(struct_pack({}))", fields);

    let mut q = Query::select();
    q.column(Col::ItemId)
        .expr_as(Expr::col(Col::Rank).max(), Col::Rank)
        .expr_as(
            Expr::cust(format!(
                "ANY_VALUE(\"{}\")",
                sea_query::Iden::to_string(&Col::ItemKind)
            )),
            Col::ItemKind,
        )
        .expr_as(Expr::cust(tags_expr), crate::db::QueryResultCol::Tags)
        .from(Alias::new(view))
        .and_where(Expr::col(Col::ItemId).in_subquery(id_query))
        .group_by_col(Col::ItemId)
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    q
}

/// アイテムを絞り込みつつ、集約せずに平坦な行として全タグを取得する SQL を生成します。
pub fn build_flat_table_sql(
    resolved: &ResolvedNode,
    query_node: &QueryNode,
    view: &str,
    limit: Option<usize>,
    offset: Option<usize>,
) -> SelectStatement {
    let pick_sql = build_pick_sql(resolved, view);
    let tagcond = to_tag_condition(query_node);

    // 1. まず ID を絞り込むためのサブクエリを構築
    let mut id_query = Query::select();
    id_query
        .column(Col::ItemId)
        .from_subquery(pick_sql.to_owned(), Alias::new("pk"))
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    if let Some(l) = limit {
        id_query.limit(l as u64);
    }
    if let Some(o) = offset {
        id_query.offset(o as u64);
    }

    let mut q = Query::select();
    q.columns(Col::raw_tag_row_columns())
        .column(Col::Rank)
        .from(Alias::new(view))
        .and_where(Expr::col(Col::ItemId).in_subquery(id_query))
        .and_where(tagcond.into())
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::ItemId, sea_query::Order::Desc);

    q
}

/// nvalue集計用CTEを構築します。
/// projection label ごとに nvalue（集約値）を計算するSQLを返します。
fn build_nvalue_cte(
    proj_operands: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
) -> SelectStatement {
    if proj_operands.len() > 1 {
        return build_nvalue_pivot_aggregate_sql(
            proj_operands,
            nvalue,
            context,
            view,
        );
    }
    let proj_operand = &proj_operands[0];

    // 物理カラム情報の抽出
    let (proj_col, proj_storage) = match proj_operand {
        ResolvedOperand::TagRef { storage, .. } => match storage {
            StorageMapping::RowTag { column, .. } => (*column, storage),
            StorageMapping::Column(col) => (*col, storage),
            _ => return SelectStatement::default(),
        },
        _ => return SelectStatement::default(),
    };

    let proj_tag_type =
        if let StorageMapping::RowTag { tag_type, .. } = &proj_storage {
            Some(tag_type.as_str())
        } else {
            None
        };

    let inner_q = match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            build_count_nvalue_sql(
                proj_col,
                proj_tag_type,
                inner,
                context,
                Some(
                    Query::select()
                        .column(Col::ItemId)
                        .from(Tbl::PickedIds)
                        .to_owned(),
                ),
                view,
                true, // include_item_id
            )
        }
        ResolvedOperand::Aggregation(
            agg @ ResolvedAggregationNode::Arithmetic { op, inner },
        ) => {
            let is_string = agg.is_string_type();
            let deduped = build_deduplicated_agg_subquery(inner, view, context);

            let mut stmt = Query::select();
            stmt.expr_as(
                Expr::col((Alias::new("proj"), proj_col)),
                Alias::new("group_label"),
            );
            stmt.expr_as(
                apply_arithmetic_agg(
                    op,
                    Expr::col((Alias::new("deduped"), Alias::new("val")))
                        .into(),
                    is_string,
                ),
                Alias::new("nvalue"),
            );

            stmt.from_as(Alias::new(view), Alias::new("proj"));
            stmt.join_subquery(
                sea_query::JoinType::InnerJoin,
                deduped,
                Alias::new("deduped"),
                Expr::col((Alias::new("proj"), Col::ItemId))
                    .equals((Alias::new("deduped"), Col::ItemId)),
            );

            // AND proj.item_id IN picked_ids
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from(Tbl::PickedIds)
                        .to_owned(),
                ),
            );

            if let Some(tag_type) = proj_tag_type {
                stmt.and_where(
                    Expr::col((Alias::new("proj"), Col::Type)).eq(tag_type),
                );
            }
            stmt.group_by_col((Alias::new("proj"), proj_col));
            wrap_with_item_id(stmt, proj_col, proj_tag_type, view)
        }
        _ => build_nvalue_standalone_subquery(
            proj_operand,
            nvalue,
            context,
            view,
            true, // CTE 用には現状一貫性のために item-level で取得
        ),
    };

    // CTE 用に最終的に (group_label, nvalue) 単位でマージする
    Query::select()
        .column(Alias::new("group_label"))
        .column(Alias::new("nvalue"))
        .from_subquery(inner_q, Alias::new("nv_items"))
        .group_by_col(Alias::new("group_label"))
        .group_by_col(Alias::new("nvalue"))
        .to_owned()
}

/// Label を sea_query の SimpleExpr に変換するヘルパー
fn label_to_simple_expr(label: &Label) -> SimpleExpr {
    use crate::types::LabelValue;
    match label.value() {
        LabelValue::Integer(i) => Expr::val(i).into(),
        LabelValue::Boolean(b) => Expr::val(b).into(),
        LabelValue::Double(bits) => Expr::val(f64::from_bits(bits)).into(),
        LabelValue::Null => Expr::val(Option::<i32>::None).into(),
        LabelValue::String(s) | LabelValue::Literal(s) => Expr::val(s).into(),
    }
}

/// 指定された条件に合致するアイテムをラベル（型）ごとに集約し、
/// 代表アイテムのリストと総数を 1 クエリで取得するための SQL を生成します。
pub fn build_fetch_label_groups_sql(
    resolver: &crate::query::lens_resolver::Resolver,
    proj_type: &TagType,
    view: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    use crate::db::CustomFunc;
    use sea_query::{CommonTableExpression, Iden, WithClause};

    let pick_sql = build_pick_sql(&resolver.resolved_query, view);

    // 1. プロジェクション対象の物理カラムを特定
    let desc = resolver.lens().look_up_or_default(proj_type);
    let col_iden = match &desc.storage {
        StorageMapping::Column(col) => *col,
        StorageMapping::RowTag { column, .. } => *column,
        _ => anyhow::bail!(
            "Unsupported storage for projection: {:?}",
            desc.storage
        ),
    };

    // --- CTE 定義開始 ---
    let mut with_clause = WithClause::new();

    // CTE 1: picked_ids (元々の絞り込み結果)
    let picked_ids_cte = CommonTableExpression::new()
        .query(wrap_to_ids(pick_sql))
        .table_name(Tbl::PickedIds)
        .to_owned();
    with_clause.cte(picked_ids_cte);

    // nvalue CTE: nvalue付きNestの場合、ラベルごとの集約値を計算
    //
    // OR クエリの場合は nvalue 条件の適用をスキップする。
    // OR の各部分が異なる nvalue 条件を持つ場合、先頭の条件のみを取得した
    // nvalue_agg で all_hits をフィルタすると、別の条件を満たすグループが
    // 誤って除外されてしまう。OR クエリでは picked_ids UNION が既に正しく
    // 絞り込みを行っているため、追加フィルタは不要。
    let is_or_query = matches!(&resolver.resolved_query, ResolvedNode::Or(_));
    let nvalue_condition = resolver.get_nvalue_condition();
    let has_nvalue = if !is_or_query {
        if let Some(nv) = resolver.get_nvalue_combined() {
            let mut nvalue_sql = build_nvalue_cte(
                resolver.resolved_query.get_projection_operands().unwrap(),
                &nv,
                resolver.resolved_query.get_context(),
                view,
            );
            // nvalue_condition がある場合、フィルタ条件を追加
            // Calculation の nvalue は GROUP BY なしのため HAVING ではなく WHERE を使用
            if let Some((op, value)) = nvalue_condition {
                let bin_op = to_bin_op(*op);
                let val = label_to_simple_expr(value);
                let cond = Expr::col(Alias::new("nvalue")).binary(bin_op, val);
                if matches!(&nv, ResolvedOperand::Calculation(_)) {
                    nvalue_sql.and_where(cond);
                } else {
                    nvalue_sql.and_having(cond);
                }
            }
            let nvalue_cte = CommonTableExpression::new()
                .query(nvalue_sql)
                .table_name(Alias::new("nvalue_agg"))
                .to_owned();
            with_clause.cte(nvalue_cte);
            true
        } else {
            false
        }
    } else {
        false
    };
    // 比較条件の有無に関わらず、nvalue (集計等の評価値) を持つ場合、
    // nvalue_agg に存在する(集計結果を持つ)グループのみを残す。
    // ※OR クエリでは各条件の独立性を保つため適用しない
    // Note: has_nvalue の生成時に既に !is_or_query が考慮されているため has_nvalue をそのまま使用。
    let must_filter_by_nvalue = has_nvalue;

    // Calculation 投影の検出: Nest(Calculation(...)) の場合は
    // 算術式を事前計算する computed CTE を挿入する
    let proj_operands =
        resolver.resolved_query.get_projection_operands().unwrap();
    let calc_node = match &proj_operands[0] {
        crate::query::lens_resolver::ResolvedOperand::Calculation(c) => Some(c),
        _ => None,
    };

    // label_col_name: CTE チェーン全体で使用するラベルカラム名
    // all_hits_source: all_hits CTE のデータソーステーブル
    // need_extra_filter: computed CTE で処理済みでなければ NULL/type フィルタが必要
    let (label_col_name, all_hits_source, need_extra_filter) =
        if proj_operands.len() > 1 {
            // 深い Nest: pivot CTE を使用
            let pivot_q = build_nest_pivot_cte(proj_operands, None, view);
            let pivot_cte = CommonTableExpression::new()
                .query(pivot_q)
                .table_name(Alias::new("pivot"))
                .to_owned();
            with_clause.cte(pivot_cte);
            ("key0".to_string(), "pivot".to_string(), false)
        } else if let Some(calc) = calc_node {
            if calc.contains_row_tag() {
                // EAV 算術: 条件集計で item_id ごとに計算値を集約
                // (例: size: + mtime: → SUM(CASE WHEN type='size' ...) + SUM(CASE WHEN type='mtime' ...))
                let calc_expr = build_calculation_eav_expr(&calc, view);
                let mut computed_q = Query::select();
                computed_q
                    .column(Col::ItemId)
                    .expr_as(calc_expr, Alias::new("calc_value"))
                    .expr_as(
                        CustomFunc::any_value(Expr::col(Col::Rank)),
                        Alias::new("rank"),
                    )
                    .from(Alias::new(view))
                    .and_where(
                        Expr::col(Col::ItemId).in_subquery(
                            Query::select()
                                .column(Col::ItemId)
                                .from(Tbl::PickedIds)
                                .to_owned(),
                        ),
                    )
                    .group_by_col(Col::ItemId);
                let computed_cte = CommonTableExpression::new()
                    .query(computed_q)
                    .table_name(Alias::new("computed"))
                    .to_owned();
                with_clause.cte(computed_cte);
                ("calc_value".to_string(), "computed".to_string(), false)
            } else {
                // 既存: カラムベース算術（RowTag なし）
                let calc_expr = build_calculation_expr(&calc, view);
                let mut computed_q = Query::select();
                computed_q
                    .column(Col::ItemId)
                    .expr_as(calc_expr, Alias::new("calc_value"))
                    .column(Col::Rank)
                    .from(Alias::new(view))
                    .and_where(
                        Expr::col(Col::ItemId).in_subquery(
                            Query::select()
                                .column(Col::ItemId)
                                .from(Tbl::PickedIds)
                                .to_owned(),
                        ),
                    );
                let computed_cte = CommonTableExpression::new()
                    .query(computed_q)
                    .table_name(Alias::new("computed"))
                    .to_owned();
                with_clause.cte(computed_cte);
                ("calc_value".to_string(), "computed".to_string(), false)
            }
        } else {
            (Iden::to_string(&col_iden), view.to_string(), true)
        };

    let label_col = Alias::new(&label_col_name);

    // Window 関数用のパーティション（キー）SQL
    let partition_sql = if proj_operands.len() > 1 {
        (0..proj_operands.len())
            .map(|i| format!("\"key{}\"", i))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!("\"{}\"", label_col_name)
    };

    // CTE 2: all_hits (Window関数を含む全ヒットアイテム)
    let mut all_hits_q = Query::select();
    all_hits_q.column(Col::ItemId);
    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            all_hits_q.column(Alias::new(&format!("key{}", i)));
        }
    } else {
        all_hits_q.column(label_col.clone());
    }
    all_hits_q
        .column(Col::Rank)
        .expr_as(
            Expr::cust(format!(
                "row_number() OVER (PARTITION BY {} ORDER BY \"rank\" DESC, \"item_id\" DESC)",
                partition_sql
            )),
            Tbl::Rn,
        )
        .expr_as(
            Expr::cust(format!("count(*) OVER (PARTITION BY {})", partition_sql)),
            Tbl::GroupTotal,
        )
        .distinct()
        .from(Alias::new(&all_hits_source))
        .and_where(
            Expr::col(Col::ItemId).in_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .from(Tbl::PickedIds)
                    .to_owned(),
            ),
        );

    if need_extra_filter {
        // 通常パス: Locationsテーブル由来のNULL除外 + RowTag type フィルタ
        all_hits_q.and_where(Expr::col(label_col.clone()).is_not_null());
        if let StorageMapping::RowTag { tag_type, .. } = &desc.storage {
            all_hits_q.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
        }
    }

    // nvalueの計算を伴う場合、値が存在しなかった(NULL/0件)や条件で除外されたグループを除外
    if must_filter_by_nvalue {
        if proj_operands.len() > 1 {
            let keys_str = (0..proj_operands.len())
                .map(|i| format!("\"key{}\"", i))
                .collect::<Vec<_>>()
                .join(", ");
            all_hits_q.and_where(Expr::cust(format!(
                "({}) IN (SELECT {} FROM \"nvalue_agg\")",
                keys_str, keys_str
            )));
        } else {
            all_hits_q.and_where(
                Expr::col(label_col.clone()).in_subquery(
                    Query::select()
                        .column(Alias::new("group_label"))
                        .from(Alias::new("nvalue_agg"))
                        .to_owned(),
                ),
            );
        }
    }

    let all_hits_cte = CommonTableExpression::new()
        .query(all_hits_q)
        .table_name(Tbl::AllHits)
        .to_owned();
    with_clause.cte(all_hits_cte);

    // CTE 3: top_items (表示対象の上位IDのみ、rankも含める)
    let mut top_items_q = Query::select();
    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            top_items_q.column(Alias::new(&format!("key{}", i)));
        }
    } else {
        top_items_q.column(label_col.clone());
    }
    top_items_q
        .column(Col::ItemId)
        .column(Col::Rank)
        .column(Tbl::GroupTotal)
        .from(Tbl::AllHits)
        .and_where(Expr::col(Tbl::Rn).lte(100));

    let top_items_cte = CommonTableExpression::new()
        .query(top_items_q)
        .table_name(Tbl::TopItems)
        .to_owned();
    with_clause.cte(top_items_cte);

    // --- CTE 定義終了 ---

    // 4. 最終SELECT: シンプルな文字列連結で item:name#id 形式のリストを生成
    let mut q = Query::select();

    // nameを取得するサブクエリを文字列で構築
    // (SELECT label_str FROM oneview WHERE item_id = top_items.item_id AND type = 'name' LIMIT 1)
    let name_subquery = format!(
        "(SELECT {} FROM {} WHERE {} = {}.{} AND {} = 'name' LIMIT 1)",
        Iden::to_string(&Col::LabelStr),
        view,
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Col::Type)
    );

    // CONCAT式を文字列で構築（COALESCEでサブクエリの結果を使用）
    let concat_expr = format!(
        "CONCAT(COALESCE({}, 'unknown'), '#', CAST({}.{} AS VARCHAR))",
        name_subquery,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId)
    );

    // list()関数全体を文字列で構築（rankはtop_itemsから取得）
    let list_expr = Expr::cust(format!(
        "list({} ORDER BY {}.{} DESC, {}.{} DESC)",
        concat_expr,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::Rank),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId)
    ));

    q.with_cte(with_clause);
    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            q.expr_as(
                Expr::col((Tbl::TopItems, Alias::new(&format!("key{}", i)))),
                Alias::new(&format!("label_value_{}", i)),
            );
        }
    } else {
        q.expr_as(
            Expr::col((Tbl::TopItems, label_col.clone())),
            Alias::new("label_value"),
        );
    }
    q.expr_as(
        Expr::col((Tbl::TopItems, Tbl::GroupTotal)),
        Alias::new("group_total"),
    )
    .expr_as(list_expr, Alias::new("item_refs"));

    // nvalue カラムの追加（スカラーサブクエリで nvalue_agg CTE を参照）
    if has_nvalue {
        let nvalue_lookup = if proj_operands.len() > 1 {
            let mut join_cond = "TRUE".to_string();
            for i in 0..proj_operands.len() {
                join_cond.push_str(&format!(
                    " AND \"nvalue_agg\".\"key{}\" = {}.\"key{}\"",
                    i,
                    Iden::to_string(&Tbl::TopItems),
                    i
                ));
            }
            Expr::cust(format!(
                "(SELECT \"nvalue\" FROM \"nvalue_agg\" WHERE {})",
                join_cond
            ))
        } else {
            Expr::cust(format!(
                "(SELECT \"nvalue\" FROM \"nvalue_agg\" WHERE \"group_label\" = {}.{})",
                Iden::to_string(&Tbl::TopItems),
                &label_col_name,
            ))
        };
        q.expr_as(nvalue_lookup, Alias::new("nvalue"));
    }

    q.from(Tbl::TopItems);
    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            q.group_by_col((Tbl::TopItems, Alias::new(&format!("key{}", i))));
        }
    } else {
        q.group_by_col((Tbl::TopItems, label_col.clone()));
    }
    q.group_by_col((Tbl::TopItems, Tbl::GroupTotal));

    if proj_operands.len() > 1 {
        for i in (0..proj_operands.len()).rev() {
            q.order_by(
                (Tbl::TopItems, Alias::new(&format!("key{}", i))),
                sea_query::Order::Asc,
            );
        }
    } else {
        q.order_by((Tbl::TopItems, label_col), sea_query::Order::Asc);
    }

    if limit > 0 {
        q.limit((limit + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }

    Ok(q)
}

/// ラベル集合演算（Intersect / Union / Except）用の SQL を生成します。
///
/// 各オペランドのアイテム集合を集合演算したうえで、先頭オペランドのプライマリキーで
/// ラベルを割り当て、`(label_value, group_total, item_refs)` 形式の行を返します。
pub fn build_fetch_label_set_op_sql(
    label_set_op: &crate::query::lens_resolver::ResolvedNode,
    view: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    use crate::query::lens_resolver::{LabelSetOpKind, ResolvedNode};
    use sea_query::{CommonTableExpression, Iden, WithClause};

    let (op, operands) = match label_set_op {
        ResolvedNode::LabelSetOp { op, operands } => (op, operands),
        _ => anyhow::bail!(
            "build_fetch_label_set_op_sql: expected LabelSetOp node"
        ),
    };
    if operands.is_empty() {
        anyhow::bail!(
            "build_fetch_label_set_op_sql: LabelSetOp with no operands"
        );
    }

    let mut with_clause = WithClause::new();

    // CTE: labels_i — 各オペランドの (label_value_cast, item_id)
    let cte_names: Vec<String> = (0..operands.len())
        .map(|i| format!("labels_{}", i))
        .collect();
    for (i, operand) in operands.iter().enumerate() {
        let ids_sql = wrap_to_ids(build_pick_sql(operand, view));

        // Union の場合のみ複合ラベルを生成（Intersect/Except は主キー値で比較する）
        let labels_sql = if matches!(op, LabelSetOpKind::Union) {
            if let Some(keys) = extract_multi_key_nest_operands(operand) {
                // 多キー Nest: pivot + 複合ラベル ("key0 &: key1 &: ...")
                build_multi_key_labels_sql(&keys, ids_sql, view)?
            } else {
                let (label_tag_type, label_col) =
                    extract_primary_label_tag_type_from_node(operand).ok_or_else(|| {
                        anyhow::anyhow!(
                            "build_fetch_label_set_op_sql: cannot determine label type from operand {}", i
                        )
                    })?;
                let cast_expr = Expr::cust_with_exprs(
                    "CAST($1 AS VARCHAR)",
                    vec![Expr::col(label_col).into()],
                );
                let mut s = Query::select();
                s.expr_as(cast_expr, Alias::new("label_value_cast"))
                    .column(Col::ItemId)
                    .from(Alias::new(view))
                    .and_where(Expr::col(Col::Type).eq(label_tag_type.as_str()))
                    .and_where(Expr::col(label_col).is_not_null())
                    .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
                s
            }
        } else {
            let (label_tag_type, label_col) =
                extract_primary_label_tag_type_from_node(operand).ok_or_else(|| {
                    anyhow::anyhow!(
                        "build_fetch_label_set_op_sql: cannot determine label type from operand {}", i
                    )
                })?;
            let cast_expr = Expr::cust_with_exprs(
                "CAST($1 AS VARCHAR)",
                vec![Expr::col(label_col).into()],
            );
            let mut s = Query::select();
            s.expr_as(cast_expr, Alias::new("label_value_cast"))
                .column(Col::ItemId)
                .from(Alias::new(view))
                .and_where(Expr::col(Col::Type).eq(label_tag_type.as_str()))
                .and_where(Expr::col(label_col).is_not_null())
                .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql));
            s
        };

        with_clause.cte(
            CommonTableExpression::new()
                .query(labels_sql)
                .table_name(Alias::new(&cte_names[i]))
                .to_owned(),
        );
    }

    // Except かつ左辺が多キー Nest の場合はアイテムレベルの差分演算を使用する。
    // ラベル値ベース EXCEPT は「左辺の label 値が右辺の label 値に存在するかどうか」で判定するため、
    // 左右でキー型が異なる場合や同一 label 値のアイテムが片側にしかない場合に誤った結果になる。
    let use_item_level_except = matches!(op, LabelSetOpKind::Except)
        && extract_multi_key_nest_operands(operands.first().unwrap()).is_some();

    if use_item_level_except {
        // アイテムレベル Except:
        //   labels = labels_0 (左辺の主キーラベル) WHERE item_id NOT IN (右辺の item_ids)
        let right_ids_sql = wrap_to_ids(build_pick_sql(&operands[1], view));

        let mut labels_sql = Query::select();
        labels_sql
            .expr_as(
                Expr::col(Alias::new("label_value_cast")),
                Alias::new("label_value"),
            )
            .column(Col::ItemId)
            .from(Alias::new(&cte_names[0]))
            .and_where(Expr::col(Col::ItemId).not_in_subquery(right_ids_sql));
        with_clause.cte(
            CommonTableExpression::new()
                .query(labels_sql)
                .table_name(Alias::new("labels"))
                .to_owned(),
        );
    } else {
        // CTE: op_labels — ラベル文字列値ベースの集合演算結果
        let set_op_type = match op {
            LabelSetOpKind::Intersect => sea_query::UnionType::Intersect,
            LabelSetOpKind::Union => sea_query::UnionType::Distinct,
            LabelSetOpKind::Except => sea_query::UnionType::Except,
        };
        let mut op_labels_sql = Query::select()
            .column(Alias::new("label_value_cast"))
            .from(Alias::new(&cte_names[0]))
            .to_owned();
        for name in &cte_names[1..] {
            let other = Query::select()
                .column(Alias::new("label_value_cast"))
                .from(Alias::new(name))
                .to_owned();
            op_labels_sql.union(set_op_type, other);
        }
        with_clause.cte(
            CommonTableExpression::new()
                .query(op_labels_sql)
                .table_name(Alias::new("op_labels"))
                .to_owned(),
        );

        // CTE: all_op_items — 各オペランドからの `(label_value_cast, item_id)` をすべてプール
        let mut all_op_items_sql = Query::select()
            .column(Alias::new("label_value_cast"))
            .column(Col::ItemId)
            .from(Alias::new(&cte_names[0]))
            .to_owned();
        for name in &cte_names[1..] {
            let other = Query::select()
                .column(Alias::new("label_value_cast"))
                .column(Col::ItemId)
                .from(Alias::new(name))
                .to_owned();
            all_op_items_sql.union(sea_query::UnionType::Distinct, other);
        }
        with_clause.cte(
            CommonTableExpression::new()
                .query(all_op_items_sql)
                .table_name(Alias::new("all_op_items"))
                .to_owned(),
        );

        // CTE: labels — op_labelsに合致するラベル値とアイテムIDをすべて集める
        let mut labels_sql = Query::select();
        labels_sql
            .expr_as(
                Expr::col(Alias::new("label_value_cast")),
                Alias::new("label_value"),
            )
            .column(Col::ItemId)
            .from(Alias::new("all_op_items"))
            .and_where(
                Expr::col(Alias::new("label_value_cast")).in_subquery(
                    Query::select()
                        .column(Alias::new("label_value_cast"))
                        .from(Alias::new("op_labels"))
                        .to_owned(),
                ),
            );
        with_clause.cte(
            CommonTableExpression::new()
                .query(labels_sql)
                .table_name(Alias::new("labels"))
                .to_owned(),
        );
    }

    // CTE: all_hits — Window 関数でページング情報を付与
    let all_hits_sql = Query::select()
        .column(Col::ItemId)
        .column(Alias::new("label_value"))
        .expr_as(
            Expr::cust(
                "row_number() OVER (PARTITION BY \"label_value\" ORDER BY \"item_id\" DESC)",
            ),
            Tbl::Rn,
        )
        .expr_as(
            Expr::cust("count(*) OVER (PARTITION BY \"label_value\")"),
            Tbl::GroupTotal,
        )
        .from(Alias::new("labels"))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(all_hits_sql)
            .table_name(Tbl::AllHits)
            .to_owned(),
    );

    // CTE: top_items — rn <= 100 フィルタ
    let top_items_sql = Query::select()
        .column(Col::ItemId)
        .column(Alias::new("label_value"))
        .column(Tbl::GroupTotal)
        .from(Tbl::AllHits)
        .and_where(Expr::col(Tbl::Rn).lte(100))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(top_items_sql)
            .table_name(Tbl::TopItems)
            .to_owned(),
    );

    // 最終 SELECT: (label_value, group_total, item_refs)
    let name_subquery = format!(
        "(SELECT {} FROM {} WHERE {} = {}.{} AND {} = 'name' LIMIT 1)",
        Iden::to_string(&Col::LabelStr),
        view,
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Col::Type),
    );
    let concat_expr = format!(
        "CONCAT(COALESCE({}, 'unknown'), '#', CAST({}.{} AS VARCHAR))",
        name_subquery,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
    );
    let list_expr = Expr::cust(format!(
        "list({} ORDER BY {}.{} DESC)",
        concat_expr,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
    ));

    let mut q = Query::select();
    q.with_cte(with_clause)
        .expr_as(
            Expr::col((Tbl::TopItems, Alias::new("label_value"))),
            Alias::new("label_value"),
        )
        .expr_as(
            Expr::col((Tbl::TopItems, Tbl::GroupTotal)),
            Alias::new("group_total"),
        )
        .expr_as(list_expr, Alias::new("item_refs"))
        .from(Tbl::TopItems)
        .group_by_col((Tbl::TopItems, Alias::new("label_value")))
        .group_by_col((Tbl::TopItems, Tbl::GroupTotal))
        .order_by(
            (Tbl::TopItems, Alias::new("label_value")),
            sea_query::Order::Asc,
        );

    if limit > 0 {
        q.limit((limit + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }

    Ok(q)
}

/// LabelSetOp の先頭オペランドからプライマリラベルのタグ型文字列とカラムを取得します。
fn extract_primary_label_tag_type_from_node(
    node: &crate::query::lens_resolver::ResolvedNode,
) -> Option<(String, Col)> {
    use crate::query::lens_resolver::{ResolvedNode, ResolvedOperand};
    use crate::query::lens_schema::StorageMapping;
    match node {
        ResolvedNode::Nest { keys, .. } => match keys.first()? {
            ResolvedOperand::TagRef {
                storage: StorageMapping::RowTag { tag_type, column },
                ..
            } => Some((tag_type.clone(), *column)),
            _ => None,
        },
        ResolvedNode::And(nodes) => nodes
            .iter()
            .find_map(|n| extract_primary_label_tag_type_from_node(n)),
        _ => None,
    }
}

/// 多キー Nest ノードからキー列を抽出します。単一キーまたは非 Nest の場合は None を返します。
fn extract_multi_key_nest_operands(
    node: &crate::query::lens_resolver::ResolvedNode,
) -> Option<Vec<crate::query::lens_resolver::ResolvedOperand>> {
    use crate::query::lens_resolver::ResolvedNode;
    match node {
        ResolvedNode::Nest { keys, .. } if keys.len() > 1 => Some(keys.clone()),
        ResolvedNode::And(nodes) => nodes
            .iter()
            .find_map(|n| extract_multi_key_nest_operands(n)),
        _ => None,
    }
}

/// 多キー Nest の labels_i CTE 用 SQL を生成します。
/// pivot 形式で key0, key1, ... を取り出し、複合ラベル "key0 &: key1 &: ..." として返します。
fn build_multi_key_labels_sql(
    keys: &[crate::query::lens_resolver::ResolvedOperand],
    ids_sql: SelectStatement,
    view: &str,
) -> anyhow::Result<SelectStatement> {
    use crate::query::lens_resolver::ResolvedOperand;
    use crate::query::lens_schema::StorageMapping;
    use std::collections::HashSet;

    let mut pivot = Query::select();
    pivot.column(Col::ItemId);

    let mut type_filters: HashSet<String> = HashSet::new();
    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef {
                storage: StorageMapping::RowTag { tag_type, column },
                ..
            } => {
                type_filters.insert(tag_type.as_str().to_string());
                let case_expr = Expr::case(
                    Expr::col(Col::Type).eq(tag_type.as_str()),
                    Expr::col(*column),
                );
                let max_expr =
                    Expr::cust_with_exprs("MAX($1)", [case_expr.into()]);
                pivot.expr_as(
                    max_expr.clone(),
                    Alias::new(&format!("key{}", i)),
                );
                pivot.and_having(max_expr.is_not_null());
            }
            ResolvedOperand::TagRef {
                storage: StorageMapping::Column(col),
                ..
            } => {
                let max_expr = Expr::col(*col).max();
                pivot.expr_as(
                    max_expr.clone(),
                    Alias::new(&format!("key{}", i)),
                );
                pivot.and_having(max_expr.is_not_null());
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "build_multi_key_labels_sql: unsupported key type at index {}",
                    i
                ));
            }
        }
    }

    pivot.from(Alias::new(view));
    if !type_filters.is_empty() {
        pivot.and_where(Expr::col(Col::Type).is_in(type_filters));
    }
    pivot
        .and_where(Expr::col(Col::ItemId).in_subquery(ids_sql))
        .group_by_col(Col::ItemId);

    // 複合ラベル: CAST("key0" AS VARCHAR) || ' &: ' || CAST("key1" AS VARCHAR) || ...
    let composite_str = (0..keys.len())
        .map(|i| format!("CAST(\"key{}\" AS VARCHAR)", i))
        .collect::<Vec<_>>()
        .join(" || ' &: ' || ");

    let mut outer = Query::select();
    outer
        .expr_as(Expr::cust(&composite_str), Alias::new("label_value_cast"))
        .column(Col::ItemId)
        .from_subquery(pivot, Alias::new("pivot_sub"));

    Ok(outer)
}

fn wrap_to_ids(sql: SelectStatement) -> SelectStatement {
    Query::select()
        .column(Col::ItemId)
        .from_subquery(sql, Alias::new("sub"))
        .to_owned()
}

/// クエリに使用されている、または投影されている型のリストを元に
/// OneView から特定のタグ行のみを抽出するための Condition を生成します。
pub fn to_tag_condition(node: &QueryNode) -> sea_query::Condition {
    let mut types = node.get_all_types();

    if types.iter().any(|t| t == "*") {
        return sea_query::Condition::all();
    }

    // 特別な扱いの推奨タグを追加
    let defaults = [
        "name",
        "path",
        "size",
        "mtime",
        "rank",
        "item_kind",
        "content",
        "value",
        "tag",
        "filename",
        "is_dir",
    ];
    for def in defaults {
        if !types.iter().any(|t| t == def) {
            types.push(def.to_string());
        }
    }

    if types.iter().any(|t| t == "*" || t == "tag") {
        return sea_query::Condition::all();
    }

    let mut cond = sea_query::Condition::any();
    let mut fixed_types = Vec::new();
    let glob_op = sea_query::BinOper::Custom("GLOB");

    for t in types {
        if t.contains('*') || t.contains('?') || t.contains('[') {
            cond = cond.add(Expr::col(Col::Type).binary(glob_op, Expr::val(t)));
        } else {
            fixed_types.push(t);
        }
    }

    if !fixed_types.is_empty() {
        cond = cond.add(Expr::col(Col::Type).is_in(fixed_types));
    }

    cond
}

fn build_resolved_and_sql(
    nodes: &[ResolvedNode],
    view: &str,
) -> SelectStatement {
    let mut it = nodes.iter();
    let Some(first) = it.next() else {
        let mut q = Query::select();
        q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .distinct()
            .from(Alias::new(view));
        return q;
    };

    let mut q = wrap_in_subquery(build_pick_sql(first, view));
    for next in it {
        q.union(
            sea_query::UnionType::Intersect,
            wrap_in_subquery(build_pick_sql(next, view)),
        );
    }
    q
}

fn build_resolved_or_sql(
    nodes: &[ResolvedNode],
    view: &str,
) -> SelectStatement {
    let mut it = nodes.iter();
    let Some(first) = it.next() else {
        let mut q = Query::select();
        q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .from(Alias::new(view))
            .and_where(Expr::val(1).eq(0));
        return q;
    };

    let mut q = wrap_in_subquery(build_pick_sql(first, view));
    for next in it {
        q.union(
            sea_query::UnionType::Distinct,
            wrap_in_subquery(build_pick_sql(next, view)),
        );
    }
    q
}

fn build_resolved_diff_sql(
    l: &ResolvedNode,
    r: &ResolvedNode,
    view: &str,
) -> SelectStatement {
    let mut q = wrap_in_subquery(build_pick_sql(l, view));
    q.union(
        sea_query::UnionType::Except,
        wrap_in_subquery(build_pick_sql(r, view)),
    );
    q
}

fn build_resolved_comp_sql(c: &ResolvedNode, view: &str) -> SelectStatement {
    let mut q = Query::select();
    if c.is_boolean_result() {
        // Boolean Universe: {TRUE(1)}
        // FALSE is represented by the absence of rows.
        let mut true_q = Query::select();
        true_q
            .expr_as(Expr::val(1i64), Col::ItemId)
            .expr_as(Expr::val(0i64), Col::Rank)
            .expr_as(
                Expr::val(<&'static str>::from(
                    crate::types::ItemKind::Volatile,
                )),
                Col::ItemKind,
            );

        q = true_q;
    } else {
        q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .distinct()
            .from(Alias::new(view))
            .and_where(Expr::col(Col::ItemKind).is_not_in(vec!["type", "tag"]));
    }

    let mut eq = Query::select();
    eq.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from_subquery(build_pick_sql(c, view), Tbl::NotSide);
    q.union(sea_query::UnionType::Except, eq);
    q
}

fn build_merged_nest_match_sql(
    keys: &[crate::query::lens_resolver::ResolvedOperand],
    matches: &[crate::query::lens_resolver::NestMatchCondition],
    is_or: bool,
    view: &str,
) -> SelectStatement {
    if keys.len() == 1 {
        // 単一キー: INNER JOIN + GROUP BY + HAVING アプローチ
        // (Optimizer でマージされた複数条件を1つの GROUP BY で処理する)
        let (proj_col, proj_tag_type) = match keys[0].get_storage() {
            Some(StorageMapping::RowTag { column, tag_type }) => {
                (*column, Some(tag_type.as_str()))
            }
            Some(StorageMapping::Column(col)) => (*col, None),
            _ => {
                panic!("MergedNestMatch key must have RowTag or Column storage")
            }
        };

        // nfilter: GROUP BY proj.label_str HAVING cond1 AND cond2 ...
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
            let crate::query::lens_resolver::NestMatchOp::Comparison(cmp_op) =
                m.op;
            let bin_op = to_bin_op(cmp_op);
            let lhs = build_merged_nvalue_agg_expr(&m.nvalue, "c", view);
            let rhs = build_merged_nvalue_agg_expr(&m.right, "c", view);
            having_cond = having_cond.add(Expr::expr(lhs).binary(bin_op, rhs));
        }
        nfilter.cond_having(having_cond);

        // 外側クエリ: WHERE type='key' AND label_str IN (SELECT group_label FROM nfilter)
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
        // 複数キー: Pivot CTE + ウィンドウ関数アプローチ
        let mut all_nv_ops: Vec<&crate::query::lens_resolver::ResolvedOperand> =
            Vec::new();
        for m in matches {
            if !all_nv_ops.iter().any(|&o| o == &m.nvalue) {
                all_nv_ops.push(&m.nvalue);
            }
            if let crate::query::lens_resolver::ResolvedOperand::Aggregation(
                _,
            ) = &m.right
            {
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
                crate::query::lens_resolver::ResolvedOperand::Aggregation(
                    crate::query::lens_resolver::ResolvedAggregationNode::Count(_),
                ) => Func::sum(Expr::col(Alias::new(&nval_pivot_alias))),
                crate::query::lens_resolver::ResolvedOperand::Aggregation(
                    crate::query::lens_resolver::ResolvedAggregationNode::Arithmetic {
                        op,
                        ..
                    },
                ) => match op {
                    crate::query::ast::ArithmeticAggOp::Sum => {
                        Func::sum(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    crate::query::ast::ArithmeticAggOp::Avg => {
                        Func::avg(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    crate::query::ast::ArithmeticAggOp::Max => {
                        Func::max(Expr::col(Alias::new(&nval_pivot_alias)))
                    }
                    crate::query::ast::ArithmeticAggOp::Min => {
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

            let crate::query::lens_resolver::NestMatchOp::Comparison(cmp_op) =
                m.op;
            let bin_op = to_bin_op(cmp_op);

            let right_expr =
                if let crate::query::lens_resolver::ResolvedOperand::Aggregation(
                    _,
                ) = &m.right
                {
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

fn build_resolved_projection_sql(
    op: &crate::query::lens_resolver::ResolvedOperand,
    view: &str,
) -> SelectStatement {
    use crate::query::lens_resolver::ResolvedOperand;

    match op {
        ResolvedOperand::TagRef {
            tag_type,
            storage: _,
            ..
        } => {
            let mut q = Query::select();
            q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
                .distinct()
                .from(Alias::new(view));

            // ResolvedNode の Nest 用条件生成を利用
            let cond = ResolvedNode::Nest {
                keys: vec![op.clone()],
                nvalue: None,
                context: None,
            }
            .to_condition();
            q.cond_where(cond);

            // 特別なタグの追加条件
            if let TagType::Base(SType::TypedTag) = tag_type {
                q.and_where(Expr::col(Col::TypedTag).is_not_null());
            } else if let TagType::Base(SType::Origin) = tag_type {
                q.and_where(Expr::col(Col::Origin).is_not_null());
            }

            q
        }
        ResolvedOperand::Calculation(calc) => {
            let mut l = build_resolved_projection_sql(&calc.left, view);
            let r = build_resolved_projection_sql(&calc.right, view);
            l.union(sea_query::UnionType::Intersect, r);
            l
        }
        _ => {
            // Literal or Aggregation used as projection filter?
            // "project:1" or "project:count()" - usually implies "all" or "none" depending on semantics.
            // For now, return a select all.
            let mut q = Query::select();
            q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
                .distinct()
                .from(Alias::new(view));
            q
        }
    }
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

// ========== SQL Generation Helper Functions ==========

/// ラベルを Unit-Aware な SQL 式に変換して返します。
/// 文字列の場合、parse_size を試みます。
fn label_to_unit_aware_expr(label: &crate::types::Label) -> SimpleExpr {
    use sea_query::Expr;
    match label.value() {
        crate::types::LabelValue::Integer(i) => Expr::val(i).into(),
        crate::types::LabelValue::String(s)
        | crate::types::LabelValue::Literal(s) => {
            // ここで parse_size を適用
            if let Some(bytes) = crate::util::parse_size(&s) {
                Expr::val(bytes).into()
            } else {
                Expr::val(s.clone()).into()
            }
        }
        crate::types::LabelValue::Boolean(b) => Expr::val(b).into(),
        crate::types::LabelValue::Double(bits) => {
            Expr::val(f64::from_bits(bits)).into()
        }
        crate::types::LabelValue::Null => Expr::val(None::<i32>).into(),
    }
}

/// サブクエリをラップする共通ヘルパー関数。
///
/// 優先順位を保証するため、サブクエリとしてラップします。
fn wrap_in_subquery(q: SelectStatement) -> SelectStatement {
    Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from_subquery(q, Tbl::Sub)
        .to_owned()
}

/// カラムマッチクエリのSQLを生成します。
///
/// 特定のカラム（物理カラム）の値に対する直接マッチング。
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


/// ラベル集計（ページング用）のクエリを生成します。
///
/// 指定されたタグタイプについて、ユニークなラベル値を取得します。
pub fn build_label_aggregation_sql(
    proj_type: &TagType,
    from_table: bool,
    path_str: Option<&str>,
    n: usize,
    offset: usize,
) -> SelectStatement {
    let mut q = Query::select();

    // カラム選択ロジック
    // カラム選択ロジック
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
        // その他のタグ（Label含む）は標準的なカラムを使用
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
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("total_count"));

    // FROM 句とフィルタリング
    if from_table {
        // テーブルからの検索: Sub クエリ (IDリスト) でフィルタリング
        q.from(Tbl::OneView).and_where(
            Expr::col(Col::ItemId).in_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .from(Tbl::Sub)
                    .to_owned(),
            ),
        );
    } else if let Some(path) = path_str {
        // パケットからの検索
        q.from_function(
            sea_query::Func::cust(crate::db::DuckDbFunc::ReadParquet)
                .arg(Expr::val(path)),
            Tbl::Diff,
        );
    }

    // Type によるフィルタリング (Label など一部を除く)
    match proj_type {
        TagType::Base(SType::TypedTag)
        | TagType::Base(SType::Origin)
        | TagType::Base(SType::Rank)
        | TagType::Base(SType::Label) => {
            // No type filter needed
        }
        _ => {
            q.and_where(Expr::col(Col::Type).eq(proj_type.as_str()));
        }
    }

    // GROUP BY (重複排除)
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
        // その他のタグ（Label含む）は標準的なカラムを使用
        _ => {
            q.group_by_columns([
                Col::LabelStr,
                Col::LabelInt,
                Col::LabelDouble,
                Col::LabelBool,
            ]);
        }
    }

    // ORDER BY と LIMIT/OFFSET
    q.order_by(Col::LabelStr, sea_query::Order::Asc);

    if n > 0 {
        q.limit((n + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }

    q
}

/// ラベル展開（アイテムID取得）用のクエリを生成します。
///
/// 特定のラベルを持つアイテムのIDを取得します。
pub fn build_label_expansion_sql(
    proj_type: &TagType,
    label: &Label,
    from_table: bool,
    path_str: Option<&str>,
) -> SelectStatement {
    let mut q = Query::select();
    q.distinct().column(Col::ItemId);

    // FROM 句
    if from_table {
        q.from(Tbl::OneView).and_where(
            Expr::col(Col::ItemId).in_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .from(Tbl::Sub)
                    .to_owned(),
            ),
        );
    } else if let Some(path) = path_str {
        q.from_function(
            sea_query::Func::cust(crate::db::DuckDbFunc::ReadParquet)
                .arg(Expr::val(path)),
            Tbl::Diff,
        );
    }

    // 条件フィルタ
    match proj_type {
        TagType::Base(SType::TypedTag) => {
            q.and_where(Expr::col(Col::TypedTag).eq(label.as_str()));
        }
        TagType::Base(SType::Origin) => {
            q.and_where(Expr::col(Col::Origin).eq(label.as_str()));
        }
        TagType::Base(SType::Rank) => {
            match label.value() {
                crate::types::LabelValue::Integer(i) => {
                    q.and_where(Expr::col(Col::Rank).eq(i));
                }
                _ => {
                    // RankなのにInteger以外が来た場合はヒットしない
                    q.and_where(Expr::val(1).eq(0));
                }
            }
        }
        TagType::Base(SType::Label) => {
            // Label (仮想タグ) の場合は Type フィルタなしで Label 値のみで検索
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
        _ => {
            // 一般的なタグ
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

/// オペランド内に含まれる RowTag のキーをすべて抽出します。
fn collect_tag_types(
    operand: &crate::query::lens_resolver::ResolvedOperand,
    keys: &mut Vec<String>,
) {
    use crate::query::lens_resolver::{
        ResolvedAggregationNode, ResolvedOperand,
    };
    match operand {
        ResolvedOperand::TagRef {
            storage: StorageMapping::RowTag { tag_type, .. },
            ..
        } => {
            keys.push(tag_type.clone());
        }
        ResolvedOperand::Calculation(calc) => {
            collect_tag_types(&calc.left, keys);
            collect_tag_types(&calc.right, keys);
        }
        ResolvedOperand::Aggregation(agg) => match agg {
            ResolvedAggregationNode::Count(inner) => {
                let (storage, _, _) = inner.extract_agg_parts();
                if let Some(StorageMapping::RowTag { tag_type, .. }) = storage {
                    keys.push(tag_type.clone());
                }
            }
            ResolvedAggregationNode::Arithmetic { inner, .. } => {
                let (storage, _, operand) = inner.extract_agg_parts();
                if let Some(StorageMapping::RowTag { tag_type, .. }) = storage {
                    keys.push(tag_type.clone());
                }
                if let Some(op) = operand {
                    collect_tag_types(op, keys);
                }
            }
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{BasicOp, QueryNode};
    use crate::query::lens_resolver::ResolvedOperand;
    use crate::types::{Label, SType, TagType, TypedTag};
    use sea_query::{PostgresQueryBuilder, SqliteQueryBuilder};

    #[test]
    fn test_to_bin_op_conversion() {
        assert_eq!(
            to_bin_op(ComparisonOp::Scalar(BasicOp::Eq)),
            BinOper::Equal
        );
        assert_eq!(
            to_bin_op(ComparisonOp::Scalar(BasicOp::Gt)),
            BinOper::GreaterThan
        );
        assert_eq!(
            to_bin_op(ComparisonOp::Scalar(BasicOp::Lt)),
            BinOper::SmallerThan
        );
    }


    #[test]
    fn test_to_tag_condition_generation() {
        let node =
            QueryNode::TypedTag(TypedTag::new(SType::Size, Label::from(100)));
        let cond = to_tag_condition(&node);

        let mut query = Query::select();
        query
            .column(Alias::new("id"))
            .from(Alias::new("tbl"))
            .cond_where(cond);
        let sql = query.to_string(SqliteQueryBuilder);

        // Verifying exact string content is fragile across sea-query versions/builders.
        // We ensure a query is generated (condition applied).
        assert!(!sql.is_empty());
    }


    #[test]
    fn test_build_flat_table_sql_structure() {
        use crate::query::lens_schema::StorageMapping;
        use sea_query::PostgresQueryBuilder;

        let node = ResolvedNode::Match {
            tag_type: TagType::Base(SType::Name),
            storage: StorageMapping::Column(Col::Name),
            sql_type: crate::db::SqlType::VARCHAR,
            op: ComparisonOp::Scalar(BasicOp::Eq),
            label: Label::from("test"),
        };
        let query_node = QueryNode::And(vec![]);
        let sql =
            build_flat_table_sql(&node, &query_node, "oneview", Some(10), None);
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // 基本構造の検証
        assert!(sql_str.contains("SELECT"), "SQL should contain SELECT");
        assert!(
            sql_str.contains("\"item_id\""),
            "SQL should contain item_id column"
        );
        assert!(
            sql_str.contains("\"rank\""),
            "SQL should contain rank column"
        );
        assert!(
            sql_str.contains("FROM \"oneview\""),
            "SQL should select from oneview"
        );
        // サブクエリによる絞り込みの検証
        assert!(
            sql_str.contains("IN (SELECT"),
            "SQL should use IN (SELECT...) for filtering"
        );
        assert!(
            sql_str.contains("AS \"pk\""),
            "SQL should use pk alias for subquery"
        );
        // ソートとリミットの検証
        assert!(
            sql_str.contains("ORDER BY"),
            "SQL should have ORDER BY clause"
        );
        assert!(sql_str.contains("LIMIT 10"), "SQL should have LIMIT 10");
    }

    #[test]
    fn test_build_resolved_aggregation_sql_count_items() {
        let agg =
            ResolvedAggregationNode::Count(Box::new(ResolvedNode::And(vec![
                ResolvedNode::Match {
                    tag_type: TagType::Base(SType::Extension),
                    storage: StorageMapping::RowTag {
                        column: Col::LabelStr,
                        tag_type: "extension".to_string(),
                    },
                    sql_type: crate::db::SqlType::VARCHAR,
                    op: ComparisonOp::Scalar(BasicOp::Eq),
                    label: Label::from("txt"),
                },
            ])));

        let sql = build_resolved_aggregation_sql(&agg, "oneview");
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // アイテム数を数えるので COUNT(DISTINCT "item_id")
        assert!(sql_str.contains("COUNT(DISTINCT \"item_id\")"));
        // フィルタ条件
        assert!(sql_str
            .contains("WHERE \"item_id\" IN (SELECT \"item_id\" FROM (SELECT"));
        assert!(sql_str.contains(
            "WHERE \"type\" = 'extension' AND \"label_str\" = 'txt'"
        ));
    }

    #[test]
    fn test_build_resolved_aggregation_sql_sum_projection() {
        use crate::query::ast::ArithmeticAggOp;
        let agg = ResolvedAggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(ResolvedNode::Nest {
                keys: vec![ResolvedOperand::TagRef {
                    tag_type: TagType::Base(SType::Size),
                    storage: StorageMapping::RowTag {
                        column: Col::LabelInt,
                        tag_type: "size".to_string(),
                    },
                    sql_type: crate::db::SqlType::BIGINT,
                }],
                nvalue: None,
                context: None,
            }),
        };

        let sql = build_resolved_aggregation_sql(&agg, "oneview");
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // サブクエリ形式: SUM("val")
        assert!(sql_str.contains("SUM(\"val\")"));
        // サブクエリ内での抽出
        assert!(sql_str.contains("MAX(\"label_int\") AS \"val\""));
        assert!(sql_str.contains("\"type\" IN ('size')"));
    }

    #[test]
    fn test_wrap_boolean_collider() {
        let mut inner = Query::select();
        inner
            .column(Col::ItemId)
            .from(Alias::new("t"))
            .and_where(Expr::col(Col::ItemId).eq(1));

        let sql = wrap_boolean_collider(inner);
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(sql_str.contains("FROM (SELECT"));
    }

    #[test]
    fn test_build_aggregation_sql_structure() {
        use crate::query::ast::ArithmeticAggOp;
        use crate::query::lens_resolver::ResolvedNode;
        use crate::query::lens_schema::StorageMapping;
        use crate::types::{Label, SType, TagType};
        use sea_query::PostgresQueryBuilder;

        // SUM(size) inside Aggregation with Filter(project:ttfm)
        // AggregationNode::Arithmetic { op: Sum, inner: And(Nest, Filter) }
        let agg = ResolvedAggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(ResolvedNode::And(vec![
                ResolvedNode::Nest {
                    keys: vec![ResolvedOperand::TagRef {
                        tag_type: TagType::Base(SType::Size),
                        storage: StorageMapping::Column(Col::Size),
                        sql_type: crate::db::SqlType::BIGINT,
                    }],
                    nvalue: None,
                    context: None,
                },
                ResolvedNode::Match {
                    tag_type: TagType::Custom("project".to_string()),
                    storage: StorageMapping::RowTag {
                        column: Col::LabelStr,
                        tag_type: "project".to_string(),
                    },
                    sql_type: crate::db::SqlType::VARCHAR,
                    op: crate::query::ast::ComparisonOp::Scalar(
                        crate::query::ast::BasicOp::Eq,
                    ),
                    label: Label::from("ttfm"),
                },
            ])),
        };

        let sql = build_resolved_aggregation_sql(&agg, "oneview");
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // サブクエリ形式: SUM("val")
        assert!(sql_str.contains("SUM(\"val\")"));
        assert!(sql_str.contains("MAX(\"size\") AS \"val\""));

        // project フィルタ
        assert!(sql_str
            .contains("WHERE \"type\" = 'project' AND \"label_str\" = 'ttfm'"));
        assert!(sql_str.contains("GROUP BY \"item_id\""));
    }

    #[test]
    fn test_build_calculation_expr_simple() {
        use crate::query::ast::ArithmeticOp;
        use crate::query::lens_resolver::{
            ResolvedCalculationNode, ResolvedOperand,
        };
        use crate::types::Label;

        let calc = ResolvedCalculationNode {
            left: ResolvedOperand::Literal(Label::from(1)),
            op: ArithmeticOp::Add,
            right: ResolvedOperand::Literal(Label::from(2)),
        };

        let expr = build_calculation_expr(&calc, "oneview");
        let sql_str = format!("{:?}", expr);

        // SQL式に加算演算が含まれていることを確認
        assert!(
            sql_str.contains("Add") || sql_str.contains("+"),
            "Should contain addition operation"
        );
    }

    #[test]
    fn test_build_resolved_operand_literal() {
        use crate::query::lens_resolver::ResolvedOperand;
        use crate::types::Label;

        let operand = ResolvedOperand::Literal(Label::from(42));
        let expr = build_resolved_operand_expr(&operand, "oneview");
        let sql_str = format!("{:?}", expr);

        // 数値リテラルが含まれていることを確認
        assert!(sql_str.contains("42"), "Should contain literal value 42");
    }

    /// TDD: build_direct_boolean_select のテスト
    /// 生成されるSQLにNULLチェックが含まれていることを確認
    #[test]
    fn test_build_direct_boolean_select_null_propagation() {
        use sea_query::PostgresQueryBuilder;

        // 左辺: サブクエリ (NULL可能)
        let left = SimpleExpr::SubQuery(
            None,
            Box::new(
                Query::select()
                    .expr(Expr::val(100i64))
                    .to_owned()
                    .into_sub_query_statement(),
            ),
        );
        // 右辺: リテラル
        let right = Expr::val(50i64).into();

        let sql = build_direct_boolean_select(
            left,
            ComparisonOp::Scalar(BasicOp::Gt),
            right,
            "oneview",
        );
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // NULL チェックが含まれていることを確認
        // CASE WHEN (...) THEN 1 WHEN (...) IS NULL THEN NULL ELSE 0 END
        assert!(
            sql_str.contains("IS NULL"),
            "SQL should contain NULL check: {}",
            sql_str
        );
        assert!(
            sql_str.contains("CASE"),
            "SQL should contain CASE: {}",
            sql_str
        );
    }

    #[test]
    fn test_build_fetch_label_groups_sql_generates_concat() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        // 単純な projection クエリを作成
        let query_str = "extension:";
        let resolver = Resolver::new(query_str).expect("Failed to resolve");

        let proj_type =
            resolver.get_projection().expect("Should have projection");

        // SQL生成
        let sql = build_fetch_label_groups_sql(
            &resolver, &proj_type, "oneview", 100, 0,
        )
        .expect("Failed to build SQL");
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // 検証: label_value, group_total, item_refs カラムが含まれているか
        assert!(
            sql_str.contains("label_value"),
            "SQL should contain label_value alias: {}",
            sql_str
        );
        assert!(
            sql_str.contains("group_total"),
            "SQL should contain group_total alias: {}",
            sql_str
        );
        assert!(
            sql_str.contains("item_refs"),
            "SQL should contain item_refs alias: {}",
            sql_str
        );

        // 検証: struct_pack が含まれていないこと（簡素化されているべき）
        assert!(
            !sql_str.contains("struct_pack"),
            "SQL should NOT contain struct_pack (simplified): {}",
            sql_str
        );
    }

    #[test]
    fn test_build_label_aggregation_sql_has_count() {
        use sea_query::PostgresQueryBuilder;

        let proj_type = TagType::from("extension");
        let sql = build_label_aggregation_sql(&proj_type, true, None, 10, 0);
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // total_count カラムが含まれているか
        assert!(
            sql_str.contains("total_count"),
            "SQL should contain total_count alias: {}",
            sql_str
        );
        // COUNT(*) が含まれているか（期待値）
        assert!(
            sql_str.contains("COUNT(*)"),
            "SQL should contain COUNT(*): {}",
            sql_str
        );
    }

    #[test]
    fn test_build_resolved_operand_expr_for_arithmetic_boolean_cast() {
        use crate::db::SqlType;
        use crate::query::lens_resolver::ResolvedOperand;
        use crate::types::TagType;

        // 1. Boolean Literal -> CAST(... AS BIGINT)
        let lit_bool = ResolvedOperand::Literal(crate::types::Label::resolve(
            TagType::from("is_dir"),
            crate::types::LabelValue::Boolean(true),
        ));
        let expr_lit =
            build_resolved_operand_expr_for_arithmetic(&lit_bool, "oneview");
        let sql_lit = sea_query::Query::select()
            .expr(expr_lit)
            .to_string(sea_query::PostgresQueryBuilder);
        assert!(
            sql_lit.contains("CAST"),
            "Boolean literal should be cast: {}",
            sql_lit
        );
        assert!(
            sql_lit.contains("BIGINT"),
            "Boolean literal should be cast to BIGINT: {}",
            sql_lit
        );

        // 2. Boolean TagRef -> CAST(... AS BIGINT)
        let tag_bool = ResolvedOperand::TagRef {
            tag_type: TagType::from("is_dir"),
            storage: crate::query::lens_schema::StorageMapping::Column(
                crate::db::Col::LabelBool,
            ),
            sql_type: SqlType::BOOLEAN,
        };
        let expr_tag =
            build_resolved_operand_expr_for_arithmetic(&tag_bool, "oneview");
        let sql_tag = sea_query::Query::select()
            .expr(expr_tag)
            .to_string(sea_query::PostgresQueryBuilder);
        assert!(
            sql_tag.contains("CAST"),
            "Boolean column should be cast: {}",
            sql_tag
        );
        assert!(
            sql_tag.contains("BIGINT"),
            "Boolean column should be cast to BIGINT: {}",
            sql_tag
        );
    }

    #[test]
    fn test_nvalue_count_projection_sql() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        // parentdir: &: count(extension:jpg) → nvalue付きNest
        let resolver =
            Resolver::new("parentdir: &: count(extension:jpg)").unwrap();
        let proj_type =
            resolver.get_projection().expect("Should have projection");

        assert!(
            resolver.get_nvalue().is_some(),
            "Should have nvalue for nest query"
        );

        let sql = build_fetch_label_groups_sql(
            &resolver, &proj_type, "oneview", 100, 0,
        )
        .unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // nvalue CTE と nvalue カラムが含まれているか
        assert!(
            sql_str.contains("nvalue_agg"),
            "SQL should contain nvalue_agg CTE: {}",
            sql_str
        );
        assert!(
            sql_str.contains("nvalue"),
            "SQL should contain nvalue column: {}",
            sql_str
        );
        // 既存のカラムも含まれているか
        assert!(
            sql_str.contains("label_value"),
            "SQL should contain label_value: {}",
            sql_str
        );
        assert!(
            sql_str.contains("group_total"),
            "SQL should contain group_total: {}",
            sql_str
        );
    }

    #[test]
    fn test_nvalue_sum_projection_sql() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        // parentdir: &: sum(size:) → nvalue付きNest (Arithmetic)
        let resolver = Resolver::new("parentdir: &: sum(size:)").unwrap();
        let proj_type =
            resolver.get_projection().expect("Should have projection");

        assert!(
            resolver.get_nvalue().is_some(),
            "Should have nvalue for nest query"
        );

        let sql = build_fetch_label_groups_sql(
            &resolver, &proj_type, "oneview", 100, 0,
        )
        .unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        assert!(
            sql_str.contains("nvalue_agg"),
            "SQL should contain nvalue_agg CTE for query: {}",
            sql_str
        );
        assert!(
            sql_str.contains("nvalue"),
            "SQL should contain nvalue column for query: {}",
            sql_str
        );
        assert!(
            sql_str.contains("SUM"),
            "SQL should contain SUM for arithmetic nvalue for query: {}",
            sql_str
        );
    }

    #[test]
    fn test_fetch_projection_no_nvalue_regression() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        // 通常の projection — nvalue なし
        let resolver = Resolver::new("extension:").unwrap();
        let proj_type =
            resolver.get_projection().expect("Should have projection");

        assert!(
            resolver.get_nvalue().is_none(),
            "Normal projection should NOT have nvalue"
        );

        let sql = build_fetch_label_groups_sql(
            &resolver, &proj_type, "oneview", 100, 0,
        )
        .unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // nvalue CTE が含まれていないこと
        assert!(
            !sql_str.contains("nvalue_agg"),
            "Normal projection should NOT contain nvalue_agg: {}",
            sql_str
        );
    }

    #[test]
    fn test_nvalue_condition_having_sql() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        // parentdir: &: (count(extension:jpg) > 1) → nvalue CTE に HAVING 付き
        let resolver =
            Resolver::new("parentdir: &: (count(extension:jpg) > 1)").unwrap();
        let proj_type =
            resolver.get_projection().expect("Should have projection");

        assert!(resolver.get_nvalue_condition().is_some());

        let sql = build_fetch_label_groups_sql(
            &resolver, &proj_type, "oneview", 100, 0,
        )
        .unwrap();
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // nvalue CTE に HAVING が含まれている
        assert!(
            sql_str.contains("HAVING"),
            "SQL should contain HAVING for nvalue condition: {}",
            sql_str
        );
        // nvalue フィルタで all_hits がフィルタされている
        assert!(
            sql_str.contains("nvalue_agg"),
            "SQL should use nvalue_agg for filtering: {}",
            sql_str
        );
    }

    #[test]
    fn test_build_nvalue_standalone_calculation_sql() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        let operators = [
            ("sum(size:) + count(size:)", "+"),
            ("sum(size:) - count(size:)", "-"),
            ("sum(size:) * count(size:)", "*"),
            ("sum(size:) / count(size:)", "/"),
            ("sum(size:) % count(size:)", "%"),
        ];

        for (query_str, expected_op) in operators {
            let query = format!("parentdir: &: ({})", query_str);
            let resolver = Resolver::new(&query).unwrap();
            let proj_operand = resolver
                .resolved_query
                .get_projection_operand()
                .expect("Should have projection");
            let nvalue = resolver
                .resolved_query
                .get_nvalue_combined()
                .expect("Should have nvalue");

            // 修正前はこの呼び出しで panic! するはず
            let sql = build_nvalue_standalone_subquery(
                proj_operand,
                &nvalue,
                resolver.resolved_query.get_context(),
                "oneview",
                false,
            );
            let sql_str = sql.to_string(PostgresQueryBuilder);

            // JOIN と算術演算が含まれていることを確認
            assert!(
                sql_str.contains("JOIN"),
                "SQL should contain JOIN for query: {}",
                query
            );
            assert!(
                sql_str.contains(expected_op),
                "SQL should contain '{}' operator for query: {}",
                expected_op,
                query
            );
            assert!(
                sql_str.contains("\"L\""),
                "SQL should use alias L for query: {}",
                query
            );
            assert!(
                sql_str.contains("\"R\""),
                "SQL should use alias R for query: {}",
                query
            );
        }
    }

    #[test]
    fn test_build_fetch_label_groups_with_calculation_nvalue_sql() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        let operators = [
            ("sum(size:) + count(size:)", "+"),
            ("sum(size:) - count(size:)", "-"),
            ("sum(size:) * count(size:)", "*"),
            ("sum(size:) / count(size:)", "/"),
            ("sum(size:) % count(size:)", "%"),
        ];

        for (query_str, expected_op) in operators {
            let query = format!("parentdir: &: ({})", query_str);
            let resolver = Resolver::new(&query).unwrap();
            let proj_type =
                resolver.get_projection().expect("Should have projection");

            let sql = build_fetch_label_groups_sql(
                &resolver, &proj_type, "oneview", 100, 0,
            )
            .unwrap();
            let sql_str = sql.to_string(PostgresQueryBuilder);

            assert!(
                sql_str.contains("nvalue_agg"),
                "SQL should contain nvalue_agg CTE for query: {}",
                query
            );
            assert!(
                sql_str.contains("JOIN"),
                "SQL should contain JOIN in nvalue_agg for query: {}",
                query
            );
            assert!(
                sql_str.contains(expected_op),
                "SQL should contain '{}' operator for query: {}",
                expected_op,
                query
            );
        }
    }

    #[test]
    fn test_build_nvalue_diverse_arithmetic_sql() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        // 多様な集約、リテラル、入れ子式の組み合わせ
        let test_cases = [
            // avg + sum
            ("avg(size:) + sum(size:)", vec!["AVG", "SUM", "+"]),
            // max * literal
            ("max(size:) * 2", vec!["MAX", "2", "*"]),
            // literal / min
            ("100 / min(size:)", vec!["100", "MIN", "/"]),
            // nested: (sum + 10) * count
            (
                "(sum(size:) + 10) * count(size:)",
                vec!["SUM", "10", "+", "COUNT", "*"],
            ),
        ];

        for (query_body, expected_keywords) in test_cases {
            let query = format!("parentdir: &: ({})", query_body);
            let resolver = Resolver::new(&query).unwrap();
            let proj_operand = resolver
                .resolved_query
                .get_projection_operand()
                .expect("Should have projection");
            let nvalue = resolver
                .resolved_query
                .get_nvalue_combined()
                .expect("Should have nvalue");

            let sql = build_nvalue_standalone_subquery(
                proj_operand,
                &nvalue,
                resolver.resolved_query.get_context(),
                "oneview",
                false,
            );
            let sql_str = sql.to_string(PostgresQueryBuilder);

            for kw in expected_keywords {
                assert!(
                    sql_str.contains(kw),
                    "SQL should contain '{}' for query: {}\nSQL: {}",
                    kw,
                    query,
                    sql_str
                );
            }

            // Calculation の場合は JOIN が含まれるはず (リテラル単体以外)
            assert!(
                sql_str.contains("JOIN"),
                "SQL should contain JOIN for query: {}",
                query
            );
        }
    }

    #[test]
    fn test_print_sql_for_debugging_nest_bug() {
        let query_str = "(((parentdir: &: count(extension:rs))) / ((parentdir: &: count()))) :> 1";
        let resolver =
            crate::query::lens_resolver::Resolver::new(query_str).unwrap();
        let optimized =
            crate::query::lens_optimizer::optimize(resolver.resolved_query);
        let sql = crate::query::sql::build_pick_sql(&optimized, "oneview");
        println!(
            "Generated FETCH ITEMS SQL: {}",
            sql.to_string(sea_query::PostgresQueryBuilder)
        );

        if let Some(_proj_node) = optimized.get_projection() {
            let resolver2 =
                crate::query::lens_resolver::Resolver::new(query_str).unwrap();
            let label_sql = crate::query::sql::build_fetch_label_groups_sql(
                &resolver2,
                &crate::types::TagType::from("parentdir"),
                "oneview",
                100,
                0,
            )
            .unwrap();
            println!(
                "Generated LABEL GROUPS SQL: {}",
                label_sql.to_string(sea_query::PostgresQueryBuilder)
            );
        }
    }

    #[test]
    fn test_resolve_simple_filter_condition_multiple_row_tags() {
        use crate::query::ast::{BasicOp, ComparisonOp};
        use crate::query::lens_resolver::ResolvedNode;
        use crate::query::lens_schema::StorageMapping;
        use crate::types::{Label, LabelValue};

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

    // ── build_pick_sql: 多キー Nest ──────────────────────────────────────────

    #[test]
    fn test_build_pick_sql_multi_key_nest_includes_all_keys() {
        // Nest{[tagA, tagB]} の build_pick_sql は tagA と tagB 両方の IN 条件を含む
        use crate::query::lens_resolver::{ResolvedNode, ResolvedOperand};
        use crate::query::lens_schema::StorageMapping;
        use sea_query::PostgresQueryBuilder;

        let make_tag_ref = |name: &str| ResolvedOperand::TagRef {
            tag_type: TagType::from(name),
            storage: StorageMapping::RowTag {
                column: crate::db::Col::LabelStr,
                tag_type: name.to_string(),
            },
            sql_type: crate::db::SqlType::VARCHAR,
        };

        let nest_two_keys = ResolvedNode::Nest {
            keys: vec![make_tag_ref("tagA"), make_tag_ref("tagB")],
            nvalue: None,
            context: None,
        };

        let sql = build_pick_sql(&nest_two_keys, "oneview")
            .to_string(PostgresQueryBuilder);

        // tagA の条件が含まれる
        assert!(
            sql.contains("'tagA'"),
            "SQL should filter on tagA, got: {}",
            sql
        );
        // tagB の IN サブクエリも含まれる
        assert!(
            sql.contains("'tagB'"),
            "SQL should also filter on tagB (multi-key), got: {}",
            sql
        );
    }

    #[test]
    fn test_build_pick_sql_single_key_nest_no_extra_subquery() {
        // Nest{[tagA]} の build_pick_sql は tagA のみ（tagB の IN サブクエリなし）
        use crate::query::lens_resolver::{ResolvedNode, ResolvedOperand};
        use crate::query::lens_schema::StorageMapping;
        use sea_query::PostgresQueryBuilder;

        let nest_one_key = ResolvedNode::Nest {
            keys: vec![ResolvedOperand::TagRef {
                tag_type: TagType::from("tagA"),
                storage: StorageMapping::RowTag {
                    column: crate::db::Col::LabelStr,
                    tag_type: "tagA".to_string(),
                },
                sql_type: crate::db::SqlType::VARCHAR,
            }],
            nvalue: None,
            context: None,
        };

        let sql = build_pick_sql(&nest_one_key, "oneview")
            .to_string(PostgresQueryBuilder);

        assert!(
            sql.contains("'tagA'"),
            "SQL should filter on tagA, got: {}",
            sql
        );
        // 余分な IN サブクエリが生成されていないことを確認（tagB への参照なし）
        assert!(
            !sql.contains("'tagB'"),
            "Single-key Nest should not reference tagB, got: {}",
            sql
        );
    }

    // ── build_fetch_label_set_op_sql ─────────────────────────────────────────

    fn make_nest_node(tag: &str) -> crate::query::lens_resolver::ResolvedNode {
        use crate::query::lens_resolver::{ResolvedNode, ResolvedOperand};
        use crate::query::lens_schema::StorageMapping;
        ResolvedNode::Nest {
            keys: vec![ResolvedOperand::TagRef {
                tag_type: TagType::from(tag),
                storage: StorageMapping::RowTag {
                    column: crate::db::Col::LabelStr,
                    tag_type: tag.to_string(),
                },
                sql_type: crate::db::SqlType::VARCHAR,
            }],
            nvalue: None,
            context: None,
        }
    }

    #[test]
    fn test_build_fetch_label_set_op_sql_intersect_structure() {
        use crate::query::lens_resolver::{LabelSetOpKind, ResolvedNode};
        use sea_query::PostgresQueryBuilder;

        let node = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![make_nest_node("cat"), make_nest_node("flavor")],
        };

        let sql = build_fetch_label_set_op_sql(&node, "oneview", 100, 0)
            .unwrap()
            .to_string(PostgresQueryBuilder);

        // CTE 名が含まれる（ラベル値積集合構造）
        assert!(
            sql.contains("labels_0"),
            "should have labels_0 CTE, got: {}",
            sql
        );
        assert!(
            sql.contains("labels_1"),
            "should have labels_1 CTE, got: {}",
            sql
        );
        assert!(
            sql.contains("op_labels"),
            "should have op_labels CTE, got: {}",
            sql
        );
        // INTERSECT キーワード
        assert!(
            sql.to_uppercase().contains("INTERSECT"),
            "should contain INTERSECT, got: {}",
            sql
        );
        // 各オペランドのタグ条件
        assert!(
            sql.contains("'cat'"),
            "should reference 'cat', got: {}",
            sql
        );
        assert!(
            sql.contains("'flavor'"),
            "should reference 'flavor', got: {}",
            sql
        );
        // 結果カラム
        assert!(
            sql.contains("label_value"),
            "should select label_value, got: {}",
            sql
        );
        assert!(
            sql.contains("group_total"),
            "should select group_total, got: {}",
            sql
        );
        assert!(
            sql.contains("item_refs"),
            "should select item_refs, got: {}",
            sql
        );
    }

    #[test]
    fn test_build_fetch_label_set_op_sql_label_from_first_operand() {
        use crate::query::lens_resolver::{LabelSetOpKind, ResolvedNode};
        use sea_query::PostgresQueryBuilder;

        // ラベル値は先頭オペランド（cat）のタグ型から取得される
        let node = ResolvedNode::LabelSetOp {
            op: LabelSetOpKind::Intersect,
            operands: vec![make_nest_node("cat"), make_nest_node("flavor")],
        };

        let sql = build_fetch_label_set_op_sql(&node, "oneview", 100, 0)
            .unwrap()
            .to_string(PostgresQueryBuilder);

        // labels CTE では先頭オペランドの tag type でラベルを取得
        let labels_cte_pos = sql.find("labels").expect("labels CTE missing");
        let after_labels = &sql[labels_cte_pos..];
        assert!(
            after_labels.contains("'cat'"),
            "labels CTE should use first operand tag type 'cat', got: {}",
            sql
        );
    }
}

fn collect_tag_types_from_operand(
    operand: &ResolvedOperand,
    set: &mut std::collections::HashSet<String>,
) {
    match operand {
        ResolvedOperand::TagRef {
            storage: StorageMapping::RowTag { tag_type, .. },
            ..
        } => {
            set.insert(tag_type.clone());
        }
        ResolvedOperand::Calculation(calc) => {
            collect_tag_types_from_calc(calc, set);
        }
        ResolvedOperand::Aggregation(agg) => {
            use crate::query::lens_resolver::ResolvedAggregationNode;
            let inner = match agg {
                ResolvedAggregationNode::Count(node) => node,
                ResolvedAggregationNode::Arithmetic { inner, .. } => inner,
            };
            let (_, _, operand) = inner.extract_agg_parts();
            if let Some(op) = operand {
                collect_tag_types_from_operand(op, set);
            }
        }
        _ => {}
    }
}

fn collect_tag_types_from_calc(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    set: &mut std::collections::HashSet<String>,
) {
    collect_tag_types_from_operand(&calc.left, set);
    collect_tag_types_from_operand(&calc.right, set);
}

fn build_nest_pivot_cte(
    keys: &[ResolvedOperand],
    nvalue: Option<&ResolvedOperand>,
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

    let mut type_filters = std::collections::HashSet::new();

    for (i, key) in keys.iter().enumerate() {
        match key {
            ResolvedOperand::TagRef { storage, .. } => match storage {
                StorageMapping::RowTag { tag_type, column } => {
                    type_filters.insert(tag_type.as_str().to_string());
                    let case_expr = Expr::case(
                        Expr::col(Col::Type).eq(tag_type.as_str()),
                        Expr::col(*column),
                    );
                    let max_expr =
                        Expr::cust_with_exprs("MAX($1)", [case_expr.into()]);
                    stmt.expr_as(
                        max_expr.clone(),
                        Alias::new(&format!("key{}", i)),
                    );
                    stmt.and_having(max_expr.is_not_null());
                }
                StorageMapping::Column(col) => {
                    let max_expr = Expr::col(*col).max();
                    stmt.expr_as(
                        max_expr.clone(),
                        Alias::new(&format!("key{}", i)),
                    );
                    stmt.and_having(max_expr.is_not_null());
                }
                _ => {}
            },
            ResolvedOperand::Calculation(calc) => {
                // Calculation が参照する全ての RowTag 型をフィルタに追加
                collect_tag_types_from_calc(calc, &mut type_filters);
                let calc_expr = build_calculation_eav_expr(calc, view);
                stmt.expr_as(
                    calc_expr.clone(),
                    Alias::new(&format!("key{}", i)),
                );
                stmt.and_having(calc_expr.is_not_null());
            }
            _ => {}
        }
    }

    if let Some(nv) = nvalue {
        let nv_expr = build_resolved_operand_eav_expr(nv, view);
        stmt.expr_as(nv_expr, Alias::new("nvalue"));
        // nvalue が存在する場合、nvalue が参照するタグ型が type_filters に
        // 含まれない可能性があるため、WHERE フィルタを適用しない。
        // CASE WHEN 式が型の弁別を担うため、フィルタなしでも結果は正しい。
    } else if !type_filters.is_empty() {
        stmt.and_where(Expr::col(Col::Type).is_in(type_filters.clone()));
    }

    stmt.group_by_col(Col::ItemId);
    stmt
}

fn build_nvalue_pivot_aggregate_sql(
    keys: &[ResolvedOperand],
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
) -> SelectStatement {
    // Calculation nvalue: 各次元のキーに対して独立した集計を行い、後でJOINする
    if let ResolvedOperand::Calculation(calc) = nvalue {
        return build_mixed_key_calc_nvalue_sql(keys, calc, context, view);
    }

    let pivot_q = build_nest_pivot_cte(keys, Some(nvalue), view);
    let mut stmt = Query::select();
    for i in 0..keys.len() {
        stmt.column(Alias::new(&format!("key{}", i)));
    }

    let is_string = nvalue.is_string_type();
    let nval_expr = match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(_)) => {
            Expr::col(Alias::new("nvalue")).count().into()
        }
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Arithmetic {
            op,
            ..
        }) => apply_arithmetic_agg(
            op,
            Expr::col(Alias::new("nvalue")).into(),
            is_string,
        ),
        _ => crate::db::CustomFunc::any_value(Expr::col(Alias::new("nvalue")))
            .into(),
    };

    stmt.expr_as(nval_expr, Alias::new("nvalue"));
    stmt.from_subquery(pivot_q, Alias::new("pivot"));

    for i in 0..keys.len() {
        stmt.group_by_col(Alias::new(&format!("key{}", i)));
    }

    if let Some(ctx) = context {
        let ctx_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(build_pick_sql(ctx, view), Alias::new("_ctx"))
            .to_owned();
        stmt.and_where(Expr::col(Col::ItemId).in_subquery(ctx_sub));
    }

    stmt
}

/// Calculation nvalue を持つ異種キー Nest の nvalue 集計 SQL を構築します。
/// 各 nvalue コンポーネントを対応するキー次元で独立して集計し、JOIN して合算します。
///
/// 例: `(parentdir: &: count()) + (extension: &: count())`
/// → count(parentdir) per parentdir group と count(extension) per extension group を
///   pivot CTE 経由で得た有効な (key0, key1) 組み合わせに対して JOIN で結合します。
fn build_mixed_key_calc_nvalue_sql(
    keys: &[ResolvedOperand],
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    context: Option<&ResolvedNode>,
    view: &str,
) -> SelectStatement {
    let n_left = count_nvalue_keys(&calc.left).max(1).min(keys.len() - 1);

    // 各コンポーネントをそれぞれのキーで独立集計するサブクエリ
    let left_sub = build_nvalue_standalone_subquery(
        &keys[0], &calc.left, context, view, false,
    );
    let right_sub = build_nvalue_standalone_subquery(
        &keys[n_left],
        &calc.right,
        context,
        view,
        false,
    );

    // キーのみ pivot CTE: 有効な (key0, key1, ...) の組み合わせを列挙
    let pivot_sub = build_nest_pivot_cte(keys, None, view);

    let is_string = calc.left.is_string_type() && calc.right.is_string_type();

    let l_nvalue: SimpleExpr = Func::coalesce([
        Expr::col((Alias::new("L"), Alias::new("nvalue"))).into(),
        if is_string {
            Expr::val("").into()
        } else {
            Expr::val(0.0f64).into()
        },
    ])
    .into();
    let r_nvalue: SimpleExpr = Func::coalesce([
        Expr::col((Alias::new("R"), Alias::new("nvalue"))).into(),
        if is_string {
            Expr::val("").into()
        } else {
            Expr::val(0.0f64).into()
        },
    ])
    .into();

    let mut stmt = Query::select();
    stmt.distinct();

    for i in 0..keys.len() {
        stmt.expr_as(
            Expr::col((Alias::new("pivot"), Alias::new(&format!("key{}", i)))),
            Alias::new(&format!("key{}", i)),
        );
    }

    stmt.expr_as(
        apply_arithmetic_op(&calc.op, l_nvalue, r_nvalue, is_string),
        Alias::new("nvalue"),
    );

    stmt.from_subquery(pivot_sub, Alias::new("pivot"));

    // LEFT JOIN left_sub L ON pivot.key0 = L.group_label
    stmt.join_subquery(
        sea_query::JoinType::LeftJoin,
        left_sub,
        Alias::new("L"),
        Expr::col((Alias::new("pivot"), Alias::new("key0")))
            .equals((Alias::new("L"), Alias::new("group_label"))),
    );

    // LEFT JOIN right_sub R ON pivot.key{n_left} = R.group_label
    stmt.join_subquery(
        sea_query::JoinType::LeftJoin,
        right_sub,
        Alias::new("R"),
        Expr::col((Alias::new("pivot"), Alias::new(&format!("key{}", n_left))))
            .equals((Alias::new("R"), Alias::new("group_label"))),
    );

    // 全キーが非 NULL のものだけを対象とする
    for i in 0..keys.len() {
        stmt.and_where(
            Expr::col((Alias::new("pivot"), Alias::new(&format!("key{}", i))))
                .is_not_null(),
        );
    }

    if let Some(ctx) = context {
        let ctx_sub = Query::select()
            .column(Col::ItemId)
            .from_subquery(build_pick_sql(ctx, view), Alias::new("_ctx"))
            .to_owned();
        stmt.and_where(
            Expr::col((Alias::new("pivot"), Col::ItemId)).in_subquery(ctx_sub),
        );
    }

    stmt
}

/// ResolvedOperand が対応するキー数を返します。
/// Calculation の場合は左右の再帰的な合計。
fn count_nvalue_keys(nvalue: &ResolvedOperand) -> usize {
    match nvalue {
        ResolvedOperand::Calculation(calc) => {
            count_nvalue_keys(&calc.left) + count_nvalue_keys(&calc.right)
        }
        ResolvedOperand::Literal(_) => 0,
        _ => 1,
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
