use crate::db::{Col, Tbl};
use crate::query::ast::{
    ArithmeticAggOp, ArithmeticOp, ComparisonNode, ComparisonOp, Operand,
    QueryNode,
};
use crate::query::lens_resolver::{
    extract_nvalue_projection_parts, ProjectionOp, ResolvedAggregationNode,
    ResolvedNode, ResolvedOperand,
};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use crate::types::{Label, SType, TagType};
use sea_query::{
    Alias, BinOper, Condition, Expr, ExprTrait, Func, Query, SelectStatement,
    SimpleExpr,
};

/// クエリ構造を SQL (SelectStatement) へ変換します。
pub fn to_sql(node: &QueryNode, view_name: &str) -> SelectStatement {
    let stmt = match node {
        QueryNode::And(nodes) => build_and_sql(nodes, view_name),
        QueryNode::Or(nodes) => build_or_sql(nodes, view_name),
        QueryNode::Difference(l, r) => build_diff_sql(l, r, view_name),
        QueryNode::Complement(c) => build_comp_sql(c, view_name),
        QueryNode::Comparison(cmp) => build_comparison_sql(cmp, view_name),
        QueryNode::ColumnMatch { tag, label } => {
            build_column_match_sql(*tag, label, view_name)
        }
        QueryNode::TypedTag(tt) => {
            build_typed_tag_sql(&tt.label.tag_type(), &tt.label, view_name)
        }
        QueryNode::Projection(op) => build_projection_sql(op, view_name),
        QueryNode::Aggregation(agg) => build_aggregation_sql(agg, view_name),
        QueryNode::Nest(_) => {
            // Nest は logical_resolver/lens_resolver で解決済みのはず
            // ここには到達しない
            unreachable!("Nest node should be resolved before SQL generation")
        }
    };
    stmt
}

/// CalculationNodeに含まれるRowTagのtypeフィルタをWHERE句に追加します。
fn extract_and_add_row_tag_filters(
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
            extract_and_add_row_tag_filters(stmt, nested);
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
            extract_and_add_row_tag_filters(stmt, nested);
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
        ResolvedNode::Complement(c) => build_resolved_comp_sql(c, view),
        ResolvedNode::Projection { operand: op, .. } => {
            build_resolved_projection_sql(op, view)
        }
        ResolvedNode::MergedProjectionMatch {
            operand: op,
            matches,
            is_or,
        } => build_merged_projection_match_sql(op, matches, *is_or, view),
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
                let calc_expr = build_calculation_eav_expr(calc);
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
                    build_calculation_expr(calc)
                };

                let label_expr = label_to_unit_aware_expr(label);
                let bin_op = to_bin_op(*op);
                let cond = Expr::expr(calc_expr).binary(bin_op, label_expr);
                stmt.cond_where(cond);

                if !calc.contains_aggregation() {
                    extract_and_add_row_tag_filters(&mut stmt, calc);
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
                    build_calculation_expr(calc)
                };

                // 解決済みの演算子をそのまま使用する（Resolver側ですでに正規化済み）
                let bin_op = to_bin_op(*op);
                let cond = Expr::expr(tag_expr).binary(bin_op, calc_expr);

                stmt.cond_where(cond);

                // 集約関数が含まれていない場合のみ、
                // calcに含まれるRowTagのtypeフィルタを追加
                if !calc.contains_aggregation() {
                    extract_and_add_row_tag_filters(&mut stmt, calc);
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
                build_calculation_expr(calc)
            };

            let bin_op = to_bin_op(*op);
            let cond = Expr::expr(agg_expr).binary(bin_op, calc_expr);

            stmt.cond_where(cond);

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
        ResolvedNode::ProjectionMatch {
            operand: op,
            nvalue,
            op: comparison_op,
            label,
            context,
        } => {
            let mut stmt = build_resolved_projection_sql(op, view);
            let mut sub = build_nvalue_standalone_subquery(
                op,
                nvalue,
                context.as_deref(),
                view,
            );

            let bin_op = to_bin_op(*comparison_op);
            let label_expr = label_to_unit_aware_expr(label);
            let cond =
                Expr::col(Alias::new("nvalue")).binary(bin_op, label_expr);
            // Calculation の nvalue は GROUP BY なしのサブクエリ包装のため
            // HAVING ではなく WHERE を使用
            if matches!(nvalue, ResolvedOperand::Calculation(_)) {
                sub.and_where(cond);
            } else {
                sub.and_having(cond);
            }

            // プロジェクションカラム
            let proj_col = match op.get_storage() {
                Some(StorageMapping::RowTag { column, .. }) => *column,
                Some(StorageMapping::Column(col)) => *col,
                _ => return SelectStatement::default(),
            };

            stmt.and_where(
                Expr::col(proj_col).in_subquery(
                    Query::select()
                        .column(Alias::new("group_label"))
                        .from_subquery(sub, Alias::new("nfilter"))
                        .to_owned(),
                ),
            );
            stmt
        }
        ResolvedNode::ProjectionProjectionMatch {
            left_operand,
            left_nvalue,
            left_context,
            op,
            right_operand,
            right_nvalue,
            right_context,
        } => match op {
            ProjectionOp::Comparison(cmp_op) => {
                // プロジェクション同士の比較：左辺ベースの SQL を作成
                let mut stmt =
                    build_resolved_projection_sql(left_operand, view);

                // 左辺と右辺の nvalue サブクエリを生成（コンテキスト反映）
                let sub_l = build_nvalue_standalone_subquery(
                    left_operand,
                    left_nvalue,
                    left_context.as_deref(),
                    view,
                );
                let sub_r = build_nvalue_standalone_subquery(
                    right_operand,
                    right_nvalue,
                    right_context.as_deref(),
                    view,
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
                        Expr::col((Alias::new("L"), Alias::new("group_label")))
                            .eq(Expr::col((
                                Alias::new("R"),
                                Alias::new("group_label"),
                            ))),
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

                let proj_col = match left_operand.get_storage() {
                    Some(StorageMapping::RowTag { column, .. }) => *column,
                    Some(StorageMapping::Column(col)) => *col,
                    _ => return SelectStatement::default(),
                };

                stmt.and_where(Expr::col(proj_col).in_subquery(join_sql));
                stmt
            }
            ProjectionOp::Arithmetic(_) => {
                // 算術演算の ProjectionProjectionMatch は
                // Optimizer が単一 GROUP BY に最適化した形式で処理される予定。
                // 現在は左辺キーの Projection として扱う（nvalue は fetcher が計算）。
                build_resolved_projection_sql(left_operand, view)
            }
        },

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
    }
}

pub fn build_aggregation_sql(
    _agg: &crate::query::ast::AggregationNode,
    _view: &str,
) -> SelectStatement {
    // Phase 1 では未実装（resolve 経由を推奨）
    Query::select().to_owned()
}

/// nvalue 付き Projection に対する集約 SQL を生成する。
/// `sum(parentdir: &: count(ext:jpg))` のように、集約の inner が
/// nvalue 付き Projection の場合、nvalue を集約対象にした SQL を返す。
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

    // inner が nvalue 付き Projection または ProjectionMatch かチェック
    let (proj_operand, nvalue, merged_context) =
        match extract_nvalue_projection_parts(inner.clone()) {
            Ok(parts) => parts,
            Err(_) => return None,
        };

    // nvalue_condition は ProjectionMatch の場合に存在する
    let nvalue_condition = inner.get_nvalue_condition();
    let context = merged_context.as_deref();

    // nvalue サブクエリを生成（picked_ids を使わないスタンドアロン版）
    let mut nvalue_sub =
        build_nvalue_standalone_subquery(&proj_operand, &nvalue, context, view);

    // nvalue_condition がある場合、フィルタ条件を追加
    // Calculation の nvalue は GROUP BY なしのサブクエリ包装のため HAVING ではなく WHERE を使用
    if let Some((op, value)) = nvalue_condition {
        let bin_op = to_bin_op(*op);
        let val = label_to_simple_expr(value);
        let cond = Expr::col(Alias::new("nvalue")).binary(bin_op, val);
        if matches!(&nvalue, ResolvedOperand::Calculation(_)) {
            nvalue_sub.and_where(cond);
        } else {
            nvalue_sub.and_having(cond);
        }
    }

    let mut stmt = Query::select();
    if outer_is_count {
        // count(Projection_with_nvalue) → ラベルグループ数
        stmt.expr_as(Expr::cust("COUNT(*)"), Alias::new("scalar_value"));
    } else {
        // sum/avg/max/min(Projection_with_nvalue) → nvalue の集約
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
    stmt.from_subquery(nvalue_sub, Alias::new("nvalue_agg"));

    Some(stmt)
}

/// Count の引数ノードから、カウント対象のカラムと内部タグタイプを決定する。
///
/// count の基本セマンティクス:
/// - Projection (`extension:`) → 種類数: `COUNT(DISTINCT label_col)` + タグタイプ
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
fn build_count_nvalue_sql(
    proj_col: Col,
    proj_tag_type: Option<&str>,
    inner: &ResolvedNode,
    context: Option<&ResolvedNode>,
    item_scope: Option<SelectStatement>,
    view: &str,
) -> SelectStatement {
    let (count_col, inner_tag_type) = resolve_count_target(inner);

    let mut stmt = Query::select();

    if let Some(tag_type) = inner_tag_type {
        // ── Projection Count: 種類数を数える ──
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

    stmt
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
            let (inner_tag, inner_filter, operand) = inner.extract_agg_parts();

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
                let case_expr: SimpleExpr = if let Some(cond) =
                    resolve_simple_filter_condition(
                        &filter_node,
                        Alias::new(tbl_alias),
                    ) {
                    if cond.is_empty() {
                        val_expr.clone()
                    } else {
                        Expr::case(cond, val_expr.clone())
                            .finally(Expr::val(None::<f64>))
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
            )
        }
        ResolvedOperand::Aggregation(
            agg @ ResolvedAggregationNode::Arithmetic { op, inner },
        ) => {
            let is_string = agg.is_string_type();
            let (_, _, operand) = inner.extract_agg_parts();

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
            stmt
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

            stmt.group_by_col(proj_col);
            stmt
        }
        ResolvedOperand::Calculation(calc) => {
            let sub_l = build_nvalue_standalone_subquery(
                proj_operand,
                &calc.left,
                context,
                view,
            );
            let sub_r = build_nvalue_standalone_subquery(
                proj_operand,
                &calc.right,
                context,
                view,
            );

            let is_string =
                calc.left.is_string_type() && calc.right.is_string_type();

            // NULL 伝播防止のため RIGHT 側の nvalue を COALESCE で補完する。
            // count(extension:rs) など、マッチなしのグループは R に行が存在しないため
            // LEFT JOIN + COALESCE(R.nvalue, default) でそのグループを保持する。
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

            stmt.from_subquery(sub_l, Alias::new("L"));
            stmt.join_subquery(
                sea_query::JoinType::LeftJoin,
                sub_r,
                Alias::new("R"),
                Expr::col((Alias::new("L"), Alias::new("group_label"))).eq(
                    Expr::col((Alias::new("R"), Alias::new("group_label"))),
                ),
            );
            // 曖昧性排除: Calculation JOIN をサブクエリで包み、
            // 後続の HAVING 等で "nvalue" を一意に参照できるようにする。
            let sub = stmt.to_owned();
            Query::select()
                .column(Alias::new("group_label"))
                .column(Alias::new("nvalue"))
                .from_subquery(sub, Alias::new("calc_sub"))
                .to_owned()
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
                            Expr::col((
                                Alias::new("nv"),
                                Alias::new("nval"),
                            ))
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
            stmt
        }
    }
}

pub fn build_resolved_aggregation_sql(
    agg: &ResolvedAggregationNode,
    view: &str,
) -> SelectStatement {
    // nvalue 付き Projection に対する集約の場合、nvalue を集約対象にする
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
        build_resolved_operand_expr_for_arithmetic(op_node)
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
    let (agg_expr, cond, tag_type) = build_aggregation_parts(agg);
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));

    let op_bin = to_bin_op(op);
    let rhs = Expr::val(label.as_i64()); // TODO: 型に応じた変換

    let condition = Expr::expr(agg_expr.clone()).binary(op_bin, rhs);

    // 集計対象（Projection）がある場合、その型(type)自体で行を絞り込む必要がある
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
            CustomFunc::any_value_filter(
                cast_expr,
                Expr::col(Col::Type).eq(tag_type.as_str()),
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
    let calc_expr = build_calculation_eav_expr(calc);

    q.and_having(left_expr.binary(to_bin_op(op), calc_expr));

    q
}

/// EAV 構造用の計算式を集約式として構築します。
fn build_calculation_eav_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_eav_expr(&calc.left);
    let right_expr = build_resolved_operand_eav_expr(&calc.right);
    let is_string = calc.left.is_string_type() && calc.right.is_string_type();
    apply_arithmetic_op(&calc.op, left_expr, right_expr, is_string)
}

/// EAV 構造用のオペランドを集約式として構築します。
fn build_resolved_operand_eav_expr(
    operand: &crate::query::lens_resolver::ResolvedOperand,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedOperand;

    match operand {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => build_tag_value_agg_expr(storage, *sql_type),
        ResolvedOperand::Calculation(calc) => build_calculation_eav_expr(calc),
        ResolvedOperand::Aggregation(agg) => build_aggregation_expr(agg),
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
            let col = match storage {
                Some(StorageMapping::Column(c)) => {
                    tag_type = None;
                    *c
                }
                Some(StorageMapping::RowTag {
                    column,
                    tag_type: key,
                }) => {
                    tag_type = Some(key.clone());
                    *column
                }
                _ => {
                    tag_type = None;
                    Col::LabelInt
                } // Fallback
            };

            // TRY_CAST は build_storage_column_expr 内で適用されるため、
            // ここでは単純に式を構築するだけ
            let expr: SimpleExpr = if let Some(operand) = operand {
                // オペランド（算術式等）から直接 SQL 式を構築
                let is_string = operand.is_string_type();
                let inner_expr = build_resolved_operand_expr(operand);
                apply_arithmetic_agg(op, inner_expr, is_string)
            } else {
                // 直接のタグ参照の場合、その型を判定
                // RowTag の場合は数値演算が必要ならキャストされるが、
                // ここでは集約演算子適用前の生のカラム式
                let col_expr: SimpleExpr = Expr::col(col).into();
                // 投影対象の型が確定していれば String か判定
                let is_string = tag_type
                    .as_ref()
                    .map(|_t| {
                        // Lens から型情報を取得するのは build_aggregation_parts 内では
                        // Resolver/Lens が無いため困難。
                        // ただし Arithmetic 集約かつ TagRef ならば、通常 operand があるはず。
                        // operand が無い場合は直接の count 等だが、Arithmetic の場合は operand がある。
                        // 一旦 false とする（既存の数値集約の挙動を維持）。
                        false
                    })
                    .unwrap_or(false);
                apply_arithmetic_agg(op, col_expr, is_string)
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
            // Virtualタグは論理タグのため、直接的な物理カラムは存在しない
            // Phase 3で適切に実装予定
            Expr::col(Col::LabelStr).into()
        }
    }
}

/// 算術演算のオペランドをSQL式に変換します。
fn build_resolved_operand_expr(
    operand: &crate::query::lens_resolver::ResolvedOperand,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedOperand;

    match operand {
        ResolvedOperand::Literal(lab) => build_resolved_literal_expr(lab),
        ResolvedOperand::TagRef {
            storage, sql_type, ..
        } => build_storage_column_expr(storage, *sql_type),
        ResolvedOperand::Calculation(calc) => build_calculation_expr(calc),
        ResolvedOperand::Aggregation(agg) => build_aggregation_expr(agg),
    }
}

/// 集約関数をSQL式に変換します（算術演算内で使用）。
fn build_aggregation_expr(
    agg: &crate::query::lens_resolver::ResolvedAggregationNode,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedAggregationNode;

    match agg {
        ResolvedAggregationNode::Count(inner) => {
            let (storage, _cond, _) = inner.extract_agg_parts();
            if let Some(s) = storage {
                // count(projection:) -> COUNT(DISTINCT col)
                let col = match s {
                    &StorageMapping::Column(c) => c,
                    &StorageMapping::RowTag { column, .. } => column,
                    _ => Col::LabelInt, // Fallback
                };
                Expr::col(col).count_distinct().into()
            } else {
                // count(query) -> COUNT(DISTINCT item_id)
                Expr::col(Col::ItemId).count_distinct().into()
            }
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let is_string = agg.is_string_type();
            let (_, _, operand) = inner.extract_agg_parts();

            let expr = if let Some(operand) = operand {
                // 算術演算用のキャストロジックを共通利用
                build_resolved_operand_expr_for_arithmetic(operand)
            } else {
                // フォールバック
                build_resolved_operand_expr(&ResolvedOperand::Literal(
                    crate::types::Label::from(0),
                ))
            };

            apply_arithmetic_agg(op, expr, is_string)
        }
    }
}

/// 算術演算ノードをSQL式に変換します。
fn build_calculation_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_expr_for_arithmetic(&calc.left);
    let right_expr = build_resolved_operand_expr_for_arithmetic(&calc.right);
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
    use sea_query::SimpleExpr;
    let subquery = build_resolved_aggregation_sql(agg, view);
    SimpleExpr::SubQuery(None, Box::new(subquery.into_sub_query_statement()))
}

/// 算術演算用のオペランドをSQL式に変換します。
/// RowTag の LabelStr (VARCHAR) は TRY_CAST で DOUBLE に変換されます。
fn build_resolved_operand_expr_for_arithmetic(
    operand: &crate::query::lens_resolver::ResolvedOperand,
) -> SimpleExpr {
    use crate::query::lens_resolver::ResolvedOperand;

    // 文字列型の場合は数値キャストを行わず、通常の式構築を行う
    if operand.is_string_type() {
        return build_resolved_operand_expr(operand);
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
        ResolvedOperand::Calculation(calc) => build_calculation_expr(calc),
        ResolvedOperand::Aggregation(agg) => build_aggregation_expr(agg),
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
                build_calculation_expr(calc)
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
    // ProjectionMatch / ProjectionProjectionMatch は tags カラムが必要なため
    // ここでは早期リターンせず、通常のタグパッキング処理に委ねる。
    match node {
        ResolvedNode::Aggregation(_) | ResolvedNode::AggregationMatch { .. } => {
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
        .map(|c| format!("\"{}\" := \"{}\"", c.to_string(), c.to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let tags_expr = format!("LIST(struct_pack({}))", fields);

    let mut q = Query::select();
    q.column(Col::ItemId)
        .expr_as(Expr::col(Col::Rank).max(), Col::Rank)
        .expr_as(
            Expr::cust(format!("ANY_VALUE(\"{}\")", Col::ItemKind.to_string())),
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
    proj_operand: &ResolvedOperand,
    nvalue: &ResolvedOperand,
    context: Option<&ResolvedNode>,
    view: &str,
) -> SelectStatement {
    // 物理カラム情報の抽出
    let (proj_col, proj_storage) = match proj_operand {
        ResolvedOperand::TagRef { storage, .. } => match storage {
            StorageMapping::RowTag { column, .. } => (*column, storage),
            StorageMapping::Column(col) => (*col, storage),
            _ => return SelectStatement::default(),
        },
        _ => return SelectStatement::default(),
    };

    match nvalue {
        ResolvedOperand::Aggregation(ResolvedAggregationNode::Count(inner)) => {
            let proj_tag_type = match proj_storage {
                StorageMapping::RowTag { tag_type, .. } => {
                    Some(tag_type.as_str())
                }
                _ => None,
            };
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
            )
        }
        ResolvedOperand::Aggregation(
            agg @ ResolvedAggregationNode::Arithmetic { op, inner },
        ) => {
            let is_string = agg.is_string_type();
            let _ = inner.extract_agg_parts();

            // Dedup subquery for values (item_id, val)
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

            // FROM oneview AS proj
            stmt.from_as(Alias::new(view), Alias::new("proj"));

            // JOIN deduped ON proj.item_id = deduped.item_id
            stmt.join_subquery(
                sea_query::JoinType::InnerJoin,
                deduped,
                Alias::new("deduped"),
                Expr::col((Alias::new("proj"), Col::ItemId))
                    .equals((Alias::new("deduped"), Col::ItemId)),
            );

            // WHERE proj.type = proj_tag_type
            if let StorageMapping::RowTag { tag_type, .. } = proj_storage {
                stmt.and_where(
                    Expr::col((Alias::new("proj"), Col::Type))
                        .eq(tag_type.as_str()),
                );
            }

            // AND proj.item_id IN picked_ids
            stmt.and_where(
                Expr::col((Alias::new("proj"), Col::ItemId)).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from(Tbl::PickedIds)
                        .to_owned(),
                ),
            );

            stmt.group_by_col((Alias::new("proj"), proj_col));

            stmt
        }
        ResolvedOperand::Literal(label) => {
            // スカラー nvalue: 全ラベルに固定値を付与
            let val = label_to_simple_expr(label);
            let mut stmt = Query::select();
            stmt.expr_as(Expr::col(proj_col), Alias::new("group_label"));
            stmt.expr_as(val, Alias::new("nvalue"));
            stmt.from(Alias::new(view));

            if let StorageMapping::RowTag { tag_type, .. } = proj_storage {
                stmt.and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
            }

            stmt.and_where(
                Expr::col(Col::ItemId).in_subquery(
                    Query::select()
                        .column(Col::ItemId)
                        .from(Tbl::PickedIds)
                        .to_owned(),
                ),
            );

            stmt.group_by_col(proj_col);

            stmt
        }
        ResolvedOperand::Calculation(calc) => {
            let sub_l =
                build_nvalue_cte(proj_operand, &calc.left, context, view);
            let sub_r =
                build_nvalue_cte(proj_operand, &calc.right, context, view);

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

            stmt.from_subquery(sub_l, Alias::new("L"));
            stmt.join_subquery(
                sea_query::JoinType::LeftJoin,
                sub_r,
                Alias::new("R"),
                Expr::col((Alias::new("L"), Alias::new("group_label"))).eq(
                    Expr::col((Alias::new("R"), Alias::new("group_label"))),
                ),
            );
            // 曖昧性排除: Calculation JOIN をサブクエリで包み、
            // 後続の HAVING 等で "nvalue" を一意に参照できるようにする。
            let sub = stmt.to_owned();
            Query::select()
                .column(Alias::new("group_label"))
                .column(Alias::new("nvalue"))
                .from_subquery(sub, Alias::new("calc_sub"))
                .to_owned()
        }
        _ => {
            // TODO: Calculation nvalue etc.
            panic!("Unsupported nvalue type for CTE generation: {:?}", nvalue)
        }
    }
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

    // nvalue CTE: nvalue付きProjectionの場合、ラベルごとの集約値を計算
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
                resolver.resolved_query.get_projection_operand().unwrap(),
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

    // Calculation 投影の検出: Projection(Calculation(...)) の場合は
    // 算術式を事前計算する computed CTE を挿入する
    let proj_operand = resolver.resolved_query.get_projection_operand();
    let calc_node = match proj_operand {
        Some(crate::query::lens_resolver::ResolvedOperand::Calculation(c)) => {
            Some(c)
        }
        _ => None,
    };

    // label_col_name: CTE チェーン全体で使用するラベルカラム名
    // all_hits_source: all_hits CTE のデータソーステーブル
    // need_extra_filter: computed CTE で処理済みでなければ NULL/type フィルタが必要
    let (label_col_name, all_hits_source, need_extra_filter) =
        if let Some(calc) = calc_node {
            if calc.contains_row_tag() {
                // EAV 算術: 条件集計で item_id ごとに計算値を集約
                // (例: size: + mtime: → SUM(CASE WHEN type='size' ...) + SUM(CASE WHEN type='mtime' ...))
                let calc_expr = build_calculation_eav_expr(&calc);
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
                let calc_expr = build_calculation_expr(&calc);
                let mut computed_q = Query::select();
                computed_q
                    .column(Col::ItemId)
                    .expr_as(calc_expr, Alias::new("calc_value"))
                    .column(Col::Rank)
                    .distinct()
                    .from(Alias::new(view))
                    .and_where(
                        Expr::col(Col::ItemId).in_subquery(
                            Query::select()
                                .column(Col::ItemId)
                                .from(Tbl::PickedIds)
                                .to_owned(),
                        ),
                    )
                    .and_where(Expr::col(col_iden).is_not_null());
                if let StorageMapping::RowTag { tag_type, .. } = &desc.storage {
                    computed_q
                        .and_where(Expr::col(Col::Type).eq(tag_type.as_str()));
                }
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

    // CTE 2: all_hits (Window関数を含む全ヒットアイテム)
    let mut all_hits_q = Query::select();
    all_hits_q
        .column(Col::ItemId)
        .column(label_col.clone())
        .column(Col::Rank)
        .expr_as(
            CustomFunc::row_number_over(
                label_col.clone(),
                vec![
                    (Col::Rank, sea_query::Order::Desc),
                    (Col::ItemId, sea_query::Order::Desc),
                ],
            ),
            Tbl::Rn,
        )
        .expr_as(CustomFunc::count_over(label_col.clone()), Tbl::GroupTotal)
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
        all_hits_q.and_where(
            Expr::col(label_col.clone()).in_subquery(
                Query::select()
                    .column(Alias::new("group_label"))
                    .from(Alias::new("nvalue_agg"))
                    .to_owned(),
            ),
        );
    }

    let all_hits_cte = CommonTableExpression::new()
        .query(all_hits_q)
        .table_name(Tbl::AllHits)
        .to_owned();
    with_clause.cte(all_hits_cte);

    // CTE 3: top_items (表示対象の上位IDのみ、rankも含める)
    let mut top_items_q = Query::select();
    top_items_q
        .column(label_col.clone())
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

    q.with_cte(with_clause)
        .expr_as(
            Expr::col((Tbl::TopItems, label_col.clone())),
            Alias::new("label_value"),
        )
        .expr_as(
            Expr::col((Tbl::TopItems, Tbl::GroupTotal)),
            Alias::new("group_total"),
        )
        .expr_as(list_expr, Alias::new("item_refs"));

    // nvalue カラムの追加（スカラーサブクエリで nvalue_agg CTE を参照）
    if has_nvalue {
        let nvalue_lookup = Expr::cust(format!(
            "(SELECT \"nvalue\" FROM \"nvalue_agg\" WHERE \"group_label\" = {}.{})",
            Iden::to_string(&Tbl::TopItems),
            &label_col_name,
        ));
        q.expr_as(nvalue_lookup, Alias::new("nvalue"));
    }

    q.from(Tbl::TopItems)
        .group_by_col((Tbl::TopItems, label_col.clone()))
        .group_by_col((Tbl::TopItems, Tbl::GroupTotal))
        .order_by((Tbl::TopItems, label_col), sea_query::Order::Asc);

    if limit > 0 {
        q.limit((limit + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }

    Ok(q)
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

fn build_merged_projection_match_sql(
    operand: &crate::query::lens_resolver::ResolvedOperand,
    matches: &[crate::query::lens_resolver::ProjectionMatchCondition],
    is_or: bool,
    view: &str,
) -> SelectStatement {
    // 1. Projection operand base query setup
    let (proj_col, tag_type_opt) = match operand {
        crate::query::lens_resolver::ResolvedOperand::TagRef {
            storage,
            tag_type,
            ..
        } => {
            let col = match storage {
                StorageMapping::RowTag { column, .. } => *column,
                StorageMapping::Column(col) => *col,
                _ => return SelectStatement::default(),
            };
            (col, Some(tag_type.as_str()))
        }
        _ => return SelectStatement::default(),
    };

    let mut outer_stmt = build_resolved_projection_sql(operand, view);

    // 2. We use a unified Subquery that joins tags_view to itself ON item_id
    // to evaluate all matches in a single pass.
    let mut sub = Query::select();
    sub.expr_as(
        Expr::col((Alias::new("proj"), proj_col)),
        Alias::new("group_label"),
    );
    sub.from_as(Alias::new(view), Alias::new("proj"));
    sub.join_as(
        sea_query::JoinType::InnerJoin,
        Alias::new(view),
        Alias::new("c"),
        Condition::all().add(
            Expr::col((Alias::new("proj"), Col::ItemId))
                .equals((Alias::new("c"), Col::ItemId)),
        ),
    );

    if let Some(tag_type) = tag_type_opt {
        sub.and_where(Expr::col((Alias::new("proj"), Col::Type)).eq(tag_type));
    }

    sub.group_by_col((Alias::new("proj"), proj_col));

    // 3. Build HAVING condition for each match
    let mut having_cond = if is_or {
        Condition::any()
    } else {
        Condition::all()
    };

    for cond in matches {
        // HAVING 内の集約式は子テーブル c エイリアスを使用する
        let nvalue_expr = build_merged_nvalue_agg_expr(&cond.nvalue, "c", view);
        let right_expr = build_merged_nvalue_agg_expr(&cond.right, "c", view);

        let cmp_op = match cond.op {
            crate::query::lens_resolver::ProjectionOp::Comparison(op) => op,
            _ => panic!(
                "Expected ComparisonOp for MergedProjectionMatch condition"
            ),
        };
        let bin_op = to_bin_op(cmp_op);

        let term = Condition::all()
            .add(Expr::expr(nvalue_expr).binary(bin_op, right_expr));

        having_cond = having_cond.add(term);
    }

    sub.cond_having(having_cond);

    // 4. Wrap outer statement with subquery filter
    outer_stmt.and_where(
        Expr::col(proj_col).in_subquery(
            Query::select()
                .column(Alias::new("group_label"))
                .from_subquery(sub, Alias::new("nfilter"))
                .to_owned(),
        ),
    );

    outer_stmt
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

            // ResolvedNode の Projection 用条件生成を利用
            let cond = ResolvedNode::Projection {
                operand: op.clone(),
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

/// AND演算（積集合）のSQLを生成します。
///
/// 各ノードをサブクエリとして INTERSECT で結合します。
fn build_and_sql(nodes: &[QueryNode], view: &str) -> SelectStatement {
    let mut it = nodes.iter();
    let Some(first) = it.next() else {
        // Empty AND = everything
        let mut q = Query::select();
        q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .distinct()
            .from(Alias::new(view));
        return q;
    };

    // Precedence Safety: Wrap children in subqueries to enforce (A | B) & C logic
    let mut q = wrap_in_subquery(extract_sql(first, view));

    for next in it {
        q.union(
            sea_query::UnionType::Intersect,
            wrap_in_subquery(extract_sql(next, view)),
        );
    }
    q
}

fn extract_sql(node: &QueryNode, view: &str) -> SelectStatement {
    to_sql(node, view)
}

/// OR演算（和集合）のSQLを生成します。
///
/// 各ノードを UNION DISTINCT で結合します。
fn build_or_sql(nodes: &[QueryNode], view: &str) -> SelectStatement {
    let mut it = nodes.iter();
    let Some(first) = it.next() else {
        // Empty OR = nothing (1=0)
        let mut q = Query::select();
        q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
            .distinct()
            .from(Alias::new(view))
            .and_where(Expr::val(1).eq(0));
        return q;
    };

    let mut q = wrap_in_subquery(extract_sql(first, view));

    for next in it {
        q.union(
            sea_query::UnionType::Distinct,
            wrap_in_subquery(extract_sql(next, view)),
        );
    }
    q
}

/// 差集合演算のSQLを生成します。
///
/// 左のノードから右のノードを EXCEPT で除外します。
fn build_diff_sql(l: &QueryNode, r: &QueryNode, view: &str) -> SelectStatement {
    let mut q = wrap_in_subquery(extract_sql(l, view));
    q.union(
        sea_query::UnionType::Except,
        wrap_in_subquery(extract_sql(r, view)),
    );
    q
}

/// 補集合演算のSQLを生成します。
///
/// 指定されたタグタイプの全アイテムから、クエリ結果を除外します。
fn build_comp_sql(c: &QueryNode, view: &str) -> SelectStatement {
    let types = c.get_all_types();
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));
    if !types.is_empty() {
        q.and_where(Expr::col(Col::Type).is_in(types));
    }
    let mut eq = Query::select();
    eq.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from_subquery(extract_sql(c, view), Tbl::NotSide);
    q.union(sea_query::UnionType::Except, eq);
    q
}

fn build_comparison_sql(node: &ComparisonNode, view: &str) -> SelectStatement {
    let mut operands = vec![&node.first];
    for (_, opd) in &node.rest {
        operands.push(opd);
    }

    let mut subqueries = Vec::new();
    for (i, (op, _)) in node.rest.iter().enumerate() {
        let left = operands[i];
        let right = operands[i + 1];
        subqueries.push(build_binary_comparison_sql(left, *op, right, view));
    }

    if subqueries.len() == 1 {
        subqueries.pop().unwrap()
    } else {
        let mut first = subqueries.remove(0);
        for next in subqueries {
            first.union(sea_query::UnionType::Intersect, next);
        }
        first
    }
}

fn build_binary_comparison_sql(
    left: &Operand,
    op: ComparisonOp,
    right: &Operand,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));

    let bin_op = to_bin_op(op);

    let (tt, lab, effective_op) =
        match normalize_comparison(left, bin_op, right) {
            Some(res) => res,
            None => {
                q.and_where(Expr::val(1).eq(0));
                return q;
            }
        };

    apply_generic_comparison(q, tt, effective_op, lab)
}

/// 比較演算子を反転します（オペランドの順序が逆転した時に使用）。
/// 例: `a < b` を `b > a` に変換する際、`<` を `>` に反転
fn flip_bin_op(op: BinOper) -> BinOper {
    match op {
        BinOper::GreaterThan => BinOper::SmallerThan,
        BinOper::GreaterThanOrEqual => BinOper::SmallerThanOrEqual,
        BinOper::SmallerThan => BinOper::GreaterThan,
        BinOper::SmallerThanOrEqual => BinOper::GreaterThanOrEqual,
        other => other,
    }
}

fn normalize_comparison(
    left: &Operand,
    op: BinOper,
    right: &Operand,
) -> Option<(TagType, Label, BinOper)> {
    match (left, right) {
        (Operand::TypeRef(tt), Operand::Literal(lab)) => {
            Some((tt.clone(), lab.clone(), op))
        }
        (Operand::Literal(lab), Operand::TypeRef(tt)) => {
            Some((tt.clone(), lab.clone(), flip_bin_op(op)))
        }
        _ => None,
    }
}

fn apply_generic_comparison(
    mut q: SelectStatement,
    tagtype: TagType,
    op: BinOper,
    label: Label,
) -> SelectStatement {
    let mut condition = Condition::any();
    let s_val = label.as_str();

    // 物理カラム（LabelInt / LabelStr 等）のどれに合致し得るかを判定
    // FIXME: Label 自身に物理型情報（LabelValue）を問い合わせるのが美しい
    match label {
        Label::Rank(_)
        | Label::Size(_)
        | Label::Mtime(_)
        | Label::ItemId(_) => {
            let i = label.as_i64();
            condition = condition
                .add(Expr::col(Col::LabelInt).binary(op, Expr::val(i)))
                .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(i)));
        }
        _ => {
            // 文字列ベースのマッチング
            condition = condition.add(
                Expr::col(Col::LabelStr).binary(op, Expr::val(s_val.clone())),
            );

            // 数値や真偽値として解釈可能な場合はそれらとも比較
            if let Ok(i) = s_val.parse::<i64>() {
                condition = condition
                    .add(Expr::col(Col::LabelInt).binary(op, Expr::val(i)))
                    .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(i)));
            } else if let Ok(f) = s_val.parse::<f64>() {
                condition = condition
                    .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(f)));
            } else if s_val == "true" || s_val == "false" {
                let b = s_val == "true";
                condition = condition
                    .add(Expr::col(Col::LabelBool).binary(op, Expr::val(b)));
            }
        }
    }

    q.and_where(Expr::col(Col::Type).eq(tagtype.as_str()))
        .and_where(condition.into());
    q
}

fn build_single_type_projection_sql(
    tagtype: &TagType,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));

    if let TagType::Base(SType::TypedTag) = tagtype {
        q.and_where(Expr::col(Col::TypedTag).is_not_null());
    } else if let TagType::Base(SType::Origin) = tagtype {
        q.and_where(Expr::col(Col::Origin).is_not_null());
    } else if let TagType::Base(SType::Rank) = tagtype {
        // Rankは全アイテムが持っているので条件追加不要（NULLチェックのみ）
        q.and_where(Expr::col(Col::Rank).is_not_null());
    } else if let TagType::Base(SType::Type) = tagtype {
        q.and_where(Expr::col(Col::Type).is_not_null());
    } else if let TagType::Base(SType::Label) = tagtype {
        // Label (仮想タグ) はすべてのタグの値を集約するもの。
        // 全てのアイテムは少なくとも1つのタグを持つため、実質的に全アイテムが対象。
        // label_str IS NOT NULL 等のチェックは DuckDB 上で不安定な挙動を示す場合があるため、
        // 条件なし（全件）とする。
    } else {
        let tag_name = tagtype.as_str();
        if tag_name != "*"
            && tag_name != "tag"
            && tag_name != "type"
            && tag_name != "origin"
        {
            q.and_where(Expr::col(Col::Type).eq(tag_name));
        }

        let mut cond = Condition::any();
        cond = cond.add(Expr::col(Col::LabelStr).is_not_null());
        cond = cond.add(Expr::col(Col::LabelInt).is_not_null());
        cond = cond.add(Expr::col(Col::LabelDouble).is_not_null());
        cond = cond.add(Expr::col(Col::LabelBool).is_not_null());

        q.and_where(cond.into());
    }
    q
}

/// プロジェクションクエリのSQLを生成します。
fn build_projection_sql(op: &Operand, view: &str) -> SelectStatement {
    match op {
        Operand::TypeRef(tt) => build_single_type_projection_sql(tt, view),
        Operand::Calculation(calc) => {
            let mut l = build_projection_sql(&calc.left, view);
            let r = build_projection_sql(&calc.right, view);
            l.union(sea_query::UnionType::Intersect, r);
            l
        }
        _ => {
            // Fallback for literals etc.
            let mut q = Query::select();
            q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
                .distinct()
                .from(Alias::new(view));
            q
        }
    }
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

/// TypedTagクエリのSQLを生成します。
///
/// タグタイプとラベルの両方を指定した検索（例: `name:test.txt`）。
fn build_typed_tag_sql(
    tagtype: &TagType,
    label: &Label,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));
    let glob = BinOper::Custom("GLOB");

    match tagtype {
        TagType::LiteralCustom(s) => {
            q.and_where(Expr::col(Col::Type).eq(s.as_str()));
        }
        _ => {
            q.and_where(
                Expr::col(Col::Type)
                    .binary(glob.clone(), Expr::val(tagtype.as_str())),
            );
        }
    }

    let mut cond = Condition::any();
    match label.value() {
        crate::types::LabelValue::Integer(i) => {
            cond = cond
                .add(Expr::col(Col::LabelInt).eq(i))
                .add(Expr::col(Col::LabelDouble).eq(i as f64));
        }
        crate::types::LabelValue::String(s) => {
            let val_str = if s.starts_with('^') {
                format!("{}*", &s[1..])
            } else {
                s.clone()
            };

            cond = cond.add(
                Expr::col(Col::LabelStr)
                    .binary(BinOper::Custom("GLOB"), Expr::val(val_str)),
            );
        }
        crate::types::LabelValue::Literal(s) => {
            cond = cond.add(Expr::col(Col::LabelStr).eq(s.as_str()));
            if let Ok(i) = s.parse::<i64>() {
                cond = cond.add(Expr::col(Col::LabelInt).eq(i));
            }
            if s == "true" || s == "false" {
                cond = cond.add(Expr::col(Col::LabelBool).eq(s == "true"));
            }
        }
        crate::types::LabelValue::Boolean(b) => {
            cond = cond.add(Expr::col(Col::LabelBool).eq(b));
        }
        crate::types::LabelValue::Double(bits) => {
            cond =
                cond.add(Expr::col(Col::LabelDouble).eq(f64::from_bits(bits)));
        }
        crate::types::LabelValue::Null => {
            cond = cond.add(Expr::col(Col::LabelStr).is_null());
        }
    }
    q.and_where(cond.into());
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
    fn test_flip_bin_op() {
        assert_eq!(flip_bin_op(BinOper::GreaterThan), BinOper::SmallerThan);
        assert_eq!(
            flip_bin_op(BinOper::SmallerThanOrEqual),
            BinOper::GreaterThanOrEqual
        );
        assert_eq!(flip_bin_op(BinOper::Equal), BinOper::Equal);
    }

    #[test]
    fn test_normalize_comparison_order() {
        let left = Operand::TypeRef(TagType::from("size"));
        let right = Operand::Literal(Label::from(100));
        let (tt, lab, op) =
            normalize_comparison(&left, BinOper::Equal, &right).unwrap();
        assert_eq!(tt.as_str(), "size");
        assert_eq!(lab.value(), crate::types::LabelValue::Integer(100));
        assert_eq!(op, BinOper::Equal);

        let left_lit = Operand::Literal(Label::from(100));
        let right_tag = Operand::TypeRef(TagType::from("size"));
        let (tt2, lab2, op2) =
            normalize_comparison(&left_lit, BinOper::GreaterThan, &right_tag)
                .unwrap();
        assert_eq!(tt2.as_str(), "size");
        assert_eq!(lab2.value(), crate::types::LabelValue::Integer(100));
        assert_eq!(op2, BinOper::SmallerThan);
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
    fn test_build_typed_tag_sql_gen() {
        let tt = TypedTag::new("name", Label::from("foo.txt"));
        let sql =
            build_typed_tag_sql(&tt.label.tag_type(), &tt.label, "oneview");
        let result = sql.to_string(SqliteQueryBuilder);
        // Expect exact logic: "label_str" = 'foo.txt' AND "type" = 'name'
        // Quotes might vary slightly by builder, but Sqlite default uses double quotes for identifiers and single for strings.
        assert!(result.contains("'foo.txt'"));
        assert!(result.contains("'name'"));
    }

    #[test]
    fn test_build_comparison_sql_int() {
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from("size")),
            rest: vec![(
                ComparisonOp::Scalar(BasicOp::Gt),
                Operand::Literal(Label::from(100)),
            )],
        };
        let sql = build_comparison_sql(&node, "oneview");
        let result = sql.to_string(SqliteQueryBuilder);
        assert!(result.contains("> 100"));
        assert!(result.contains("'size'"));
    }

    #[test]
    fn test_build_and_sql_structure() {
        let node1 = QueryNode::TypedTag(TypedTag::new("name", "foo"));
        let node2 = QueryNode::TypedTag(TypedTag::new("extension", "rs"));
        let nodes = vec![node1, node2];

        let sql = build_and_sql(&nodes, "oneview");
        let result = sql.to_string(SqliteQueryBuilder);
        assert!(result.contains("INTERSECT"));
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
            inner: Box::new(ResolvedNode::Projection {
                operand: ResolvedOperand::TagRef {
                    tag_type: TagType::Base(SType::Size),
                    storage: StorageMapping::RowTag {
                        column: Col::LabelInt,
                        tag_type: "size".to_string(),
                    },
                    sql_type: crate::db::SqlType::BIGINT,
                },
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
        // AggregationNode::Arithmetic { op: Sum, inner: And(Projection, Filter) }
        let agg = ResolvedAggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(ResolvedNode::And(vec![
                ResolvedNode::Projection {
                    operand: ResolvedOperand::TagRef {
                        tag_type: TagType::Base(SType::Size),
                        storage: StorageMapping::Column(Col::Size),
                        sql_type: crate::db::SqlType::BIGINT,
                    },
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

        let expr = build_calculation_expr(&calc);
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
        let expr = build_resolved_operand_expr(&operand);
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
        let expr_lit = build_resolved_operand_expr_for_arithmetic(&lit_bool);
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
        let expr_tag = build_resolved_operand_expr_for_arithmetic(&tag_bool);
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

        // parentdir: &: count(extension:jpg) → nvalue付きProjection
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

        // parentdir: &: sum(size:) → nvalue付きProjection (Arithmetic)
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
            "SQL should contain nvalue_agg CTE: {}",
            sql_str
        );
        assert!(
            sql_str.contains("nvalue"),
            "SQL should contain nvalue column: {}",
            sql_str
        );
        assert!(
            sql_str.contains("SUM"),
            "SQL should contain SUM for arithmetic nvalue: {}",
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
        use crate::query::lens_resolver::{ResolvedNode, ResolvedOperand};
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
}
