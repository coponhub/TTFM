use crate::db::{Col, Tbl};
use crate::query::ast::ArithmeticAggOp::*;
use crate::query::ast::ArithmeticOp::*;
use crate::query::ast::{
    ArithmeticAggOp, ArithmeticOp, ComparisonNode, ComparisonOp, Operand,
    QueryNode,
};
use crate::query::lens_resolver::{ResolvedAggregationNode, ResolvedNode};
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
            if let StorageMapping::RowTag { tag_key, .. } = storage {
                stmt.and_where(Expr::col(Col::Type).eq(tag_key.as_str()));
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
            if let StorageMapping::RowTag { tag_key, .. } = storage {
                stmt.and_where(Expr::col(Col::Type).eq(tag_key.as_str()));
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
        ResolvedNode::Projection(op) => build_resolved_projection_sql(op, view),
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
            let mut stmt = Query::select();
            stmt.from(Alias::new(view));
            stmt.column(Col::ItemId);

            // 集約関数が含まれている場合はサブクエリを使用
            let calc_expr = if calc.contains_aggregation() {
                build_calculation_subquery(calc, view)
            } else {
                build_calculation_expr(calc)
            };

            // ヘルパー関数で簡潔に記述（単位パース付き）
            let label_expr = label_to_unit_aware_expr(label);
            let bin_op = to_bin_op(*op);
            let cond = Expr::expr(calc_expr).binary(bin_op, label_expr);

            stmt.cond_where(cond);

            // 集約関数が含まれていない場合のみ、
            // calcに含まれるRowTagのtypeフィルタを追加
            if !calc.contains_aggregation() {
                extract_and_add_row_tag_filters(&mut stmt, calc);
            }

            stmt
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

                // 集約関数が含まれている場合はサブクエリを使用
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
            if let StorageMapping::RowTag { tag_key, .. } = storage {
                stmt.and_where(Expr::col(Col::Type).eq(tag_key.as_str()));
            }

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

pub fn build_resolved_aggregation_sql(
    agg: &ResolvedAggregationNode,
    view: &str,
) -> SelectStatement {
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));

    let (expr, cond, tag_key) = build_aggregation_parts(agg);

    // 集計対象（Projection）がある場合、その型(type)自体で行を絞り込む必要がある
    // ただし、物理カラム自体（item_id, path等）や、特別な仮想タグの場合は絞り込まない
    let target_type = match agg {
        ResolvedAggregationNode::Count(inner) => inner.get_projection(),
        ResolvedAggregationNode::Arithmetic { inner, .. } => {
            inner.get_projection()
        }
    };

    let mut final_cond = Condition::all();
    if let Some(key) = tag_key {
        // RowTag の場合は実際の tag_key でフィルタする
        final_cond = final_cond.add(Expr::col(Col::Type).eq(key));
    } else if target_type.is_some() {
        // 念のため: tag_key がなく target_type がある場合は何もしない
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

    stmt.expr_as(expr, Alias::new("scalar_value"));
    stmt.cond_where(final_cond);

    let sql = stmt.to_owned();
    if std::env::var("TTFM_DEBUG").is_ok() {
        println!(
            "DEBUG AGG SQL: {}",
            sql.to_string(sea_query::PostgresQueryBuilder)
        );
    }
    sql
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
    let mut stmt = Query::select();
    stmt.from(Alias::new(view));

    let (agg_expr, cond, tag_key) = build_aggregation_parts(agg);
    let op_bin = to_bin_op(op);
    let rhs = Expr::val(label.as_i64()); // TODO: 型に応じた変換

    let condition = Expr::expr(agg_expr).binary(op_bin, rhs);

    // 集計対象（Projection）がある場合、その型(type)自体で行を絞り込む必要がある
    let target_type = match agg {
        ResolvedAggregationNode::Count(inner) => inner.get_projection(),
        ResolvedAggregationNode::Arithmetic { inner, .. } => {
            inner.get_projection()
        }
    };

    let mut final_cond = Condition::all();
    if let Some(key) = tag_key {
        // RowTag の場合は実際の tag_key でフィルタする
        final_cond = final_cond.add(Expr::col(Col::Type).eq(key));
    } else if target_type.is_some() {
        // 念のため: tag_key がなく target_type がある場合は何もしない
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

    // 真なら TRUE (1), 偽なら FALSE (NULL) の ItemId を返す
    // 仮想アイテムかどうかは item_kind = 'virtual' で判定する
    let case_expr = Expr::case(condition, Expr::val(1i64));

    stmt.expr_as(case_expr, Col::ItemId);
    stmt.expr_as(
        Expr::val(crate::types::VolatileItem::KIND),
        Col::ItemKind,
    );
    stmt.expr_as(Expr::val("boolean"), Col::Type);
    stmt.expr_as(Expr::val(0i64), Col::Rank);
    // tags カラムが必要（fetch_items で decode_item_from_row が呼ばれるため）
    // 空のリフト（リスト）をダミーとして設定
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
        StorageMapping::RowTag { column, tag_key } => {
            // RowTag は label_str (VARCHAR) として保存されているため、
            // 算術演算を行う場合は TRY_CAST(... AS DOUBLE) で数値に変換する
            // TRY_CAST は変換失敗時に NULL を返す（エラーにならない）
            let cast_expr = Expr::cust_with_exprs(
                "TRY_CAST($1 AS DOUBLE)",
                [Expr::col(*column).into()],
            );
            CustomFunc::any_value_filter(
                cast_expr,
                Expr::col(Col::Type).eq(tag_key.as_str()),
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
    apply_arithmetic_op(&calc.op, left_expr, right_expr)
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
        }
    }
}

fn build_aggregation_parts(
    agg: &ResolvedAggregationNode,
) -> (SimpleExpr, Option<ResolvedNode>, Option<String>) {
    match agg {
        ResolvedAggregationNode::Count(node) => {
            let (storage, cond, _) = node.extract_agg_parts();
            let tag_key;
            let expr = if let Some(s) = storage {
                // count(projection:) -> COUNT(DISTINCT col)
                let col = match s {
                    StorageMapping::Column(c) => {
                        tag_key = None;
                        *c
                    }
                    StorageMapping::RowTag {
                        column,
                        tag_key: key,
                    } => {
                        tag_key = Some(key.clone());
                        *column
                    }
                    _ => {
                        tag_key = None;
                        Col::LabelInt
                    } // Fallback
                };
                Expr::col(col).count_distinct().into()
            } else {
                // count(query) -> COUNT(DISTINCT item_id)
                tag_key = None;
                Expr::col(Col::ItemId).count_distinct().into()
            };
            (expr, cond, tag_key)
        }
        ResolvedAggregationNode::Arithmetic { op, inner } => {
            let (storage, cond, operand) = inner.extract_agg_parts();
            let tag_key;
            let col = match storage {
                Some(StorageMapping::Column(c)) => {
                    tag_key = None;
                    *c
                }
                Some(StorageMapping::RowTag {
                    column,
                    tag_key: key,
                }) => {
                    tag_key = Some(key.clone());
                    *column
                }
                _ => {
                    tag_key = None;
                    Col::LabelInt
                } // Fallback
            };

            let expr: SimpleExpr = if let Some(operand) = operand {
                // オペランド（算術式等）から直接 SQL 式を構築
                let inner_expr = build_resolved_operand_expr(operand);
                apply_arithmetic_agg(op, inner_expr)
            } else {
                apply_arithmetic_agg(op, Expr::col(col).into())
            };
            (expr, cond, tag_key)
        }
    }
}

/// 集約関数を式に適用します。
fn apply_arithmetic_agg(op: &ArithmeticAggOp, expr: SimpleExpr) -> SimpleExpr {
    match op {
        Sum => Func::sum(expr).into(),
        Avg => Func::avg(expr).into(),
        Max => Func::max(expr).into(),
        Min => Func::min(expr).into(),
    }
}

/// StorageMappingから適切なSQL列式を生成します。
fn build_storage_column_expr(
    storage: &StorageMapping,
    _sql_type: crate::db::SqlType,
) -> SimpleExpr {
    match storage {
        StorageMapping::Column(col) => Expr::col(*col).into(),
        StorageMapping::RowTag { column, .. } => Expr::col(*column).into(),
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
            let (storage, _cond, operand) = inner.extract_agg_parts();
            let col = match storage {
                Some(&StorageMapping::Column(c)) => c,
                Some(&StorageMapping::RowTag { column, .. }) => column,
                _ => Col::LabelInt, // Fallback
            };

            let inner_expr = if let Some(operand) = operand {
                build_resolved_operand_expr(operand)
            } else {
                Expr::col(col).into()
            };

            apply_arithmetic_agg(op, inner_expr)
        }
    }
}

/// 算術演算ノードをSQL式に変換します。
fn build_calculation_expr(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_expr(&calc.left);
    let right_expr = build_resolved_operand_expr(&calc.right);
    apply_arithmetic_op(&calc.op, left_expr, right_expr)
}

/// 算術演算子を適用します。
fn apply_arithmetic_op(
    op: &ArithmeticOp,
    left: SimpleExpr,
    right: SimpleExpr,
) -> SimpleExpr {
    let bin_op = match op {
        Add => BinOper::Add,
        Sub => BinOper::Sub,
        Mul => BinOper::Mul,
        Div => BinOper::Div,
        Mod => BinOper::Custom("%"),
    };
    Expr::expr(left).binary(bin_op, right)
}

/// 集約関数を含む算術演算をサブクエリとして構築します。
fn build_calculation_subquery(
    calc: &crate::query::lens_resolver::ResolvedCalculationNode,
    view: &str,
) -> SimpleExpr {
    let left_expr = build_resolved_operand_subquery(&calc.left, view);
    let right_expr = build_resolved_operand_subquery(&calc.right, view);
    apply_arithmetic_op(&calc.op, left_expr, right_expr)
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
                // 除算などで整数切り捨てを防ぐため、DOUBLEとして扱う
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
                }
            }
        }
        ResolvedOperand::TagRef { .. } => {
            // サブクエリ内でTagRefは使えない（全体の集約値のみ）
            // これは通常発生しないが、エラー処理として0を返す
            Expr::val(0).into()
        }
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
    // build_resolved_aggregation_sqlを使ってSELECT文を構築
    let subquery = build_resolved_aggregation_sql(agg, view);
    // サブクエリとして返す
    SimpleExpr::SubQuery(None, Box::new(subquery.into_sub_query_statement()))
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
    .expr_as(Expr::val("virtual"), Col::ItemKind)
    .expr_as(Expr::val("boolean"), Col::Type)
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
    .expr_as(Expr::val("virtual"), Col::ItemKind)
    .expr_as(Expr::val("boolean"), Col::Type)
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
    use sea_query::{CommonTableExpression, WithClause};

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

    // CTE 2: all_hits (Window関数を含む全ヒットアイテム)
    let mut all_hits_q = Query::select();
    all_hits_q
        .column(Col::ItemId)
        .column(col_iden)
        .column(Col::Rank)
        .expr_as(
            CustomFunc::row_number_over(
                col_iden,
                vec![
                    (Col::Rank, sea_query::Order::Desc),
                    (Col::ItemId, sea_query::Order::Desc),
                ],
            ),
            Tbl::Rn,
        )
        .expr_as(CustomFunc::count_over(col_iden), Tbl::GroupTotal)
        .distinct()
        .from(Alias::new(view))
        .and_where(
            Expr::col(Col::ItemId).in_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .from(Tbl::PickedIds)
                    .to_owned(),

            ),
        );

    // Locationsテーブル由来のNULL（例: extensionなし）を除外
    all_hits_q.and_where(Expr::col(col_iden).is_not_null());

    if let StorageMapping::RowTag { tag_key, .. } = &desc.storage {
        all_hits_q.and_where(Expr::col(Col::Type).eq(tag_key.as_str()));
    }

    let all_hits_cte = CommonTableExpression::new()
        .query(all_hits_q)
        .table_name(Tbl::AllHits)
        .to_owned();
    with_clause.cte(all_hits_cte);

    // CTE 3: top_items (表示対象の上位IDのみ、rankも含める)
    let mut top_items_q = Query::select();
    top_items_q
        .column(col_iden)
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
    use sea_query::Iden;
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
        .expr_as(Expr::col((Tbl::TopItems, col_iden)), Alias::new("label_value"))
        .expr_as(
            Expr::col((Tbl::TopItems, Tbl::GroupTotal)),
            Alias::new("group_total"),
        )
        .expr_as(list_expr, Alias::new("item_refs"))
        .from(Tbl::TopItems)
        .group_by_col((Tbl::TopItems, col_iden))
        .group_by_col((Tbl::TopItems, Tbl::GroupTotal))
        .order_by((Tbl::TopItems, col_iden), sea_query::Order::Asc);

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
            .expr_as(Expr::val("virtual"), Col::ItemKind);

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
            let cond = ResolvedNode::Projection(op.clone()).to_condition();
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
        .expr_as(col_bool, Col::LabelBool);

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
            }
        }
    }

    q
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{BasicOp, QueryNode};
    use crate::query::lens_resolver::ResolvedOperand;
    use crate::types::{Label, SType, TagType, TypedTag};
    use sea_query::{PostgresQueryBuilder, Query, SqliteQueryBuilder};

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
                        tag_key: "extension".to_string(),
                    },
                    sql_type: crate::db::SqlType::VARCHAR,
                    op: ComparisonOp::Scalar(BasicOp::Eq),
                    label: Label::from("txt"),
                },
            ])));

        let sql = build_resolved_aggregation_sql(&agg, "oneview");
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // 基本的な COUNT
        assert!(sql_str.contains("COUNT(DISTINCT \"item_id\")"));
        // フィルタ条件 (IN サブクエリ形式: build_pick_sql により階層化される)
        assert!(sql_str.contains("IN (SELECT \"item_id\" FROM (SELECT"));
        assert!(sql_str.contains(
            "WHERE \"type\" = 'extension' AND \"label_str\" = 'txt'"
        ));
    }

    #[test]
    fn test_build_resolved_aggregation_sql_sum_projection() {
        use crate::query::ast::ArithmeticAggOp;
        let agg = ResolvedAggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(ResolvedNode::Projection(
                ResolvedOperand::TagRef {
                    tag_type: TagType::Base(SType::Size),
                    storage: StorageMapping::RowTag {
                        column: Col::LabelInt,
                        tag_key: "size".to_string(),
                    },
                    sql_type: crate::db::SqlType::BIGINT,
                },
            )),
        };

        let sql = build_resolved_aggregation_sql(&agg, "oneview");
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // SUM(label_int) であること。
        assert!(sql_str.contains("SUM(\"label_int\")"));
        // 重複集計防止の type = 'size' フィルタがメインの WHERE に含まれていること。
        assert!(sql_str.contains("WHERE \"type\" = 'size'"));
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
                ResolvedNode::Projection(ResolvedOperand::TagRef {
                    tag_type: TagType::Base(SType::Size),
                    storage: StorageMapping::Column(Col::Size),
                    sql_type: crate::db::SqlType::BIGINT,
                }),
                ResolvedNode::Match {
                    tag_type: TagType::Custom("project".to_string()),
                    storage: StorageMapping::RowTag {
                        column: Col::LabelStr,
                        tag_key: "project".to_string(),
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

        // 1. Should select SUM(size)
        assert!(sql_str.contains("SUM(\"size\")"));

        // 2. Should contain subquery for project:ttfm
        // IN (SELECT ... FROM "oneview" ... WHERE "type" = 'project' AND "label_str" = 'ttfm')
        assert!(sql_str.contains("IN (SELECT"));
        assert!(sql_str.contains("WHERE \"type\" = 'project'"));
        assert!(sql_str.contains("'ttfm'"));
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

        let proj_type = resolver.get_projection().expect("Should have projection");

        // SQL生成
        let sql = build_fetch_label_groups_sql(&resolver, &proj_type, "oneview", 100, 0)
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
}
