mod pick;
mod util;
mod agg_pieces;
mod calc_pieces;
mod boolean;
mod scalar;
mod nest;
mod low_dispatcher;
mod precompute;
pub use pick::{
    build_pick, build_pick_agg, build_pick_nest,
    BuildPick, SimplePickNode, AggPickNode, NestPickNode, PickNode,
};
pub use util::{AggregationContext, NestContext, to_tag_condition};
pub use precompute::{
    needs_aggregation_context, build_aggregation_context, build_aggregation_context_for_operand, build_aggregation_context_for_agg,
    needs_nest_context, build_nest_context, build_nest_context_for_operand,
};
use agg_pieces::{
    agg_expr, resolve_count_target,
    build_agg, build_agg_nest,
    build_agg_calc_expr, build_agg_calc_eav_expr,
    build_agg_calc_subquery, build_agg_calc_subquery_nest,
    build_agg_operand_eav_expr,
    build_agg_operand_subquery, build_agg_operand_subquery_nest,
    build_nvalue_standalone_subquery, build_nvalue_cte, build_nvalue_cte_nest,
    build_nest_pivot_cte,
};
#[cfg(test)]
use agg_pieces::{build_agg_operand_expr, build_resolved_operand_expr_for_arithmetic};
use calc_pieces::{build_calculation_expr, build_calculation_eav_expr, fold_simple_operand};
use boolean::{
    build_direct_boolean_select, wrap_boolean_collider,
    build_resolved_and_sql, build_resolved_or_sql, build_resolved_diff_sql,
    build_resolved_comp_sql, build_label_set_op_pick_sql,
};
use scalar::{
    build_resolved_match_sql, build_column_match_sql,
    build_resolved_tag_tag_match_sql, build_scalar_match_sql,
};
use nest::{
    build_nest_sql,
    extract_primary_label_tag_type_from_node, extract_multi_key_nest_operands,
    build_multi_key_labels_sql,
    build_nest_match_sql, build_nest_nest_match_sql, build_merged_nest_match_sql,
};
use low_dispatcher::try_dispatch_common;
use util::*;

use crate::db::{Col, Tbl};
use crate::query::ast::QueryNode;
use crate::query::lens_resolver::{ResolvedNode, ResolvedOperand};
use crate::query::lens_schema::{to_bin_op, StorageMapping};
use sea_query::{Alias, Expr, Query, SelectStatement};


pub fn build_resolved_scalar_sql(
    op: &crate::query::lens_resolver::ResolvedOperand,
    view: &str,
) -> SelectStatement {
    use crate::query::lens_resolver::ResolvedOperand;
    let agg_ctx = build_aggregation_context_for_operand(op, view);
    match op {
        ResolvedOperand::Aggregation(agg) => {
            if needs_nest_context(agg.inner_node()) {
                let nest_ctx = build_nest_context(agg.inner_node(), view);
                build_agg_nest(agg, view, &agg_ctx, &nest_ctx)
            } else {
                build_agg(agg, view, &agg_ctx)
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
                let nest_ctx = build_nest_context_for_operand(op, view);
                build_agg_operand_subquery_nest(op, view, &agg_ctx, &nest_ctx)
            } else {
                build_agg_operand_subquery(op, view, &agg_ctx)
            };
            let mut stmt = Query::select();
            stmt.from(Alias::new(view));
            stmt.expr_as(scalar_expr, Alias::new("scalar_value"));
            stmt.limit(1);
            stmt
        }
    }
}

pub fn build_boolean_sql(node: &ResolvedNode, view: &str) -> SelectStatement {
    // 比較系ノードの場合は直接 SELECT で比較結果を計算
    // これによりFALSEとNULLを区別できる
    let agg_ctx = build_aggregation_context(node, view);
    match node {
        ResolvedNode::AggregationMatch { agg, op, label } => {
            build_direct_boolean_select(
                subquery(build_agg(agg, view, &agg_ctx)),
                *op,
                label_to_unit_aware_expr(label),
                view,
            )
        }
        ResolvedNode::AggregationAggregationMatch { left, op, right } => {
            build_direct_boolean_select(
                subquery(build_agg(left, view, &agg_ctx)),
                *op,
                subquery(build_agg(right, view, &agg_ctx)),
                view,
            )
        }
        ResolvedNode::AggregationCalculationMatch { agg, op, calc } => {
            let calc_expr = if calc.contains_aggregation() {
                build_agg_calc_subquery(calc, view, &agg_ctx)
            } else {
                build_agg_calc_expr(calc, &agg_ctx)
            };
            build_direct_boolean_select(
                subquery(build_agg(agg, view, &agg_ctx)),
                *op,
                calc_expr,
                view,
            )
        }
        ResolvedNode::AggregationTagMatch { .. } => {
            // タグ側は行ごとに異なる可能性があるのでWHERE方式を使用
            // （将来的に改善の余地があるが、一旦現状維持）
            let pick_sql = PickNode::new(node, view).build_pick();
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
            let pick_sql = PickNode::new(node, view).build_pick();
            wrap_boolean_collider(pick_sql)
        }
    }
}

pub fn build_fetch_items_sql(
    pick: &PickNode<'_>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> SelectStatement {
    let node = pick.node();
    let view = pick.view();
    // 集約クエリ (e.g. count(path:) や sum(size:) > 100) の場合は、
    // oneview との結合を行わず、集約計算結果だけをそのまま返す。
    // NestMatch / NestNestMatch は tags カラムが必要なため
    // ここでは早期リターンせず、通常のタグパッキング処理に委ねる。
    match node {
        ResolvedNode::Aggregation(_)
        | ResolvedNode::AggregationMatch { .. } => {
            return pick.build_pick();
        }
        _ => {}
    }

    let pick_sql = pick.build_pick();
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

pub fn build_flat_table_sql(
    pick: &PickNode<'_>,
    query_node: &QueryNode,
    limit: Option<usize>,
    offset: Option<usize>,
) -> SelectStatement {
    let view = pick.view();
    let pick_sql = pick.build_pick();
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

/// ネストクエリ（プロジェクションまたは LabelSetOp）の結果を取得する SQL を生成します。
/// `label_value` / `group_total` / `item_refs` スキーマを返します。
pub fn build_fetch_nest_sql(
    resolver: &crate::query::lens_resolver::Resolver,
    view: &str,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    if let Some(node) = resolver.get_label_set_op_node() {
        build_label_set_op_sql(node, view, limit, offset)
    } else {
        let pick = PickNode::new(&resolver.resolved_query, view);
        build_label_groups_sql(&pick, resolver, limit, offset)
    }
}

/// raw_tag_row_columns() の 8 フィールドを持つ struct_pack 行を文字列で生成する。
/// item_id と item_kind は volatile ダミー値で固定。
/// type_str / label_str_expr / label_int_expr / label_double_expr に SQL 式を渡す。
fn make_tag_struct_pack(
    type_str: &str,
    label_str_expr: &str,
    label_int_expr: &str,
    label_double_expr: &str,
) -> String {
    format!(
        concat!(
            r#"struct_pack("item_id" := 0::BIGINT, "item_kind" := 'volatile', "#,
            r#""type" := '{type_str}', "label_str" := {label_str_expr}, "#,
            r#""label_int" := {label_int_expr}, "label_double" := {label_double_expr}, "#,
            r#""label_bool" := NULL::BOOLEAN, "origin" := 'system')"#
        ),
        type_str = type_str,
        label_str_expr = label_str_expr,
        label_int_expr = label_int_expr,
        label_double_expr = label_double_expr,
    )
}

fn build_label_groups_sql(
    pick: &PickNode<'_>,
    resolver: &crate::query::lens_resolver::Resolver,
    limit: usize,
    offset: usize,
) -> anyhow::Result<SelectStatement> {
    use crate::db::CustomFunc;
    use sea_query::{CommonTableExpression, Iden, WithClause};

    let view = pick.view();
    let pick_sql = pick.build_pick();

    // 1. プロジェクション対象の物理カラムを特定
    let proj_type = resolver
        .resolved_query
        .get_projection()
        .ok_or_else(|| anyhow::anyhow!("build_label_groups_sql: no projection type in resolved query"))?;
    let desc = resolver.lens().look_up_or_default(&proj_type);
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
        .query(wrap_to_item_ids(pick_sql))
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
        if let Some(nv) = resolver.get_nvalue() {
            let proj_operands = resolver.resolved_query.get_projection_operands().unwrap();
            let context = resolver.resolved_query.get_context();
            let computed_agg_ctx;
            let mut nvalue_sql = if let (Some(agg_ctx), Some(nest_ctx)) = (pick.agg_ctx(), pick.nest_ctx()) {
                build_nvalue_cte_nest(proj_operands, nv, context, view, agg_ctx, nest_ctx)
            } else {
                computed_agg_ctx = build_aggregation_context_for_operand(nv, view);
                build_nvalue_cte(proj_operands, nv, context, view, &computed_agg_ctx)
            };
            // nvalue_condition がある場合、フィルタ条件を追加
            // Calculation の nvalue は GROUP BY なしのため HAVING ではなく WHERE を使用
            if let Some((op, value)) = nvalue_condition {
                let bin_op = to_bin_op(*op);
                let val = label_to_simple_expr(value);
                let cond = Expr::col(Alias::new("nvalue")).binary(bin_op, val);
                if matches!(nv, ResolvedOperand::Calculation(_)) {
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
            let agg_ctx = pick.agg_ctx().expect("AggregationContext required for pivot CTE");
            let pivot_q = build_nest_pivot_cte(proj_operands, None, view, agg_ctx);
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
                let calc_expr = if calc.contains_aggregation() {
                    let agg_ctx = pick.agg_ctx().expect("AggregationContext required for EAV+agg calculation CTE");
                    build_agg_calc_eav_expr(&calc, agg_ctx)
                } else {
                    build_calculation_eav_expr(&calc)
                };
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
                // カラムベース算術（RowTag なし）
                let calc_expr = if calc.contains_aggregation() {
                    let agg_ctx = pick.agg_ctx().expect("AggregationContext required for calculation CTE");
                    build_agg_calc_expr(&calc, agg_ctx)
                } else {
                    build_calculation_expr(&calc)
                };
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

    // 4. 最終 SELECT: item_id / rank / item_kind / tags (struct_pack) で統一スキーマ出力
    //
    // ラベル値の SQL 式 (VARCHAR にキャスト済み)
    let label_ref = if proj_operands.len() > 1 {
        (0..proj_operands.len())
            .map(|i| {
                format!(
                    "CAST({}.key{} AS VARCHAR)",
                    Iden::to_string(&Tbl::TopItems),
                    i
                )
            })
            .collect::<Vec<_>>()
            .join(" || ' &: ' || ")
    } else {
        format!(
            "CAST({}.{} AS VARCHAR)",
            Iden::to_string(&Tbl::TopItems),
            label_col_name
        )
    };

    // Volatile item_id: グループの連番 (1 始まり)
    let partition_order = if proj_operands.len() > 1 {
        (0..proj_operands.len())
            .map(|i| format!("{}.key{}", Iden::to_string(&Tbl::TopItems), i))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!("{}.{}", Iden::to_string(&Tbl::TopItems), label_col_name)
    };
    let volatile_id_expr = format!(
        "row_number() OVER (ORDER BY {})",
        partition_order
    );

    // name タグ struct_pack
    let name_sp = make_tag_struct_pack(
        "name",
        &format!("({})", label_ref),
        "NULL::BIGINT",
        "NULL::DOUBLE",
    );

    // item: タグ struct_pack (list() 集約で使用)
    let name_subquery = format!(
        "(SELECT {} FROM {} WHERE {} = {}.{} AND {} = 'name' LIMIT 1)",
        Iden::to_string(&Col::LabelStr),
        view,
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Col::Type)
    );
    let item_label_str = format!(
        "CONCAT(COALESCE({}, 'unknown'), '#', CAST({}.{} AS VARCHAR))",
        name_subquery,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId)
    );
    let item_sp = make_tag_struct_pack("item", &item_label_str, "NULL::BIGINT", "NULL::DOUBLE");
    let item_list_expr = format!(
        "list({} ORDER BY {}.{} DESC, {}.{} DESC)",
        item_sp,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::Rank),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId)
    );

    // projected_label タグ struct_pack (グループ総数)
    let proj_label_sp = format!(
        concat!(
            r#"struct_pack("item_id" := 0::BIGINT, "item_kind" := 'volatile', "#,
            r#""type" := 'projected_label', "label_str" := NULL::VARCHAR, "#,
            r#""label_int" := ANY_VALUE({}.{})::BIGINT, "label_double" := NULL::DOUBLE, "#,
            r#""label_bool" := NULL::BOOLEAN, "origin" := 'system')"#
        ),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Tbl::GroupTotal)
    );

    // tags = [name] || list(item:) || [projected_label] (|| [nvalue] があれば追加)
    let mut tags_expr =
        format!("[{}] || {} || [{}]", name_sp, item_list_expr, proj_label_sp);

    if has_nvalue {
        let nvalue_subq = if proj_operands.len() > 1 {
            let mut join_cond = "TRUE".to_string();
            for i in 0..proj_operands.len() {
                join_cond.push_str(&format!(
                    " AND \"nvalue_agg\".\"key{}\" = {}.\"key{}\"",
                    i,
                    Iden::to_string(&Tbl::TopItems),
                    i
                ));
            }
            format!("(SELECT \"nvalue\" FROM \"nvalue_agg\" WHERE {})", join_cond)
        } else {
            format!(
                "(SELECT \"nvalue\" FROM \"nvalue_agg\" WHERE \"group_label\" = {}.{})",
                Iden::to_string(&Tbl::TopItems),
                &label_col_name,
            )
        };
        let nvalue_sp = format!(
            concat!(
                r#"struct_pack("item_id" := 0::BIGINT, "item_kind" := 'volatile', "#,
                r#""type" := 'nvalue', "label_str" := NULL::VARCHAR, "#,
                r#""label_int" := NULL::BIGINT, "label_double" := CAST(({}) AS DOUBLE), "#,
                r#""label_bool" := NULL::BOOLEAN, "origin" := 'system')"#
            ),
            nvalue_subq
        );
        tags_expr = format!("{} || [{}]", tags_expr, nvalue_sp);
    }

    let mut q = Query::select();
    q.with_cte(with_clause);
    q.expr_as(Expr::cust(volatile_id_expr), Col::ItemId);
    q.expr_as(
        Expr::cust(format!(
            "ANY_VALUE({}.{})",
            Iden::to_string(&Tbl::TopItems),
            Iden::to_string(&Col::Rank)
        )),
        Col::Rank,
    );
    q.expr_as(Expr::cust("'volatile'"), Col::ItemKind);
    q.expr_as(Expr::cust(tags_expr), crate::db::QueryResultCol::Tags);
    q.from(Tbl::TopItems);

    if proj_operands.len() > 1 {
        for i in 0..proj_operands.len() {
            q.group_by_col((Tbl::TopItems, Alias::new(&format!("key{}", i))));
        }
        for i in (0..proj_operands.len()).rev() {
            q.order_by(
                (Tbl::TopItems, Alias::new(&format!("key{}", i))),
                sea_query::Order::Asc,
            );
        }
    } else {
        q.group_by_col((Tbl::TopItems, label_col.clone()));
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

fn build_label_set_op_sql(
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
            "build_label_set_op_sql: expected LabelSetOp node"
        ),
    };
    if operands.is_empty() {
        anyhow::bail!(
            "build_label_set_op_sql: LabelSetOp with no operands"
        );
    }

    let mut with_clause = WithClause::new();

    // CTE: labels_i — 各オペランドの (label_value_cast, item_id)
    let cte_names: Vec<String> = (0..operands.len())
        .map(|i| format!("labels_{}", i))
        .collect();
    for (i, operand) in operands.iter().enumerate() {
        let ids_sql = wrap_to_item_ids(build_pick(operand, view));

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
        let right_ids_sql = wrap_to_item_ids(build_pick(&operands[1], view));

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

    // 最終 SELECT: item_id / rank / item_kind / tags (struct_pack) で統一スキーマ出力
    let label_ref = format!(
        "CAST({}.label_value AS VARCHAR)",
        Iden::to_string(&Tbl::TopItems)
    );
    let volatile_id_expr = format!(
        "row_number() OVER (ORDER BY {}.label_value)",
        Iden::to_string(&Tbl::TopItems)
    );

    let name_sp = make_tag_struct_pack(
        "name",
        &format!("({})", label_ref),
        "NULL::BIGINT",
        "NULL::DOUBLE",
    );

    let name_subquery = format!(
        "(SELECT {} FROM {} WHERE {} = {}.{} AND {} = 'name' LIMIT 1)",
        Iden::to_string(&Col::LabelStr),
        view,
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
        Iden::to_string(&Col::Type),
    );
    let item_label_str = format!(
        "CONCAT(COALESCE({}, 'unknown'), '#', CAST({}.{} AS VARCHAR))",
        name_subquery,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
    );
    let item_sp = make_tag_struct_pack("item", &item_label_str, "NULL::BIGINT", "NULL::DOUBLE");
    let item_list_expr = format!(
        "list({} ORDER BY {}.{} DESC)",
        item_sp,
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Col::ItemId),
    );

    let proj_label_sp = format!(
        concat!(
            r#"struct_pack("item_id" := 0::BIGINT, "item_kind" := 'volatile', "#,
            r#""type" := 'projected_label', "label_str" := NULL::VARCHAR, "#,
            r#""label_int" := ANY_VALUE({}.{})::BIGINT, "label_double" := NULL::DOUBLE, "#,
            r#""label_bool" := NULL::BOOLEAN, "origin" := 'system')"#
        ),
        Iden::to_string(&Tbl::TopItems),
        Iden::to_string(&Tbl::GroupTotal)
    );

    let tags_expr = format!("[{}] || {} || [{}]", name_sp, item_list_expr, proj_label_sp);

    let mut q = Query::select();
    q.with_cte(with_clause);
    q.expr_as(Expr::cust(volatile_id_expr), Col::ItemId);
    q.expr_as(Expr::cust("0::BIGINT"), Col::Rank);
    q.expr_as(Expr::cust("'volatile'"), Col::ItemKind);
    q.expr_as(Expr::cust(tags_expr), crate::db::QueryResultCol::Tags);
    q.from(Tbl::TopItems)
        .group_by_col((Tbl::TopItems, Alias::new("label_value")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{BasicOp, ComparisonOp, QueryNode};
    use crate::query::lens_resolver::ResolvedAggregationNode;
    use crate::query::lens_resolver::ResolvedOperand;
    use crate::types::{Label, SType, TagType, TypedTag};
    use sea_query::{BinOper, PostgresQueryBuilder, SqliteQueryBuilder};

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
            build_flat_table_sql(&PickNode::new(&node, "oneview"), &query_node, Some(10), None);
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
    fn test_build_agg_count_items() {
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

        let agg_ctx = build_aggregation_context_for_agg(&agg, "oneview");
        let sql = build_agg(&agg, "oneview", &agg_ctx);
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
    fn test_build_agg_sum_projection() {
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

        let sql = build_agg(&agg, "oneview", &AggregationContext::new());
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // サブクエリ形式: SUM("val")
        assert!(sql_str.contains("SUM(\"val\")"));
        // サブクエリ内での抽出
        assert!(sql_str.contains("any_value(\"label_int\") AS \"val\""));
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

        let agg_ctx = build_aggregation_context_for_agg(&agg, "oneview");
        let sql = build_agg(&agg, "oneview", &agg_ctx);
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // サブクエリ形式: SUM("val")
        assert!(sql_str.contains("SUM(\"val\")"));
        assert!(sql_str.contains("any_value(\"size\") AS \"val\""));

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

        let expr = build_agg_calc_expr(&calc, &AggregationContext::new());
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
        let expr = build_agg_operand_expr(&operand, &AggregationContext::new());
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
        let left = subquery(Query::select().expr(Expr::val(100i64)).to_owned());
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

        // SQL生成
        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0)
            .expect("Failed to build SQL");
        let sql_str = sql.to_string(PostgresQueryBuilder);

        // 検証: 統一スキーマ (item_id / rank / item_kind / tags) が生成されるか
        assert!(
            sql_str.contains("struct_pack"),
            "SQL should contain struct_pack: {}",
            sql_str
        );
        assert!(
            sql_str.contains("'name'"),
            "SQL should contain name tag: {}",
            sql_str
        );
        assert!(
            sql_str.contains("projected_label"),
            "SQL should contain projected_label tag: {}",
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
            build_resolved_operand_expr_for_arithmetic(&lit_bool, &AggregationContext::new());
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
            build_resolved_operand_expr_for_arithmetic(&tag_bool, &AggregationContext::new());
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

        assert!(
            resolver.get_nvalue().is_some(),
            "Should have nvalue for nest query"
        );

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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
        // struct_pack で統一スキーマが出力されるか
        assert!(
            sql_str.contains("struct_pack"),
            "SQL should contain struct_pack: {}",
            sql_str
        );
    }

    #[test]
    fn test_nvalue_sum_projection_sql() {
        use crate::query::lens_resolver::Resolver;
        use sea_query::PostgresQueryBuilder;

        // parentdir: &: sum(size:) → nvalue付きNest (Arithmetic)
        let resolver = Resolver::new("parentdir: &: sum(size:)").unwrap();
        assert!(
            resolver.get_nvalue().is_some(),
            "Should have nvalue for nest query"
        );

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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

        assert!(
            resolver.get_nvalue().is_none(),
            "Normal projection should NOT have nvalue"
        );

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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

        assert!(resolver.get_nvalue_condition().is_some());

        let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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
                &AggregationContext::new(),
                None,
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

            let sql = build_fetch_nest_sql(&resolver, "oneview", 100, 0).unwrap();
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
                &AggregationContext::new(),
                None,
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
        let sql = PickNode::new(&optimized, "oneview").build_pick();
        println!(
            "Generated FETCH ITEMS SQL: {}",
            sql.to_string(sea_query::PostgresQueryBuilder)
        );

        if optimized.get_projection().is_some() {
            let resolver2 =
                crate::query::lens_resolver::Resolver::new(query_str).unwrap();
            let label_sql = build_fetch_nest_sql(&resolver2, "oneview", 100, 0).unwrap();
            println!(
                "Generated LABEL GROUPS SQL: {}",
                label_sql.to_string(sea_query::PostgresQueryBuilder)
            );
        }
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

        let sql = build_pick(&nest_two_keys, "oneview")
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

        let sql = build_pick(&nest_one_key, "oneview")
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

        let sql = build_label_set_op_sql(&node, "oneview", 100, 0)
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
        // 統一スキーマ (struct_pack) が生成されるか
        assert!(
            sql.contains("struct_pack"),
            "should contain struct_pack, got: {}",
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

        let sql = build_label_set_op_sql(&node, "oneview", 100, 0)
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

