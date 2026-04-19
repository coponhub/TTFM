use crate::db::{Col, CustomFunc, SqlType, Tbl};
use crate::query::ast::{ArithmeticAggOp, ArithmeticOp};
use crate::query::lens_resolver::ResolvedNode;
use crate::query::lens_schema::StorageMapping;
use crate::types::Label;
use sea_query::{Alias, BinOper, Expr, Func, Query, SelectStatement, SimpleExpr};
use std::collections::HashMap;

/// `SelectStatement` をインライン副問合せ式 (`SimpleExpr`) に変換します。
pub(super) fn subquery(stmt: SelectStatement) -> SimpleExpr {
    SimpleExpr::SubQuery(None, Box::new(stmt.into_sub_query_statement()))
}

/// SQL を `SELECT item_id FROM (sql) AS t` でラップします。
pub(super) fn wrap_to_item_ids(sql: SelectStatement) -> SelectStatement {
    Query::select()
        .column(Col::ItemId)
        .from_subquery(sql, Alias::new("t"))
        .to_owned()
}

/// SQL を `SELECT item_id, rank, item_kind FROM (sql) AS sub` でラップします。
/// UNION / INTERSECT / EXCEPT で子クエリを結合する際に使用します。
pub(super) fn wrap_in_subquery(q: SelectStatement) -> SelectStatement {
    Query::select()
        .columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from_subquery(q, Tbl::Sub)
        .to_owned()
}

/// `Label` の値を単純な SQL 式に変換します。
pub(super) fn label_to_simple_expr(label: &Label) -> SimpleExpr {
    use crate::types::LabelValue;
    match label.value() {
        LabelValue::Integer(i) => Expr::val(i).into(),
        LabelValue::Boolean(b) => Expr::val(b).into(),
        LabelValue::Double(bits) => Expr::val(f64::from_bits(bits)).into(),
        LabelValue::Null => Expr::val(Option::<i32>::None).into(),
        LabelValue::String(s) | LabelValue::Literal(s) => Expr::val(s).into(),
    }
}

/// `Label` の値をサイズ単位を考慮した SQL 式に変換します（例: "1MB" → 1048576）。
pub(super) fn label_to_unit_aware_expr(label: &Label) -> SimpleExpr {
    use crate::types::LabelValue;
    match label.value() {
        LabelValue::Integer(i) => Expr::val(i).into(),
        LabelValue::String(s) | LabelValue::Literal(s) => {
            if let Some(bytes) = crate::util::parse_size(&s) {
                Expr::val(bytes).into()
            } else {
                Expr::val(s.clone()).into()
            }
        }
        LabelValue::Boolean(b) => Expr::val(b).into(),
        LabelValue::Double(bits) => Expr::val(f64::from_bits(bits)).into(),
        LabelValue::Null => Expr::val(None::<i32>).into(),
    }
}

/// リテラルラベルを SQL 式に変換します。サイズ単位のパース（"1MB" → 1048576）および
/// 数値リテラルの DOUBLE キャストを行います。
pub(super) fn build_resolved_literal_expr(lab: &Label) -> SimpleExpr {
    use crate::types::LabelValue;
    let s = lab.as_str();
    if let Some(bytes) = crate::util::parse_size(&s) {
        Expr::val(bytes).cast_as(SqlType::DOUBLE).into()
    } else {
        match lab.value() {
            LabelValue::Integer(i) => Expr::val(i).cast_as(SqlType::DOUBLE).into(),
            LabelValue::String(s) | LabelValue::Literal(s) => Expr::val(s.clone()).into(),
            LabelValue::Boolean(b) => Expr::val(b).into(),
            LabelValue::Double(bits) => Expr::val(f64::from_bits(bits)).into(),
            LabelValue::Null => Expr::val(None::<i32>).into(),
        }
    }
}

/// `ArithmeticAggOp` を集約 SQL 式に適用します。
pub(super) fn apply_arithmetic_agg(
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

/// `StorageMapping` からカラム式を生成します。
/// 数値演算が必要な `RowTag.LabelStr` には `TRY_CAST` を適用します。
pub(super) fn build_storage_column_expr(
    storage: &StorageMapping,
    sql_type: SqlType,
) -> SimpleExpr {
    match storage {
        StorageMapping::Column(col) => Expr::col(*col).into(),
        StorageMapping::RowTag { column, .. } => {
            let col_expr = Expr::col(*column);
            if *column == Col::LabelStr
                && matches!(sql_type, SqlType::BIGINT | SqlType::DOUBLE)
            {
                Expr::cust_with_exprs("TRY_CAST($1 AS DOUBLE)", [col_expr.into()])
            } else {
                col_expr.into()
            }
        }
        StorageMapping::Virtual => {
            CustomFunc::any_value(Expr::col(Col::LabelStr)).into()
        }
    }
}

// ── AggregationContext / NestContext ───────────────────────────────────────

/// 集約ノードのフィルタ/内部SQL を事前計算した結果を保持します。
///
/// `build_pick_sql` に渡すことで fold 内での再帰呼び出しを排除します。
/// `needs_aggregation_context` で必要性を判定し、`build_aggregation_context` で構築します。
pub struct AggregationContext {
    /// inner_node ポインタ → フィルタ SQL (Phase 3 で参照)
    pub agg_filters: HashMap<usize, SelectStatement>,
    /// Count(inner) かつ inner_tag_type が None の場合の inner 全体 SQL (Phase 3 で参照)
    pub agg_inner_sqls: HashMap<usize, SelectStatement>,
    /// Phase 1 収集: フィルタノード (Phase 2 で SQL 化して agg_filters へ移動)
    pub filter_nodes: HashMap<usize, ResolvedNode>,
    /// Phase 1 収集: Count inner ノード (Phase 2 で SQL 化して agg_inner_sqls へ移動)
    pub inner_nodes: HashMap<usize, ResolvedNode>,
}

impl AggregationContext {
    pub fn new() -> Self {
        Self {
            agg_filters: HashMap::new(),
            agg_inner_sqls: HashMap::new(),
            filter_nodes: HashMap::new(),
            inner_nodes: HashMap::new(),
        }
    }
}

/// Nest コンテキストノードの SQL を事前計算した結果を保持します。
///
/// `build_pick_sql` に渡すことで fold 内での再帰呼び出しを排除します。
/// `needs_nest_context` で必要性を判定し、`build_nest_context` で構築します。
pub struct NestContext {
    /// コンテキスト ResolvedNode ポインタ → コンテキスト SQL (Phase 3 で参照)
    pub contexts: HashMap<usize, SelectStatement>,
    /// Phase 1 収集: コンテキストノード (Phase 2 で SQL 化して contexts へ移動)
    pub context_nodes: HashMap<usize, ResolvedNode>,
}

impl NestContext {
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
            context_nodes: HashMap::new(),
        }
    }
}

/// `ArithmeticOp` を二項演算 SQL 式に適用します。
pub(super) fn apply_arithmetic_op(
    op: &ArithmeticOp,
    left: SimpleExpr,
    right: SimpleExpr,
    is_string: bool,
) -> SimpleExpr {
    use ArithmeticOp::*;
    if is_string {
        return match op {
            Add => Expr::expr(left)
                .binary(BinOper::Custom("||"), Expr::val(", "))
                .binary(BinOper::Custom("||"), right),
            Mul => Expr::expr(left).binary(BinOper::Custom("||"), right),
            _ => Expr::expr(left).binary(BinOper::Custom("||"), right),
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
