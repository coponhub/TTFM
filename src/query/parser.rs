use crate::query::ast::{
    ArithmeticOp, CalculationNode, ComparisonNode, ComparisonOp, Operand,
    QueryNode,
};
use crate::types::{Label, TagType, TypedTag};
use crate::util::DotOk;
use anyhow::{anyhow, Result};
use pest::iterators::Pair;
use pest::pratt_parser::{Assoc, Op, PrattParser};
use pest::Parser;
use pest_derive::Parser;
use std::collections::VecDeque;
use std::sync::OnceLock;

#[derive(Parser)]
#[grammar = "query/query.pest"]
pub struct PestQueryParser;

static PRATT_PARSER: OnceLock<PrattParser<Rule>> = OnceLock::new();

// ========== Error Messages ==========
mod errors {
    pub const PARSE_ERROR: &str = "Parse error";
    pub const NO_QUERY_FOUND: &str = "No query found";
    pub const NO_EXPRESSION_FOUND: &str = "No expression found";
    pub const UNKNOWN_INFIX_RULE: &str = "Unknown infix rule";
    pub const UNKNOWN_FACTOR_INNER: &str = "Unknown factor inner";
    pub const COMPLEMENT_MISSING_EXPR: &str = "Complement missing expr";
    pub const UNEXPECTED_RULE: &str = "Unexpected rule in build_ast";
    pub const MISSING_TAG_KEY: &str = "Missing tag key";
    pub const MISSING_TAG_LABEL: &str = "Missing tag label";
    pub const MISSING_PROJECTION_INNER: &str = "Missing projection inner";
    pub const MISSING_TAG_KEY_IN_PROJECTION: &str =
        "Missing tag key in projection";
    pub const UNEXPECTED_TAG_TYPE_RULE: &str = "Unexpected tag_type rule";
    pub const UNKNOWN_COMPARISON_OP: &str = "Unknown comparison op";
    pub const MISSING_COMPARISON_OPERAND: &str = "Missing comparison operand";
    pub const UNKNOWN_OPERAND_RULE: &str = "Unknown operand rule";
    pub const UNKNOWN_ARITHMETIC_OP: &str = "Unknown arithmetic op";
    pub const CALC_REQUIRES_OP: &str =
        "Calculation must contain at least one operation";
    pub const UNKNOWN_OPERAND_CALC_RULE: &str = "Unknown operand_calc rule";
    pub const UNKNOWN_LABEL_RULE: &str = "Unknown label rule";
}

fn get_parser() -> &'static PrattParser<Rule> {
    PRATT_PARSER.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::pipe, Assoc::Left)
                | Op::infix(Rule::minus, Assoc::Left))
            .op(Op::infix(Rule::ampersand, Assoc::Left))
    })
}

/// クエリ文字列を解析し、QueryNode AST を構築します。
pub fn parse(input: &str) -> Result<QueryNode> {
    let mut pairs = PestQueryParser::parse(Rule::query, input)
        .map_err(|e| anyhow!("{}: {}", errors::PARSE_ERROR, e))?;
    let expr_pair = pairs
        .next()
        .ok_or_else(|| anyhow!(errors::NO_QUERY_FOUND))?
        .into_inner()
        .next()
        .ok_or_else(|| anyhow!(errors::NO_EXPRESSION_FOUND))?;
    build_ast(expr_pair)
}

fn build_ast(pair: Pair<Rule>) -> Result<QueryNode> {
    match pair.as_rule() {
        Rule::expr => build_expr(pair),
        Rule::primary => build_primary(pair),
        Rule::factor => build_factor(pair),
        Rule::complement => build_complement(pair),
        _ => Err(anyhow!("{}: {:?}", errors::UNEXPECTED_RULE, pair.as_rule())),
    }
}

fn build_expr(pair: Pair<Rule>) -> Result<QueryNode> {
    let pairs = pair.into_inner();
    get_parser()
        .map_primary(|primary| build_ast(primary))
        .map_infix(|lhs, op, rhs| {
            let lhs = lhs?;
            let rhs = rhs?;
            match op.as_rule() {
                Rule::ampersand => {
                    // Combine And nodes if possible
                    match lhs {
                        QueryNode::And(mut v) => {
                            v.push(rhs);
                            Ok(QueryNode::And(v))
                        }
                        _ => Ok(QueryNode::And(vec![lhs, rhs])),
                    }
                }
                Rule::pipe => match lhs {
                    QueryNode::Or(mut v) => {
                        v.push(rhs);
                        Ok(QueryNode::Or(v))
                    }
                    _ => Ok(QueryNode::Or(vec![lhs, rhs])),
                },
                Rule::minus => {
                    Ok(QueryNode::Difference(Box::new(lhs), Box::new(rhs)))
                }
                _ => Err(anyhow!(
                    "{}: {:?}",
                    errors::UNKNOWN_INFIX_RULE,
                    op.as_rule()
                )),
            }
        })
        .parse(pairs)
}

fn build_primary(pair: Pair<Rule>) -> Result<QueryNode> {
    let inner = pair.into_inner().next().unwrap();
    build_ast(inner)
}

fn build_factor(pair: Pair<Rule>) -> Result<QueryNode> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::expr => build_ast(inner),
        Rule::typed_tag => build_typed_tag(inner),
        Rule::comparison => build_comparison(inner),
        Rule::projection => build_projection(inner),
        _ => Err(anyhow!(
            "{}: {:?}",
            errors::UNKNOWN_FACTOR_INNER,
            inner.as_rule()
        )),
    }
}

fn build_complement(pair: Pair<Rule>) -> Result<QueryNode> {
    let mut inner = pair.into_inner();
    let _ = inner.next(); // Skip '^' token
    let expr_pair = inner
        .next()
        .ok_or_else(|| anyhow!(errors::COMPLEMENT_MISSING_EXPR))?;
    Ok(QueryNode::Complement(Box::new(build_ast(expr_pair)?)))
}

fn build_typed_tag(pair: Pair<Rule>) -> Result<QueryNode> {
    // typed_tag = ${ tag_type ~ ":" ~ label }
    let mut inner = pair.into_inner();
    let type_pair = inner
        .next()
        .ok_or_else(|| anyhow!(errors::MISSING_TAG_KEY))?;
    let tagtype = build_tag_type(type_pair)?;

    let label_pair = inner
        .next()
        .ok_or_else(|| anyhow!(errors::MISSING_TAG_LABEL))?;
    let label = build_label(label_pair)?;

    // Empty label implies projection (e.g. "extension:")
    if label.as_str().is_empty() {
        return Ok(QueryNode::Projection(tagtype));
    }

    Ok(QueryNode::TypedTag(TypedTag::new(tagtype, label)))
}

fn build_projection(pair: Pair<Rule>) -> Result<QueryNode> {
    // projection = { type_ref }
    // type_ref = ${ tag_type ~ ":" }
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| anyhow!(errors::MISSING_PROJECTION_INNER))?;
    let mut type_ref_inner = inner.into_inner();
    let type_pair = type_ref_inner
        .next()
        .ok_or_else(|| anyhow!(errors::MISSING_TAG_KEY_IN_PROJECTION))?;
    let tagtype = build_tag_type(type_pair)?;
    Ok(QueryNode::Projection(tagtype))
}

fn build_tag_type(pair: Pair<Rule>) -> Result<TagType> {
    // tag_type = { quoted_string | identifier }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::quoted_string => {
            // Remove outer quotes before unescaping
            let content = &inner.as_str()[1..inner.as_str().len() - 1];
            let s = unescape_string(content)?;
            TagType::LiteralCustom(s).to_ok()
        }
        Rule::identifier => {
            let s = unescape_unquoted(inner.as_str())?;
            TagType::from(s).to_ok()
        }
        _ => Err(anyhow!(
            "{}: {:?}",
            errors::UNEXPECTED_TAG_TYPE_RULE,
            inner.as_rule()
        )),
    }
}

fn build_comparison(pair: Pair<Rule>) -> Result<QueryNode> {
    // comparison = { operand ~ (cmp_op ~ operand)+ }
    // AST Node: ComparisonNode { first: Operand, rest: Vec<(ComparisonOp, Operand)> }
    let mut inner = pair.into_inner();
    let first_op = build_operand(inner.next().unwrap())?;
    let mut rest = Vec::new();

    while let Some(op_pair) = inner.next() {
        let op = match op_pair.as_str() {
            "==" => ComparisonOp::Eq,
            "^=" | "^" => ComparisonOp::Ne, // ^ and ^= are NotEqual
            ">" => ComparisonOp::Gt,
            ">=" => ComparisonOp::Ge,
            "<" => ComparisonOp::Lt,
            "<=" => ComparisonOp::Le,
            s => {
                return Err(anyhow!("{}: {}", errors::UNKNOWN_COMPARISON_OP, s))
            }
        };
        let right_pair = inner
            .next()
            .ok_or_else(|| anyhow!(errors::MISSING_COMPARISON_OPERAND))?;
        let right_op = build_operand(right_pair)?;
        rest.push((op, right_op));
    }
    Ok(QueryNode::Comparison(ComparisonNode {
        first: first_op,
        rest,
    }))
}

fn build_operand(pair: Pair<Rule>) -> Result<Operand> {
    // operand = { calculation | type_ref | label }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::calculation => {
            Ok(Operand::Calculation(Box::new(build_calculation(inner)?)))
        }
        Rule::type_ref => {
            // type_ref = ${ tag_type ~ ":" }
            let inner_tag = inner.into_inner().next().unwrap();
            Ok(Operand::TypeRef(build_tag_type(inner_tag)?))
        }
        Rule::label => Ok(Operand::Literal(build_label(inner)?)),
        _ => Err(anyhow!(
            "{}: {:?}",
            errors::UNKNOWN_OPERAND_RULE,
            inner.as_rule()
        )),
    }
}

fn build_calculation(pair: Pair<Rule>) -> Result<CalculationNode> {
    let inner_pair = pair.into_inner().next().unwrap();
    let mut pairs = inner_pair.into_inner();

    let first_pair = pairs.next().unwrap();
    let mut left = build_operand_calc(first_pair)?;

    while let Some(op_pair) = pairs.next() {
        let op = parse_arithmetic_op(op_pair.as_str())?;
        let right_pair = pairs.next().unwrap();
        let right = build_operand_calc(right_pair)?;

        // Build left-associative chain: (A + B) + C
        left =
            Operand::Calculation(Box::new(CalculationNode { left, op, right }));
    }

    match left {
        Operand::Calculation(node) => Ok(*node),
        _ => Err(anyhow!(errors::CALC_REQUIRES_OP)),
    }
}

fn parse_arithmetic_op(s: &str) -> Result<ArithmeticOp> {
    match s {
        "+" => Ok(ArithmeticOp::Add),
        "-" => Ok(ArithmeticOp::Sub),
        "*" => Ok(ArithmeticOp::Mul),
        "/" => Ok(ArithmeticOp::Div),
        "%" => Ok(ArithmeticOp::Mod),
        _ => Err(anyhow!(errors::UNKNOWN_ARITHMETIC_OP)),
    }
}

fn build_operand_calc(pair: Pair<Rule>) -> Result<Operand> {
    // operand_calc = { type_ref | label | calculation }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::type_ref => {
            let s = inner.as_str();
            let key = s.trim_end_matches(':');
            Ok(Operand::TypeRef(TagType::from(key)))
        }
        Rule::label => Ok(Operand::Literal(build_label(inner)?)),
        Rule::calculation => {
            Ok(Operand::Calculation(Box::new(build_calculation(inner)?)))
        }
        _ => Err(anyhow!(errors::UNKNOWN_OPERAND_CALC_RULE)),
    }
}

fn build_label(pair: Pair<Rule>) -> Result<Label> {
    // label = { quoted_string | number | unquoted_string }
    // tag_label = { quoted_string | number | unquoted_tag_string }
    // Both are handled by this single function.
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::quoted_string => {
            // Remove outer quotes before unescaping
            let content = &inner.as_str()[1..inner.as_str().len() - 1];
            let s = unescape_string(content)?;
            Ok(Label::Other(
                TagType::Custom(String::new()),
                crate::types::LabelValue::Literal(s),
            ))
        }
        Rule::number => {
            let i = inner.as_str().parse::<i64>()?;
            Ok(Label::from(i))
        }
        Rule::unquoted_string | Rule::unquoted_tag_string => {
            let s = unescape_unquoted(inner.as_str())?;
            Ok(Label::from(s))
        }
        _ => Err(anyhow!(
            "{}: {:?}",
            errors::UNKNOWN_LABEL_RULE,
            inner.as_rule()
        )),
    }
}

/// 引用符で囲まれた文字列内のエスケープシーケンスを展開します。
///
/// 標準的なエスケープシーケンス（\n, \r, \t, \\, \', \"）を処理します。
/// クエリ文法の `quoted_string` ルールで使用されます。
fn unescape_string(s: &str) -> Result<String> {
    let mut chars = s.chars();
    std::iter::from_fn(move || match chars.next()? {
        '\\' => match chars.next() {
            Some('n') => Some('\n'),
            Some('r') => Some('\r'),
            Some('t') => Some('\t'),
            Some('\\') => Some('\\'),
            Some('\'') => Some('\''),
            Some('"') => Some('"'),
            Some(c) => Some(c),
            None => Some('\\'),
        },
        c => Some(c),
    })
    .collect::<String>()
    .to_ok()
}

/// 引用符なし文字列のエスケープシーケンスを展開します。
///
/// DuckDB の GLOB パターンで特殊な意味を持つ文字（*, ?, [, ], !）を
/// エスケープするため、バックスラッシュ付きの文字を `[char]` 形式に変換します。
/// 例: `\*` → `[*]`（リテラルのアスタリスク）
///
/// クエリ文法の `unquoted_string` および `unquoted_tag_string` ルールで使用されます。
fn unescape_unquoted(s: &str) -> Result<String> {
    let mut chars = s.chars();
    let mut pending = VecDeque::new();

    std::iter::from_fn(move || {
        if let Some(c) = pending.pop_front() {
            return Some(c);
        }
        match chars.next()? {
            '\\' => match chars.next() {
                // DuckDB GLOB escape: \* -> [*]
                Some(c @ ('*' | '?' | '[' | ']' | '!')) => {
                    pending.push_back(c);
                    pending.push_back(']');
                    Some('[')
                }
                Some(c) => Some(c),
                None => Some('\\'),
            },
            c => Some(c),
        }
    })
    .collect::<String>()
    .to_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::{ComparisonOp, QueryNode};

    #[test]
    fn test_unescape_string_basic() {
        assert_eq!(unescape_string("foo").unwrap(), "foo");
        assert_eq!(unescape_string("foo bar").unwrap(), "foo bar");
    }

    #[test]
    fn test_unescape_string_escapes() {
        assert_eq!(unescape_string(r"foo\nbar").unwrap(), "foo\nbar");
        assert_eq!(unescape_string(r"foo\rbar").unwrap(), "foo\rbar");
        assert_eq!(unescape_string(r"foo\tbar").unwrap(), "foo\tbar");
        assert_eq!(unescape_string(r"foo\\bar").unwrap(), "foo\\bar");
        assert_eq!(unescape_string(r#"foo\"bar"#).unwrap(), "foo\"bar");
        assert_eq!(unescape_string(r"foo\'bar").unwrap(), "foo'bar");
    }

    #[test]
    fn test_unescape_unquoted_basic() {
        assert_eq!(unescape_unquoted("foo").unwrap(), "foo");
        assert_eq!(unescape_unquoted("foo.txt").unwrap(), "foo.txt");
    }

    #[test]
    fn test_unescape_unquoted_special_chars() {
        // Glob patterns handling: \* -> [*]
        assert_eq!(unescape_unquoted(r"foo\*bar").unwrap(), "foo[*]bar");
        assert_eq!(unescape_unquoted(r"foo\?bar").unwrap(), "foo[?]bar");
        assert_eq!(unescape_unquoted(r"foo\[bar").unwrap(), "foo[[]bar");
        assert_eq!(unescape_unquoted(r"foo\]bar").unwrap(), "foo[]]bar");
        // ! is NOT escaped by simple logic mostly, but let's check input
        // Standard globs: [!] is one thing.
        // implementation details in unescape_unquoted:
        // "DuckDB GLOB pattern ... characters (*, ?, [, ], !) ... escape with [char]"
        assert_eq!(unescape_unquoted(r"foo\!bar").unwrap(), "foo[!]bar");
    }

    #[test]
    fn test_parse_arithmetic_op_basic() {
        assert_eq!(parse_arithmetic_op("+").unwrap(), ArithmeticOp::Add);
        assert_eq!(parse_arithmetic_op("-").unwrap(), ArithmeticOp::Sub);
        assert_eq!(parse_arithmetic_op("*").unwrap(), ArithmeticOp::Mul);
        assert_eq!(parse_arithmetic_op("/").unwrap(), ArithmeticOp::Div);
        assert_eq!(parse_arithmetic_op("%").unwrap(), ArithmeticOp::Mod);
        assert!(parse_arithmetic_op("&").is_err());
    }

    #[test]
    fn test_parse_typed_tag() {
        let node = parse("name:test.txt").expect("Failed to parse typed tag");
        match node {
            QueryNode::TypedTag(tt) => {
                assert_eq!(tt.label.tag_type().as_str(), "name");
                assert_eq!(tt.label.as_str(), "test.txt");
            }
            _ => panic!("Expected TypedTag, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_comparison_simple() {
        let node = parse("size > 100").expect("Failed to parse comparison");
        match node {
            QueryNode::Comparison(cmp) => {
                // first should be 'size' (Operand::TypeRef)
                // op should be Gt
                // operand should be '100' (Operand::Literal)
                // first should be 'size' (Operand::Literal) - conversion happens in expand phase
                match cmp.first {
                    Operand::Literal(ref l) => assert_eq!(l.as_str(), "size"),
                    _ => panic!(
                        "Expected Literal for first operand, got {:?}",
                        cmp.first
                    ),
                }
                assert_eq!(cmp.rest.len(), 1);
                assert_eq!(cmp.rest[0].0, ComparisonOp::Gt);
                match &cmp.rest[0].1 {
                    Operand::Literal(l) => assert_eq!(l.as_str(), "100"),
                    _ => panic!("Expected Literal for second operand"),
                }
            }
            _ => panic!("Expected Comparison, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_logical_ops() {
        // AND
        let node = parse("a:1 & b:2").expect("Failed to parse AND");
        match node {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
            }
            _ => panic!("Expected And, got {:?}", node),
        }

        // Parentheses
        let node = parse("(a:1 | b:2)").expect("Failed to parse parentheses");
        match node {
            QueryNode::Or(nodes) => assert_eq!(nodes.len(), 2),
            _ => panic!("Expected Or, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_origin_projection() {
        let node = parse("origin:").expect("Failed to parse origin:");
        match node {
            QueryNode::Projection(tt) => assert_eq!(tt.as_str(), "origin"),
            _ => panic!("Expected Projection(origin), got {:?}", node),
        }
    }
}
