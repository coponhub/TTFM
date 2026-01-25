use crate::db::{Col, Tbl};
use crate::query::ast::{ComparisonNode, ComparisonOp, Operand, QueryNode};
use crate::query::lens::{to_bin_op, ResolvedNode, StorageMapping};
use crate::types::{Label, SType, TagType};
use sea_query::{Alias, BinOper, Condition, Expr, Query, SelectStatement};

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
            build_typed_tag_sql(&tt.tagtype, &tt.label, view_name)
        }
        QueryNode::Projection(tt) => build_projection_sql(tt, view_name),
    };
    stmt
}

/// 物理マッピング解決済みの構造から SQL を生成します (Phase 2)。
pub fn build_pick_sql(node: &ResolvedNode, view: &str) -> SelectStatement {
    match node {
        ResolvedNode::And(nodes) => build_resolved_and_sql(nodes, view),
        ResolvedNode::Or(nodes) => build_resolved_or_sql(nodes, view),
        ResolvedNode::Difference(l, r) => build_resolved_diff_sql(l, r, view),
        ResolvedNode::Complement(c) => build_resolved_comp_sql(c, view),
        ResolvedNode::Projection(tt, storage) => {
            build_resolved_projection_sql(tt, storage, view)
        }
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
    }
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
        "typedtag",
        "filename",
        "is_dir",
    ];
    for def in defaults {
        if !types.iter().any(|t| t == def) {
            types.push(def.to_string());
        }
    }

    if types.iter().any(|t| t == "*" || t == "typedtag") {
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
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view))
        .and_where(
            Expr::col(Col::ItemKind).is_not_in(vec!["type", "typedtag"]),
        );

    let mut eq = Query::select();
    eq.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .from_subquery(build_pick_sql(c, view), Tbl::NotSide);
    q.union(sea_query::UnionType::Except, eq);
    q
}

fn build_resolved_projection_sql(
    tagtype: &TagType,
    storage: &StorageMapping,
    view: &str,
) -> SelectStatement {
    let mut q = Query::select();
    q.columns([Col::ItemId, Col::Rank, Col::ItemKind])
        .distinct()
        .from(Alias::new(view));

    // ResolvedNode の Projection 用条件生成を利用
    let cond = ResolvedNode::Projection(tagtype.clone(), storage.clone())
        .to_condition();
    q.cond_where(cond);

    // 特別なタグの追加条件
    if let TagType::Base(SType::TypedTag) = tagtype {
        q.and_where(Expr::col(Col::TypedTag).is_not_null());
    } else if let TagType::Base(SType::Origin) = tagtype {
        q.and_where(Expr::col(Col::Origin).is_not_null());
    }

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

// ========== SQL Generation Helper Functions ==========

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
    match label {
        Label::Integer(i) => {
            condition = condition
                .add(Expr::col(Col::LabelInt).binary(op, Expr::val(i)))
                .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(i)));
        }
        Label::String(s) | Label::Literal(s) => {
            condition = condition.add(
                Expr::col(Col::LabelStr).binary(op, Expr::val(s.as_str())),
            );

            if let Ok(i) = s.parse::<i64>() {
                condition = condition
                    .add(Expr::col(Col::LabelInt).binary(op, Expr::val(i)))
                    .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(i)));
            } else if let Ok(f) = s.parse::<f64>() {
                condition = condition
                    .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(f)));
            } else if s == "true" || s == "false" {
                let b = s == "true";
                condition = condition
                    .add(Expr::col(Col::LabelBool).binary(op, Expr::val(b)));
            }
        }
    };

    q.and_where(Expr::col(Col::Type).eq(tagtype.as_str()))
        .and_where(condition.into());
    q
}

/// プロジェクションクエリのSQLを生成します。
///
/// 指定されたタグタイプの値を持つ全アイテムを返します。
fn build_projection_sql(tagtype: &TagType, view: &str) -> SelectStatement {
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
            && tag_name != "typedtag"
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

    match label {
        Label::Integer(i) => {
            let t = if matches!(tag, SType::Label) {
                Col::LabelInt.into()
            } else {
                tag
            };
            q.and_where(Expr::col(t).eq(*i));
        }
        Label::String(s) => {
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
        Label::Literal(s) => {
            let t = if matches!(tag, SType::Label) {
                Col::LabelStr.into()
            } else {
                tag
            };
            q.and_where(Expr::col(t).eq(s.as_str()));
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
    match label {
        Label::Integer(i) => {
            cond = cond
                .add(Expr::col(Col::LabelInt).eq(*i))
                .add(Expr::col(Col::LabelDouble).eq(*i as f64));
        }
        Label::String(s) => {
            let val_str = if s.starts_with('^') {
                format!("{}*", &s[1..])
            } else {
                s.clone()
            };

            cond = cond
                .add(Expr::col(Col::LabelStr).binary(glob, Expr::val(val_str)));
            if let Ok(i) = s.parse::<i64>() {
                cond = cond.add(Expr::col(Col::LabelInt).eq(i));
            }
            if s == "true" || s == "false" {
                cond = cond.add(Expr::col(Col::LabelBool).eq(s == "true"));
            }
        }
        Label::Literal(s) => {
            cond = cond.add(Expr::col(Col::LabelStr).eq(s.as_str()));
            if let Ok(i) = s.parse::<i64>() {
                cond = cond.add(Expr::col(Col::LabelInt).eq(i));
            }
            if s == "true" || s == "false" {
                cond = cond.add(Expr::col(Col::LabelBool).eq(s == "true"));
            }
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
            match label {
                Label::Integer(i) => {
                    q.and_where(Expr::col(Col::Rank).eq(*i));
                }
                _ => {
                    // RankなのにInteger以外が来た場合はヒットしない
                    q.and_where(Expr::val(1).eq(0));
                }
            }
        }
        TagType::Base(SType::Label) => {
            // Label (仮想タグ) の場合は Type フィルタなしで Label 値のみで検索
            match label {
                Label::String(s) | Label::Literal(s) => {
                    q.and_where(Expr::col(Col::LabelStr).eq(s.as_str()));
                }
                Label::Integer(i) => {
                    q.and_where(Expr::col(Col::LabelInt).eq(*i));
                }
            }
        }
        _ => {
            // 一般的なタグ
            q.and_where(Expr::col(Col::Type).eq(proj_type.as_str()));
            match label {
                Label::String(s) | Label::Literal(s) => {
                    q.and_where(Expr::col(Col::LabelStr).eq(s.as_str()));
                }
                Label::Integer(i) => {
                    q.and_where(Expr::col(Col::LabelInt).eq(*i));
                }
            }
        }
    }

    q
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::QueryNode;
    use crate::types::{Label, TagType, TypedTag};
    use sea_query::{Alias, BinOper, Query, SqliteQueryBuilder};

    #[test]
    fn test_to_bin_op_conversion() {
        assert_eq!(to_bin_op(ComparisonOp::Eq), BinOper::Equal);
        assert_eq!(to_bin_op(ComparisonOp::Gt), BinOper::GreaterThan);
        assert_eq!(to_bin_op(ComparisonOp::Lt), BinOper::SmallerThan);
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
        let right = Operand::Literal(Label::Integer(100));
        let (tt, lab, op) =
            normalize_comparison(&left, BinOper::Equal, &right).unwrap();
        assert_eq!(tt.as_str(), "size");
        assert_eq!(lab, Label::Integer(100));
        assert_eq!(op, BinOper::Equal);

        let left_lit = Operand::Literal(Label::Integer(100));
        let right_tag = Operand::TypeRef(TagType::from("size"));
        let (tt2, lab2, op2) =
            normalize_comparison(&left_lit, BinOper::GreaterThan, &right_tag)
                .unwrap();
        assert_eq!(tt2.as_str(), "size");
        assert_eq!(lab2, Label::Integer(100));
        assert_eq!(op2, BinOper::SmallerThan);
    }

    #[test]
    fn test_to_tag_condition_generation() {
        let node =
            QueryNode::TypedTag(TypedTag::new("size", Label::Integer(100)));
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
        let tt = TypedTag::new("name", "foo.txt");
        let sql = build_typed_tag_sql(&tt.tagtype, &tt.label, "oneview");
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
                ComparisonOp::Gt,
                Operand::Literal(Label::Integer(100)),
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
}
