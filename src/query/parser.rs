use crate::query::ast::{
    AggregationNode, ArithmeticAggOp, ArithmeticOp, BasicOp, CalculationNode,
    ComparisonNode, ComparisonOp, Operand, QueryNode,
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
    pub const MISSING_TAG_KEY: &str = "Missing tag key";
    pub const MISSING_TAG_LABEL: &str = "Missing tag label";
    pub const MISSING_PROJECTION_INNER: &str = "Missing projection inner";
    pub const MISSING_TAG_KEY_IN_PROJECTION: &str =
        "Missing tag key in projection";
    pub const UNEXPECTED_TAG_TYPE_RULE: &str = "Unexpected tag_type rule";
    pub const UNKNOWN_COMPARISON_OP: &str = "Unknown comparison op";
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
                | Op::infix(Rule::minus_colon, Assoc::Left)
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
        Rule::comparison => build_comparison(pair),
        Rule::typed_tag => build_typed_tag(pair),
        Rule::projection => build_projection(pair),
        Rule::aggregation => {
            build_aggregation(pair).map(QueryNode::Aggregation)
        }
        Rule::expr => build_expr(pair),
        Rule::primary => build_primary(pair), // Keep primary for now, it delegates to build_ast
        Rule::factor => build_factor(pair), // Keep factor for now, it delegates to build_ast
        Rule::complement => build_complement(pair), // Keep complement for now
        _ => Err(anyhow!(errors::UNKNOWN_FACTOR_INNER)),
    }
}

fn build_aggregation(pair: Pair<Rule>) -> Result<AggregationNode> {
    let mut inner = pair.into_inner();
    let op_pair = inner.next().ok_or_else(|| anyhow!("Missing aggregator"))?;
    let expr_pair =
        inner.next().ok_or_else(|| anyhow!("Missing expression"))?;

    let node = build_expr(expr_pair)?;

    match op_pair.as_str() {
        "count" => Ok(AggregationNode::Count(Box::new(node))),
        "sum" => Ok(AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Sum,
            inner: Box::new(node),
        }),
        "avg" => Ok(AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Avg,
            inner: Box::new(node),
        }),
        "max" => Ok(AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Max,
            inner: Box::new(node),
        }),
        "min" => Ok(AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Min,
            inner: Box::new(node),
        }),
        _ => Err(anyhow!("Unknown aggregator: {}", op_pair.as_str())),
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
                Rule::minus | Rule::minus_colon => {
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
        Rule::aggregation => {
            build_aggregation(inner).map(QueryNode::Aggregation)
        }
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
    // comparison = { label_comparison | scalar_comparison }
    let inner = pair.into_inner().next().unwrap();
    let rule = inner.as_rule();
    let mut inner_pairs = inner.into_inner();

    let first_op = build_operand(inner_pairs.next().unwrap())?;
    let mut rest = Vec::new();

    while let Some(step_pair) = inner_pairs.next() {
        // Scalar comparison consumes WHITESPACE+ tokens here (they are pairs in ${})
        // If it's whitespace, get the next real token
        let actual_step = if step_pair.as_rule() == Rule::WHITESPACE {
            inner_pairs.next().unwrap()
        } else {
            step_pair
        };

        let (op, right_op) = match rule {
            Rule::label_comparison => {
                let step_rule = actual_step.as_rule();
                let mut step_inner = actual_step.into_inner();
                let op_str = if step_rule == Rule::label_op {
                    let _colon = step_inner.next(); // consume the colon pair
                    step_inner.next().unwrap().as_str() // label_basic_op
                } else {
                    step_inner.next().unwrap().as_str() // stuck_basic_op
                };
                let basic_op = parse_label_basic_op(op_str)?;
                let right_op = build_operand(step_inner.next().unwrap())?;
                (ComparisonOp::Label(basic_op), right_op)
            }
            Rule::scalar_comparison => {
                let basic_op_str = actual_step.as_str();
                let basic_op = parse_scalar_basic_op(basic_op_str)?;
                let right_op = build_operand(inner_pairs.next().unwrap())?;
                (ComparisonOp::Scalar(basic_op), right_op)
            }
            _ => unreachable!(),
        };
        rest.push((op, right_op));
    }

    Ok(QueryNode::Comparison(ComparisonNode {
        first: first_op,
        rest,
    }))
}

/// Label comparison uses "=" for equality (DESIGN.md:71)
fn parse_label_basic_op(s: &str) -> Result<BasicOp> {
    match s {
        "=" => Ok(BasicOp::Eq),
        "^=" | "^" => Ok(BasicOp::Ne),
        ">" => Ok(BasicOp::Gt),
        ">=" => Ok(BasicOp::Ge),
        "<" => Ok(BasicOp::Lt),
        "<=" => Ok(BasicOp::Le),
        s => Err(anyhow!("{}: {}", errors::UNKNOWN_COMPARISON_OP, s)),
    }
}

/// Scalar comparison uses "==" for equality
fn parse_scalar_basic_op(s: &str) -> Result<BasicOp> {
    match s {
        "==" => Ok(BasicOp::Eq),
        "^=" | "^" | "!=" => Ok(BasicOp::Ne),
        ">" => Ok(BasicOp::Gt),
        ">=" => Ok(BasicOp::Ge),
        "<" => Ok(BasicOp::Lt),
        "<=" => Ok(BasicOp::Le),
        s => Err(anyhow!("{}: {}", errors::UNKNOWN_COMPARISON_OP, s)),
    }
}

fn build_operand(pair: Pair<Rule>) -> Result<Operand> {
    let rule = pair.as_rule();
    let inner = if rule == Rule::operand
        || rule == Rule::scalar_operand
        || rule == Rule::stuck_operand
    {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::typed_tag => {
            let tn = build_typed_tag(inner)?;
            if let QueryNode::TypedTag(tt) = tn {
                Ok(Operand::Literal(tt.label))
            } else {
                unreachable!()
            }
        }
        Rule::calculation | Rule::calculation_inner => {
            Ok(Operand::Calculation(Box::new(build_calculation(inner)?)))
        }
        Rule::type_ref => {
            let inner_tag = inner.into_inner().next().unwrap();
            Ok(Operand::TypeRef(build_tag_type(inner_tag)?))
        }
        Rule::label
        | Rule::number
        | Rule::quoted_string
        | Rule::unquoted_string
        | Rule::unquoted_tag_string => {
            Ok(Operand::Literal(build_label(inner)?))
        }
        Rule::aggregation => {
            Ok(Operand::Aggregation(Box::new(build_aggregation(inner)?)))
        }
        _ => Err(anyhow!(
            "{}: {:?}",
            errors::UNKNOWN_OPERAND_RULE,
            inner.as_rule()
        )),
    }
}

fn build_calculation(pair: Pair<Rule>) -> Result<CalculationNode> {
    let rule = pair.as_rule();
    let inner_pair = if rule == Rule::calculation {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };
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
    match s.trim() {
        "+" => Ok(ArithmeticOp::Add),
        "-" => Ok(ArithmeticOp::Sub),
        "*" => Ok(ArithmeticOp::Mul),
        "x" => Ok(ArithmeticOp::Mul), // 'x' is also allowed for multiplication
        "/" => Ok(ArithmeticOp::Div),
        "%" => Ok(ArithmeticOp::Mod),
        _ => Err(anyhow!(errors::UNKNOWN_ARITHMETIC_OP)),
    }
}

fn build_operand_calc(pair: Pair<Rule>) -> Result<Operand> {
    let rule = pair.as_rule();
    let inner = if rule == Rule::operand_calc || rule == Rule::scalar_operand {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::aggregation => {
            Ok(Operand::Aggregation(Box::new(build_aggregation(inner)?)))
        }
        Rule::type_ref => {
            let inner_tag = inner.into_inner().next().unwrap();
            Ok(Operand::TypeRef(build_tag_type(inner_tag)?))
        }
        Rule::label
        | Rule::number
        | Rule::quoted_string
        | Rule::unquoted_string
        | Rule::unquoted_tag_string => {
            Ok(Operand::Literal(build_label(inner)?))
        }
        Rule::calculation | Rule::calculation_inner => {
            Ok(Operand::Calculation(Box::new(build_calculation(inner)?)))
        }
        _ => Err(anyhow!(
            "{}: {:?}",
            errors::UNKNOWN_OPERAND_CALC_RULE,
            inner.as_rule()
        )),
    }
}

fn unescape_glob_string(s: &str) -> Result<String> {
    let mut result = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next_char) = chars.peek() {
                // Keep backslash if it escapes a glob special char
                if matches!(next_char, '*' | '?' | '[' | ']') {
                    result.push('\\');
                    result.push(chars.next().unwrap());
                } else {
                    // Otherwise consumes backslash (unescape)
                    result.push(chars.next().unwrap());
                }
            } else {
                // Trailing backslash
                result.push('\\');
            }
        } else {
            result.push(c);
        }
    }
    Ok(result)
}

fn build_label(pair: Pair<Rule>) -> Result<Label> {
    // label = { quoted_string | number | unquoted_string }
    // tag_label = { quoted_string | number | unquoted_tag_string }
    // Both are handled by this single function.
    let rule = pair.as_rule();
    let inner = if rule == Rule::label || rule == Rule::tag_label {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };

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
            let s = unescape_glob_string(inner.as_str())?;
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
        // Using colon-operator format for explicit Label comparison
        let node = parse("size:>100").expect("Failed to parse comparison");
        match node {
            QueryNode::Comparison(cmp) => {
                // first should be 'size' (Operand::TypeRef) because 'size:' is parsed as TypeRef
                // and followed by stuck operator '>100'
                if let Operand::TypeRef(ref t) = cmp.first {
                    assert_eq!(t.as_str(), "size");
                } else {
                    panic!(
                        "Expected TypeRef for first operand, got {:?}",
                        cmp.first
                    );
                }
                assert_eq!(cmp.rest.len(), 1);
                assert_eq!(
                    cmp.rest[0].0,
                    ComparisonOp::Label(crate::query::ast::BasicOp::Gt)
                );
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

    #[test]
    fn test_parse_count_items() {
        let node = parse("count(name:*.rs)")
            .expect("Failed to parse count(name:*.rs)");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Count(inner) => match &*inner {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.label.tag_type().as_str(), "name");
                        assert_eq!(tt.label.as_str(), "*.rs");
                    }
                    _ => panic!(
                        "Expected TypedTag inside count, got {:?}",
                        inner
                    ),
                },
                _ => panic!("Expected Count aggregation"),
            },
            _ => panic!("Expected Aggregation, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_count_projection() {
        let node = parse("count(extension:)")
            .expect("Failed to parse count(extension:)");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Count(inner) => match &*inner {
                    QueryNode::Projection(tt) => {
                        assert_eq!(tt.as_str(), "extension");
                    }
                    _ => panic!(
                        "Expected Projection inside count, got {:?}",
                        inner
                    ),
                },
                _ => panic!("Expected Count aggregation"),
            },
            _ => panic!("Expected Aggregation, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_sum_projection() {
        let node = parse("sum(size:)").expect("Failed to parse sum(size:)");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    match &**inner {
                        QueryNode::Projection(tt) => {
                            assert_eq!(tt.as_str(), "size");
                        }
                        _ => panic!(
                            "Expected Projection inside sum, got {:?}",
                            inner
                        ),
                    }
                }
                _ => panic!("Expected Arithmetic aggregation, got {:?}", agg),
            },
            _ => panic!("Expected Aggregation, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_aggregation_comparison() {
        let node =
            parse("sum(size:) > 100").expect("Failed to parse comparison");
        match node {
            QueryNode::Comparison(cmp) => match &cmp.first {
                Operand::Aggregation(agg) => match &**agg {
                    AggregationNode::Arithmetic { op, .. } => {
                        assert_eq!(*op, ArithmeticAggOp::Sum);
                    }
                    _ => panic!("Expected Arithmetic agg"),
                },
                _ => {
                    panic!("Expected Aggregation operand, got {:?}", cmp.first)
                }
            },
            _ => panic!("Expected Comparison, got {:?}", node),
        }
    }
}
