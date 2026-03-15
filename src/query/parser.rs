use crate::query::ast::{
    AggregationNode, ArithmeticAggOp, ArithmeticOp, BasicOp, CalculationNode,
    ComparisonNode, ComparisonOp, NestNode, Operand, QueryNode,
};
use crate::query::error;
use crate::types::{Label, TagType, TypedTag};
use crate::util::DotOk;
use anyhow::{anyhow, Result};
use pest::error::{Error, ErrorVariant};
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
// Error definitions are now in crate::query::error

fn get_parser() -> &'static PrattParser<Rule> {
    PRATT_PARSER.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::pipe, Assoc::Left)
                | Op::infix(Rule::minus_colon, Assoc::Left)
                | Op::infix(Rule::minus, Assoc::Left))
            .op(Op::infix(Rule::ampersand, Assoc::Left))
            .op(Op::infix(Rule::ampersand_colon, Assoc::Left))
    })
}

/// クエリ文字列を解析し、QueryNode AST を構築します。
pub fn parse(input: &str) -> Result<QueryNode> {
    let mut pairs = PestQueryParser::parse(Rule::query, input)
        .map_err(|e| error::map_grammar_error(input, e))?;

    // query = { SOI ~ (choice) ~ EOI }
    // The top-level pair is 'query'.
    let query_pair = pairs.next().unwrap();

    // Into inner: expected to contain SOI(implicit?), choice, EOI
    // Pest 2.0+ usually doesn't yield SOI/EOI in into_inner unless explicit?
    // Actually query definition: query = { SOI ~ ... }
    // inner of query will be the choice match.
    // Let's get the first inner pair that is NOT SOI/EOI if they appear.
    // In many patterns, `query` inner is just the content.
    // Let's find the relevant inner pair.

    let inner_pair = query_pair.into_inner().next().unwrap();

    match inner_pair.as_rule() {
        Rule::expr => build_expr(inner_pair),
        Rule::scalar_arithmetic_query => {
            let inner = inner_pair.into_inner().next().unwrap();
            let calc = build_scalar_arithmetic_expr(inner)?;
            Ok(QueryNode::Projection(Operand::Calculation(Box::new(calc))))
        }
        Rule::EOI => Err(anyhow!("Empty query")),
        _ => Err(anyhow!(
            "Unexpected top-level rule: {:?}",
            inner_pair.as_rule()
        )),
    }
}

fn build_ast(pair: Pair<Rule>) -> Result<QueryNode> {
    match pair.as_rule() {
        Rule::scalar_arithmetic_query => {
            let inner = pair.into_inner().next().unwrap();
            let calc = build_scalar_arithmetic_expr(inner)?;
            Ok(QueryNode::Projection(Operand::Calculation(Box::new(calc))))
        }
        Rule::comparison => build_comparison(pair),
        Rule::typed_tag => build_typed_tag(pair),
        Rule::projection => build_projection(pair),
        Rule::aggregation => {
            build_aggregation(pair).map(QueryNode::Aggregation)
        }
        Rule::nest_expr => build_nest_expr(pair),
        Rule::expr => build_expr(pair),
        Rule::primary => build_primary(pair),
        Rule::factor => build_factor(pair),
        // complement was removed from grammar; // No changes needed for parser.rs as logic is generic enough.
        // Complement nodes are still generated internally by functions.rs (date Ne expansion).
        _ => Err(anyhow!(error::UNKNOWN_FACTOR_INNER)),
    }
}

fn build_aggregation(pair: Pair<Rule>) -> Result<AggregationNode> {
    let mut inner = pair.into_inner();
    let op_pair = inner.next().ok_or_else(|| anyhow!("Missing aggregator"))?;
    let op_str = op_pair.as_str();
    // 引数（body_pair）をオプションとして受け取る
    let body_pair = inner.next();

    let node = if let Some(body_pair) = body_pair {
        match body_pair.as_rule() {
            Rule::bare_calculation => {
                let calc_inner = body_pair.into_inner().next().unwrap();
                let calc = build_calculation(calc_inner)?;
                QueryNode::Projection(Operand::Calculation(Box::new(calc)))
            }
            Rule::expr => build_expr(body_pair)?,
            _ => {
                return Err(anyhow!(
                    "Unexpected rule in aggregation: {:?}",
                    body_pair.as_rule()
                ))
            }
        }
    } else {
        // 引数が空の場合のバリデーションとデフォルト値設定
        match op_str {
            "count" => {
                // count() は count(*:*) の短縮形として、ワイルドカードタグを生成する
                QueryNode::TypedTag(TypedTag::new(
                    TagType::from("*"),
                    Label::from("*"),
                ))
            }
            _ => {
                // sum(), avg() 等は引数が必須
                return Err(error::aggregator_requires_argument(op_str));
            }
        }
    };

    match op_str {
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
        _ => Err(anyhow!("Unknown aggregator: {}", op_str)),
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
                Rule::ampersand_colon => Ok(QueryNode::Nest(NestNode {
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                })),
                _ => Err(anyhow!(
                    "{}: {:?}",
                    error::UNKNOWN_INFIX_RULE,
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
        Rule::nest_expr => build_nest_expr(inner),
        Rule::projection => build_projection(inner),
        Rule::aggregation => {
            build_aggregation(inner).map(QueryNode::Aggregation)
        }
        Rule::label => {
            let label = build_label(inner)?;
            Ok(QueryNode::Projection(Operand::Literal(label)))
        }
        _ => Err(anyhow!(
            "{}: {:?}",
            error::UNKNOWN_FACTOR_INNER,
            inner.as_rule()
        )),
    }
}

fn build_typed_tag(pair: Pair<Rule>) -> Result<QueryNode> {
    // typed_tag = ${ tag_type ~ ":" ~ label }
    let mut inner = pair.into_inner();
    let type_pair = inner
        .next()
        .ok_or_else(|| anyhow!(error::MISSING_TAG_KEY))?;
    let tagtype = build_tag_type(type_pair)?;

    let label_pair = inner
        .next()
        .ok_or_else(|| anyhow!(error::MISSING_TAG_LABEL))?;
    let label = build_label(label_pair)?;

    // Empty label implies projection (e.g. "extension:")
    if label.as_str().is_empty() {
        return Ok(QueryNode::Projection(Operand::from(tagtype)));
    }

    Ok(QueryNode::TypedTag(TypedTag::new(tagtype, label)))
}

fn build_projection(pair: Pair<Rule>) -> Result<QueryNode> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::type_ref => {
            let tag_type = TagType::from(inner.as_str().trim_end_matches(':'));
            Ok(QueryNode::Projection(Operand::TypeRef(tag_type)))
        }
        Rule::calculation => {
            let calc = build_calculation(inner)?;
            Ok(QueryNode::Projection(Operand::Calculation(Box::new(calc))))
        }
        _ => Err(anyhow!(
            "Unexpected rule in projection: {:?}",
            inner.as_rule()
        )),
    }
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
            error::UNEXPECTED_TAG_TYPE_RULE,
            inner.as_rule()
        )),
    }
}

fn build_comparison(pair: Pair<Rule>) -> Result<QueryNode> {
    // comparison = { label_comparison | scalar_comparison }
    let inner = pair.into_inner().next().unwrap();
    build_comparison_inner(inner)
}

/// scalar_comparison を nest_operand 内で直接処理するためのヘルパー。
fn build_comparison_from_scalar(pair: Pair<Rule>) -> Result<QueryNode> {
    build_comparison_inner(pair)
}

/// label_comparison または scalar_comparison のペアを受け取り Comparison ノードを構築します。
fn build_comparison_inner(inner: Pair<Rule>) -> Result<QueryNode> {
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
                let op_str = if step_rule == Rule::label_op
                    || step_rule == Rule::label_op_to_proj
                {
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
                // 不適切なスカラー演算のチェック (Example: size: > 100)
                // Post-processing error check has been moved to error::map_grammar_error

                let basic_op_str = actual_step.as_str();
                let basic_op = parse_scalar_basic_op(basic_op_str)?;
                let right_op = build_operand(inner_pairs.next().unwrap())?;

                if let Operand::TypeRef(tt) = &right_op {
                    let op_span = actual_step.as_span();
                    let op_str = op_span.as_str();
                    let message = format!(
                        "Invalid operator '{}': Scalar comparison cannot be applied to a Projection ('{}:') on the right side.",
                        op_str, tt.as_str()
                    );
                    // Use the operator span if possible, or just return a general error.
                    // Since specific span tracking for right_op isn't set up, we use basic error.
                    // But we can use actual_step.as_span() which is the operator.
                    return Err(anyhow!(Error::<Rule>::new_from_span(
                        ErrorVariant::CustomError { message },
                        actual_step.as_span(),
                    )));
                }

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
        s => Err(anyhow!("{}: {}", error::UNKNOWN_COMPARISON_OP, s)),
    }
}

/// Scalar comparison uses "==" for equality
fn parse_scalar_basic_op(s: &str) -> Result<BasicOp> {
    match s {
        "==" => Ok(BasicOp::Eq),
        "^=" | "^" => Ok(BasicOp::Ne),
        ">" => Ok(BasicOp::Gt),
        ">=" => Ok(BasicOp::Ge),
        "<" => Ok(BasicOp::Lt),
        "<=" => Ok(BasicOp::Le),
        s => Err(anyhow!("{}: {}", error::UNKNOWN_COMPARISON_OP, s)),
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
        Rule::calculation
        | Rule::calculation_inner
        | Rule::scalar_calculation
        | Rule::scalar_calculation_inner => {
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
        Rule::parenthesized_expr => {
            let expr_pair = inner.into_inner().next().unwrap();
            let node = build_expr(expr_pair)?;
            Ok(Operand::Query(Box::new(node)))
        }
        Rule::nest_expr => {
            let node = build_nest_expr(inner)?;
            Ok(Operand::Query(Box::new(node)))
        }
        _ => Err(anyhow!(
            "{}: {:?}",
            error::UNKNOWN_OPERAND_RULE,
            inner.as_rule()
        )),
    }
}

/// `nest_expr = ${ nest_operand ~ (WHITESPACE+ ~ ampersand_colon ~ WHITESPACE+ ~ nest_operand)+ }`
/// を左結合で Nest ノードにビルドします。
fn build_nest_expr(pair: Pair<Rule>) -> Result<QueryNode> {
    let mut inner = pair.into_inner();

    let first = inner.next().unwrap();
    let mut left = build_nest_operand(first)?;

    // inner yields alternating: ampersand_colon, nest_operand, ampersand_colon, nest_operand, ...
    // (WHITESPACE is silent _{ } so it doesn't produce tokens)
    while let Some(op_pair) = inner.next() {
        // op_pair should be ampersand_colon
        debug_assert_eq!(op_pair.as_rule(), Rule::ampersand_colon);
        let right_pair = inner.next().unwrap();
        let right = build_nest_operand(right_pair)?;
        left = QueryNode::Nest(NestNode {
            left: Box::new(left),
            right: Box::new(right),
        });
    }
    Ok(left)
}

/// `nest_operand = { scalar_comparison | aggregation | projection | "(" ~ expr ~ ")" | label }`
fn build_nest_operand(pair: Pair<Rule>) -> Result<QueryNode> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::scalar_comparison => build_comparison_from_scalar(inner),
        Rule::aggregation => {
            build_aggregation(inner).map(QueryNode::Aggregation)
        }
        Rule::projection => build_projection(inner),
        Rule::expr => build_expr(inner),
        Rule::label => {
            let label = build_label(inner)?;
            Ok(QueryNode::Projection(Operand::Literal(label)))
        }
        _ => Err(anyhow!(
            "Unexpected rule in nest_operand: {:?}",
            inner.as_rule()
        )),
    }
}

fn build_calculation(pair: Pair<Rule>) -> Result<CalculationNode> {
    let rule = pair.as_rule();
    let inner_pair =
        if rule == Rule::calculation || rule == Rule::scalar_calculation {
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
        _ => Err(anyhow!(error::CALC_REQUIRES_OP)),
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
        _ => Err(anyhow!(error::UNKNOWN_ARITHMETIC_OP)),
    }
}

fn build_operand_calc(pair: Pair<Rule>) -> Result<Operand> {
    let rule = pair.as_rule();
    let inner = if rule == Rule::operand_calc
        || rule == Rule::scalar_operand
        || rule == Rule::scalar_operand_calc
    {
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
        Rule::calculation
        | Rule::calculation_inner
        | Rule::scalar_calculation
        | Rule::scalar_calculation_inner => {
            Ok(Operand::Calculation(Box::new(build_calculation(inner)?)))
        }
        Rule::parenthesized_expr => {
            // 括弧で囲まれた式: (is_dir:false & size:)
            let expr_pair = inner.into_inner().next().unwrap();
            let node = build_expr(expr_pair)?;
            Ok(Operand::Query(Box::new(node)))
        }

        _ => Err(anyhow!(
            "{}: {:?}",
            error::UNKNOWN_OPERAND_CALC_RULE,
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
            error::UNKNOWN_LABEL_RULE,
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

/// 文法上は許容されたスカラー比較が、意味的に不正（プロジェクションへの適用）でないかチェックします。
// check_invalid_scalar_comparison removed (moved to error.rs)

fn build_scalar_arithmetic_expr(pair: Pair<Rule>) -> Result<CalculationNode> {
    let mut inner = pair.into_inner();
    let first = inner.next().unwrap();
    let mut lhs_operand = build_arithmetic_operand(first)?;

    while let Some(op_pair) = inner.next() {
        let op_str = op_pair.as_str().trim();

        let op = match op_str {
            "+" => ArithmeticOp::Add,
            "-" => ArithmeticOp::Sub,
            "*" => ArithmeticOp::Mul,
            "/" => ArithmeticOp::Div,
            "%" => ArithmeticOp::Mod,
            _ => {
                return Err(anyhow!(
                    "Expected arithmetic operator (+, -, *, /, %), found '{}'",
                    op_str
                ))
            }
        };

        let rhs_pair = inner.next().unwrap();
        let rhs_operand = build_arithmetic_operand(rhs_pair)?;

        let lhs_node = CalculationNode {
            left: lhs_operand,
            op,
            right: rhs_operand,
        };

        lhs_operand = Operand::Calculation(Box::new(lhs_node));
    }

    match lhs_operand {
        Operand::Calculation(boxed_node) => Ok(*boxed_node),
        _ => Err(anyhow!(
            "Failed to build calculation node (single operand?)"
        )),
    }
}

fn build_arithmetic_operand(pair: Pair<Rule>) -> Result<Operand> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::aggregation => {
            let agg = build_aggregation(inner)?;
            Ok(Operand::Aggregation(Box::new(agg)))
        }
        Rule::number => {
            let s = inner.as_str();
            Ok(Operand::Literal(Label::from(s)))
        }
        Rule::calculation => {
            let calc_inner = inner.into_inner().next().unwrap();
            let calc = build_calculation(calc_inner)?;
            Ok(Operand::Calculation(Box::new(calc)))
        }
        Rule::type_ref => {
            let mut type_inner = inner.into_inner();
            let tag_type_pair = type_inner.next().unwrap();
            let tag_type = TagType::from(tag_type_pair.as_str());
            Ok(Operand::TypeRef(tag_type))
        }
        Rule::parenthesized_expr => {
            let expr_pair = inner.into_inner().next().unwrap();
            let node = build_expr(expr_pair)?;
            Ok(Operand::Query(Box::new(node)))
        }
        Rule::label | Rule::quoted_string | Rule::unquoted_string => {
            Ok(Operand::Literal(build_label(inner)?))
        }
        _ => Err(anyhow!(
            "Unexpected arithmetic operand inner: {:?}",
            inner.as_rule()
        )),
    }
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
            QueryNode::Projection(op) => match op {
                Operand::TypeRef(tt) => assert_eq!(tt.as_str(), "origin"),
                _ => panic!("Expected TypeRef operand, got {:?}", op),
            },
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
                    QueryNode::Projection(op) => match op {
                        Operand::TypeRef(tt) => {
                            assert_eq!(tt.as_str(), "extension");
                        }
                        _ => panic!("Expected TypeRef operand, got {:?}", op),
                    },
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
                        QueryNode::Projection(op) => match op {
                            Operand::TypeRef(tt) => {
                                assert_eq!(tt.as_str(), "size");
                            }
                            _ => {
                                panic!("Expected TypeRef operand, got {:?}", op)
                            }
                        },
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

    #[test]
    fn test_mismatched_comparison_error() {
        // size: > 100 (本来は :> であるべき)
        let result = parse("size: > 100");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();

        // 期待される詳細なエラーメッセージが含まれているか確認 (Red)
        assert!(
            err_msg.contains("Invalid operator '>'"),
            "Error message should point out the invalid operator. Got: {}",
            err_msg
        );
        assert!(err_msg.contains("Did you mean: 'size: :> 100'"));
    }

    // ========== bare_calculation 単体テスト ==========

    /// bare_calculation: sum(size: - 100) →
    /// Arithmetic { Sum, Projection(Calculation { TypeRef(size), Sub, Literal(100) }) }
    #[test]
    fn test_parse_bare_calc_sum() {
        let node =
            parse("sum(size: - 100)").expect("bare_calc sum should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    match &**inner {
                        QueryNode::Projection(operand) => match operand {
                            Operand::Calculation(calc) => {
                                // left = TypeRef(size)
                                match &calc.left {
                                    Operand::TypeRef(tt) => {
                                        assert_eq!(tt.as_str(), "size");
                                    }
                                    _ => panic!(
                                        "Expected TypeRef(size) as left, got {:?}",
                                        calc.left
                                    ),
                                }
                                // op = Sub
                                assert_eq!(calc.op, ArithmeticOp::Sub);
                                // right = Literal(100)
                                match &calc.right {
                                    Operand::Literal(l) => {
                                        assert_eq!(l.as_str(), "100");
                                    }
                                    _ => panic!(
                                        "Expected Literal(100) as right, got {:?}",
                                        calc.right
                                    ),
                                }
                            }
                            _ => panic!(
                                "Expected Calculation operand, got {:?}",
                                operand
                            ),
                        },
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

    /// bare_calculation: sum(size: + 100 - 50) → 左結合 Calculation チェーン
    /// ((size + 100) - 50)
    #[test]
    fn test_parse_bare_calc_multiop() {
        let node = parse("sum(size: + 100 - 50)")
            .expect("bare_calc multiop should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    match &**inner {
                        QueryNode::Projection(operand) => match operand {
                            Operand::Calculation(calc) => {
                                // 外側の演算子は Sub
                                assert_eq!(calc.op, ArithmeticOp::Sub);
                                // right = Literal(50)
                                match &calc.right {
                                    Operand::Literal(l) => {
                                        assert_eq!(l.as_str(), "50");
                                    }
                                    _ => panic!(
                                        "Expected Literal(50) as right, got {:?}",
                                        calc.right
                                    ),
                                }
                                // left = Calculation(size + 100)
                                match &calc.left {
                                    Operand::Calculation(inner_calc) => {
                                        assert_eq!(inner_calc.op, ArithmeticOp::Add);
                                        match &inner_calc.left {
                                            Operand::TypeRef(tt) => {
                                                assert_eq!(tt.as_str(), "size");
                                            }
                                            _ => panic!(
                                                "Expected TypeRef(size), got {:?}",
                                                inner_calc.left
                                            ),
                                        }
                                        match &inner_calc.right {
                                            Operand::Literal(l) => {
                                                assert_eq!(l.as_str(), "100");
                                            }
                                            _ => panic!(
                                                "Expected Literal(100), got {:?}",
                                                inner_calc.right
                                            ),
                                        }
                                    }
                                    _ => panic!(
                                        "Expected Calculation as left, got {:?}",
                                        calc.left
                                    ),
                                }
                            }
                            _ => panic!(
                                "Expected Calculation operand, got {:?}",
                                operand
                            ),
                        },
                        _ => panic!(
                            "Expected Projection inside sum, got {:?}",
                            inner
                        ),
                    }
                }
                _ => panic!("Expected Arithmetic aggregation"),
            },
            _ => panic!("Expected Aggregation, got {:?}", node),
        }
    }

    /// リグレッション: sum(size:) は既存動作と同一
    #[test]
    fn test_parse_agg_projection_no_regression() {
        let node = parse("sum(size:)").expect("sum(size:) should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    match &**inner {
                        QueryNode::Projection(Operand::TypeRef(tt)) => {
                            assert_eq!(tt.as_str(), "size");
                        }
                        _ => panic!(
                            "Expected Projection(TypeRef(size)), got {:?}",
                            inner
                        ),
                    }
                }
                _ => panic!("Expected Arithmetic aggregation"),
            },
            _ => panic!("Expected Aggregation, got {:?}", node),
        }
    }

    /// リグレッション: count(extension:txt) は既存動作と同一
    #[test]
    fn test_parse_agg_typed_tag_no_regression() {
        let node = parse("count(extension:txt)")
            .expect("count(extension:txt) should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Count(ref inner) => match &**inner {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.label.tag_type().as_str(), "extension");
                        assert_eq!(tt.label.as_str(), "txt");
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

    /// リグレッション: count(extension:txt & size:>100) は既存動作と同一 (And ノード)
    #[test]
    fn test_parse_agg_set_op_no_regression() {
        let node = parse("count(extension:txt & size:>100)")
            .expect("count with set-op should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Count(ref inner) => match &**inner {
                    QueryNode::And(nodes) => {
                        assert_eq!(
                            nodes.len(),
                            2,
                            "And should have 2 children"
                        );
                    }
                    _ => panic!(
                        "Expected And node inside count, got {:?}",
                        inner
                    ),
                },
                _ => panic!("Expected Count aggregation"),
            },
            _ => panic!("Expected Aggregation, got {:?}", node),
        }
    }

    /// 新機能: 集合演算と算術演算の混在
    /// sum((is_dir:false & size:) + 1000)
    #[test]
    fn test_parse_parenthesized_expr_in_arithmetic() {
        let node = parse("sum((is_dir:false & size:) + 1000)")
            .expect("parenthesized expr with arithmetic should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    // inner は Projection(Calculation(...))
                    match &**inner {
                        QueryNode::Projection(Operand::Calculation(calc)) => {
                            // left = Query(And(...))
                            match &calc.left {
                                Operand::Query(node) => match &**node {
                                    QueryNode::And(nodes) => {
                                        assert_eq!(nodes.len(), 2);
                                    }
                                    _ => panic!(
                                        "Expected And node in Query, got {:?}",
                                        node
                                    ),
                                },
                                _ => panic!(
                                    "Expected Query operand, got {:?}",
                                    calc.left
                                ),
                            }
                            // op = Add
                            assert_eq!(calc.op, ArithmeticOp::Add);
                            // right = Literal(1000)
                            match &calc.right {
                                Operand::Literal(l) => {
                                    assert_eq!(l.as_str(), "1000");
                                }
                                _ => panic!(
                                    "Expected Literal operand, got {:?}",
                                    calc.right
                                ),
                            }
                        }
                        _ => panic!(
                            "Expected Projection(Calculation) inside sum, got {:?}",
                            inner
                        ),
                    }
                }
                _ => panic!("Expected Sum aggregation"),
            },
            _ => panic!("Expected Aggregation"),
        }
    }

    #[test]
    fn test_parse_count_empty_args() {
        let node = parse("count()").expect("count() should parse");
        match node {
            QueryNode::Aggregation(AggregationNode::Count(inner)) => {
                match &*inner {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.label.tag_type().as_str(), "*");
                        assert_eq!(tt.label.as_str(), "*");
                    }
                    _ => panic!(
                        "Expected TypedTag(*:*) inside count(), got {:?}",
                        inner
                    ),
                }
            }
            _ => panic!("Expected Count aggregation, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_sum_empty_args_fail() {
        let result = parse("sum()");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires an argument"));
    }

    // ========== Nest (&:) パーサーテスト ==========

    #[test]
    fn test_parse_nest_basic() {
        let node =
            parse("project: &: extension:").expect("nest basic should parse");
        match node {
            QueryNode::Nest(nest) => {
                match &*nest.left {
                    QueryNode::Projection(Operand::TypeRef(tt)) => {
                        assert_eq!(tt.as_str(), "project");
                    }
                    _ => panic!(
                        "Expected Projection(project) as left, got {:?}",
                        nest.left
                    ),
                }
                match &*nest.right {
                    QueryNode::Projection(Operand::TypeRef(tt)) => {
                        assert_eq!(tt.as_str(), "extension");
                    }
                    _ => panic!(
                        "Expected Projection(extension) as right, got {:?}",
                        nest.right
                    ),
                }
            }
            _ => panic!("Expected Nest, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_nest_with_agg() {
        let node = parse("parentdir: &: count(extension:jpg)")
            .expect("nest with agg should parse");
        match node {
            QueryNode::Nest(nest) => {
                match &*nest.left {
                    QueryNode::Projection(Operand::TypeRef(tt)) => {
                        assert_eq!(tt.as_str(), "parentdir");
                    }
                    _ => panic!(
                        "Expected Projection(parentdir), got {:?}",
                        nest.left
                    ),
                }
                match &*nest.right {
                    QueryNode::Aggregation(AggregationNode::Count(_)) => {}
                    _ => panic!(
                        "Expected Aggregation(Count) as right, got {:?}",
                        nest.right
                    ),
                }
            }
            _ => panic!("Expected Nest, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_nest_with_comparison() {
        let node = parse("project: &: (count(extension:jpg) > 10)")
            .expect("nest with comparison should parse");
        match node {
            QueryNode::Nest(nest) => {
                match &*nest.left {
                    QueryNode::Projection(Operand::TypeRef(tt)) => {
                        assert_eq!(tt.as_str(), "project");
                    }
                    _ => panic!(
                        "Expected Projection(project), got {:?}",
                        nest.left
                    ),
                }
                match &*nest.right {
                    QueryNode::Comparison(_) => {}
                    _ => panic!(
                        "Expected Comparison as right, got {:?}",
                        nest.right
                    ),
                }
            }
            _ => panic!("Expected Nest, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_nest_chain() {
        // a: &: b: &: c: → Nest(Nest(a, b), c) due to left-associativity
        let node = parse("a: &: b: &: c:").expect("nest chain should parse");
        match node {
            QueryNode::Nest(outer) => {
                match &*outer.right {
                    QueryNode::Projection(Operand::TypeRef(tt)) => {
                        assert_eq!(tt.as_str(), "c");
                    }
                    _ => panic!(
                        "Expected Projection(c) as outer right, got {:?}",
                        outer.right
                    ),
                }
                match &*outer.left {
                    QueryNode::Nest(inner) => {
                        match &*inner.left {
                            QueryNode::Projection(Operand::TypeRef(tt)) => {
                                assert_eq!(tt.as_str(), "a");
                            }
                            _ => panic!("Expected Projection(a) as inner left, got {:?}", inner.left),
                        }
                        match &*inner.right {
                            QueryNode::Projection(Operand::TypeRef(tt)) => {
                                assert_eq!(tt.as_str(), "b");
                            }
                            _ => panic!("Expected Projection(b) as inner right, got {:?}", inner.right),
                        }
                    }
                    _ => panic!(
                        "Expected Nest as outer left, got {:?}",
                        outer.left
                    ),
                }
            }
            _ => panic!("Expected Nest, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_nest_priority_over_and() {
        // a: &: b: & c:d → And(Nest(a, b), TypedTag(c:d))
        // &: has higher priority than &
        let node = parse("a: &: b: & c:d").expect("nest priority should parse");
        match node {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                match &nodes[0] {
                    QueryNode::Nest(nest) => {
                        match &*nest.left {
                            QueryNode::Projection(Operand::TypeRef(tt)) => {
                                assert_eq!(tt.as_str(), "a");
                            }
                            _ => panic!(
                                "Expected Projection(a), got {:?}",
                                nest.left
                            ),
                        }
                        match &*nest.right {
                            QueryNode::Projection(Operand::TypeRef(tt)) => {
                                assert_eq!(tt.as_str(), "b");
                            }
                            _ => panic!(
                                "Expected Projection(b), got {:?}",
                                nest.right
                            ),
                        }
                    }
                    _ => panic!(
                        "Expected Nest as first child, got {:?}",
                        nodes[0]
                    ),
                }
                match &nodes[1] {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.label.tag_type().as_str(), "c");
                        assert_eq!(tt.label.as_str(), "d");
                    }
                    _ => panic!("Expected TypedTag(c:d), got {:?}", nodes[1]),
                }
            }
            _ => panic!("Expected And, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_nest_nest_join() {
        // (a: &: b:) &: (c: &: d:) → Nest(Nest(a, b), Nest(c, d))
        let node = parse("(a: &: b:) &: (c: &: d:)")
            .expect("nest-nest join should parse");
        match node {
            QueryNode::Nest(outer) => {
                match &*outer.left {
                    QueryNode::Nest(left) => {
                        match &*left.left {
                            QueryNode::Projection(Operand::TypeRef(tt)) => {
                                assert_eq!(tt.as_str(), "a")
                            }
                            _ => panic!("Expected a"),
                        }
                        match &*left.right {
                            QueryNode::Projection(Operand::TypeRef(tt)) => {
                                assert_eq!(tt.as_str(), "b")
                            }
                            _ => panic!("Expected b"),
                        }
                    }
                    _ => panic!("Expected Nest as left, got {:?}", outer.left),
                }
                match &*outer.right {
                    QueryNode::Nest(right) => {
                        match &*right.left {
                            QueryNode::Projection(Operand::TypeRef(tt)) => {
                                assert_eq!(tt.as_str(), "c")
                            }
                            _ => panic!("Expected c"),
                        }
                        match &*right.right {
                            QueryNode::Projection(Operand::TypeRef(tt)) => {
                                assert_eq!(tt.as_str(), "d")
                            }
                            _ => panic!("Expected d"),
                        }
                    }
                    _ => {
                        panic!("Expected Nest as right, got {:?}", outer.right)
                    }
                }
            }
            _ => panic!("Expected Nest, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_nest_collect_types() {
        let node = parse("project: &: extension:").expect("should parse");
        let types = node.get_all_types();
        assert!(types.contains(&"project".to_string()));
        assert!(types.contains(&"extension".to_string()));
    }

    /// Nest 右辺の括弧内でラベル比較演算子 `:>` を使うとパースエラー。
    /// 括弧内はスカラー式なので `>` を使うべき。
    #[test]
    fn test_parse_nest_right_label_op_in_scalar_context_is_error() {
        let result = parse("parentdir: &: (count(extension:jpg) :> 1)");
        assert!(
            result.is_err(),
            "Using label op :> inside Nest right scalar context should be a parse error"
        );
    }

    #[test]
    fn test_parse_nested_arithmetic_in_nest() {
        // スペースなし
        let query_no_inner_space = "parentdir: &: ((sum(size:) + count()) / 2)";
        assert!(parse(query_no_inner_space).is_ok());

        // スペースあり (以前は失敗していたケース)
        let query_with_inner_space =
            "parentdir: &: ( (sum(size:) + count()) / 2 )";
        let res = parse(query_with_inner_space);
        assert!(res.is_ok(), "Should now parse with spaces: {:?}", res.err());
    }
}
