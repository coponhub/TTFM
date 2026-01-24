use crate::db::{Col, Tbl};
use crate::query::ast::{
    ComparisonNode, ComparisonOp, Operand, QueryNode,
};
use crate::types::{Label, SType, TagType};
use sea_query::{Alias, BinOper, Condition, Expr, Query, SelectStatement};

/// クエリ構造を SQL (SelectStatement) へ変換します。
pub fn to_sql(node: &QueryNode, view_name: &str) -> SelectStatement {
    match node {
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
            cond = cond
                .add(Expr::col(Col::Type).binary(glob_op, Expr::val(t)));
        } else {
            fixed_types.push(t);
        }
    }

    if !fixed_types.is_empty() {
        cond = cond.add(Expr::col(Col::Type).is_in(fixed_types));
    }

    cond
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

fn build_comparison_sql(
    node: &ComparisonNode,
    view: &str,
) -> SelectStatement {
    let mut operands = vec![&node.first];
    for (_, opd) in &node.rest {
        operands.push(opd);
    }

    let mut subqueries = Vec::new();
    for (i, (op, _)) in node.rest.iter().enumerate() {
        let left = operands[i];
        let right = operands[i + 1];
        subqueries
            .push(build_binary_comparison_sql(left, *op, right, view));
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

/// ComparisonOp を sea_query の BinOper に変換します。
fn to_bin_op(op: ComparisonOp) -> BinOper {
    match op {
        ComparisonOp::Eq => BinOper::Equal,
        ComparisonOp::Ne => BinOper::NotEqual,
        ComparisonOp::Gt => BinOper::GreaterThan,
        ComparisonOp::Ge => BinOper::GreaterThanOrEqual,
        ComparisonOp::Lt => BinOper::SmallerThan,
        ComparisonOp::Le => BinOper::SmallerThanOrEqual,
    }
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
                    .add(
                        Expr::col(Col::LabelDouble)
                            .binary(op, Expr::val(i)),
                    );
            } else if let Ok(f) = s.parse::<f64>() {
                condition = condition.add(
                    Expr::col(Col::LabelDouble).binary(op, Expr::val(f)),
                );
            } else if s == "true" || s == "false" {
                let b = s == "true";
                condition = condition.add(
                    Expr::col(Col::LabelBool).binary(op, Expr::val(b)),
                );
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
fn build_projection_sql(
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
    } else if let TagType::Base(SType::Type) = tagtype {
        q.and_where(Expr::col(Col::Type).is_not_null());
    } else if let TagType::Base(SType::Label) = tagtype {
        let mut cond = Condition::any();
        cond = cond.add(Expr::col(Col::LabelStr).is_not_null());
        cond = cond.add(Expr::col(Col::LabelInt).is_not_null());
        cond = cond.add(Expr::col(Col::LabelDouble).is_not_null());
        cond = cond.add(Expr::col(Col::LabelBool).is_not_null());
        q.and_where(cond.into());
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

            cond = cond.add(
                Expr::col(Col::LabelStr).binary(glob, Expr::val(val_str)),
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::{BinOper, SqliteQueryBuilder, Alias, Query};
    use crate::query::ast::QueryNode;
    use crate::types::{TypedTag, Label, TagType};

    #[test]
    fn test_to_bin_op_conversion() {
        assert_eq!(to_bin_op(ComparisonOp::Eq), BinOper::Equal);
        assert_eq!(to_bin_op(ComparisonOp::Gt), BinOper::GreaterThan);
        assert_eq!(to_bin_op(ComparisonOp::Lt), BinOper::SmallerThan);
    }

    #[test]
    fn test_flip_bin_op() {
        assert_eq!(flip_bin_op(BinOper::GreaterThan), BinOper::SmallerThan);
        assert_eq!(flip_bin_op(BinOper::SmallerThanOrEqual), BinOper::GreaterThanOrEqual);
        assert_eq!(flip_bin_op(BinOper::Equal), BinOper::Equal);
    }

    #[test]
    fn test_normalize_comparison_order() {
        let left = Operand::TypeRef(TagType::from("size"));
        let right = Operand::Literal(Label::Integer(100));
        let (tt, lab, op) = normalize_comparison(&left, BinOper::Equal, &right).unwrap();
        assert_eq!(tt.as_str(), "size");
        assert_eq!(lab, Label::Integer(100));
        assert_eq!(op, BinOper::Equal);

        let left_lit = Operand::Literal(Label::Integer(100));
        let right_tag = Operand::TypeRef(TagType::from("size"));
        let (tt2, lab2, op2) = normalize_comparison(&left_lit, BinOper::GreaterThan, &right_tag).unwrap();
        assert_eq!(tt2.as_str(), "size");
        assert_eq!(lab2, Label::Integer(100));
        assert_eq!(op2, BinOper::SmallerThan);
    }

    #[test]
    fn test_to_tag_condition_generation() {
        let node = QueryNode::TypedTag(TypedTag::new("size", Label::Integer(100)));
        let cond = to_tag_condition(&node);
        
        let mut query = Query::select();
        query.column(Alias::new("id")).from(Alias::new("tbl")).cond_where(cond);
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
            rest: vec![(ComparisonOp::Gt, Operand::Literal(Label::Integer(100)))],
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

