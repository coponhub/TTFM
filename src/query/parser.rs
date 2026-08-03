// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::query::ast::{
    AggregationNode, ArithmeticAggOp, ArithmeticOp, BasicOp, CalculationNode,
    ComparisonNode, ComparisonOp, NestNode, Operand, QueryNode,
};
use crate::query::error;
use crate::types::{Bitical, Label, TagType, TypedTag};
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
/// パース中に検出された警告は `sink` へ即時発出されます。
pub fn parse(
    input: &str,
    sink: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
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

    let node = match inner_pair.as_rule() {
        Rule::expr => build_expr(inner_pair, sink),
        Rule::scalar_arithmetic_query => {
            let inner = inner_pair.into_inner().next().unwrap();
            let calc = build_scalar_arithmetic_expr(inner, sink)?;
            Ok(QueryNode::base_nest(Operand::Calculation(Box::new(calc))))
        }
        Rule::EOI => Err(anyhow!("Empty query")),
        _ => Err(anyhow!(
            "Unexpected top-level rule: {:?}",
            inner_pair.as_rule()
        )),
    }?;

    if node.has_projection_intersection() {
        sink.warn(error::label_set_intersect_warning_msg());
    }

    Ok(node)
}

/// 警告を捨てて QueryNode のみを返す薄いラッパー。
pub fn parse_nowarn(input: &str) -> Result<QueryNode> {
    let mut discard: Vec<error::Warning> = Vec::new();
    parse(input, &mut discard)
}

fn build_ast(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    match pair.as_rule() {
        Rule::scalar_arithmetic_query => {
            let inner = pair.into_inner().next().unwrap();
            let calc = build_scalar_arithmetic_expr(inner, warnings)?;
            Ok(QueryNode::base_nest(Operand::Calculation(Box::new(calc))))
        }
        Rule::comparison => build_comparison(pair, warnings),
        Rule::typed_tag => build_typed_tag(pair),
        Rule::projection => build_projection(pair, warnings),
        Rule::aggregation => {
            build_aggregation(pair, warnings).map(QueryNode::Aggregation)
        }
        Rule::nest_expr => build_nest_expr(pair, warnings),
        Rule::expr => build_expr(pair, warnings),
        Rule::primary => build_primary(pair, warnings),
        Rule::factor => build_factor(pair, warnings),
        // complement was removed from grammar; // No changes needed for parser.rs as logic is generic enough.
        // Complement nodes are still generated internally by functions.rs (date Ne expansion).
        _ => Err(anyhow!(error::UNKNOWN_FACTOR_INNER)),
    }
}

fn build_aggregation(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<AggregationNode> {
    let mut inner = pair.into_inner();
    let op_pair = inner.next().ok_or_else(|| anyhow!("Missing aggregator"))?;
    let op_str = op_pair.as_str();
    // 引数（body_pair）をオプションとして受け取る
    let body_pair = inner.next();

    let node = if let Some(body_pair) = body_pair {
        match body_pair.as_rule() {
            Rule::bare_calculation => {
                let calc_inner = body_pair.into_inner().next().unwrap();
                let calc = build_calculation(calc_inner, warnings)?;
                QueryNode::base_nest(Operand::Calculation(Box::new(calc)))
            }
            Rule::expr => build_expr(body_pair, warnings)?,
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

fn build_expr(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    let pairs = pair.into_inner();
    get_parser()
        .map_primary(|primary| build_ast(primary, warnings))
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
                Rule::ampersand_colon => Ok(nest(lhs, rhs)),
                _ => Err(anyhow!(
                    "{}: {:?}",
                    error::UNKNOWN_INFIX_RULE,
                    op.as_rule()
                )),
            }
        })
        .parse(pairs)
}

fn build_primary(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    let inner = pair.into_inner().next().unwrap();
    build_ast(inner, warnings)
}

fn build_factor(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::expr => build_ast(inner, warnings),
        Rule::typed_tag => build_typed_tag(inner),
        Rule::comparison => build_comparison(inner, warnings),
        Rule::nest_expr => build_nest_expr(inner, warnings),
        Rule::projection => build_projection(inner, warnings),
        Rule::aggregation => {
            build_aggregation(inner, warnings).map(QueryNode::Aggregation)
        }
        Rule::label => {
            let label = build_label(inner)?;
            Ok(QueryNode::base_nest(Operand::Literal(label)))
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
        return Ok(QueryNode::base_nest(Operand::from(tagtype)));
    }

    Ok(QueryNode::TypedTag(TypedTag::retag(tagtype, &label)))
}

fn build_projection(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::type_ref => {
            let tag_type = TagType::from(inner.as_str().trim_end_matches(':'));
            Ok(QueryNode::base_nest(Operand::TypeRef(tag_type)))
        }
        Rule::calculation => {
            let calc = build_calculation(inner, warnings)?;
            Ok(QueryNode::base_nest(Operand::Calculation(Box::new(calc))))
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

fn build_comparison(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    // comparison = { label_comparison | scalar_comparison }
    let inner = pair.into_inner().next().unwrap();
    build_comparison_inner(inner, warnings)
}

/// scalar_comparison を nest_operand 内で直接処理するためのヘルパー。
fn build_comparison_from_scalar(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    build_comparison_inner(pair, warnings)
}

/// label_comparison または scalar_comparison のペアを受け取り Comparison ノードを構築します。
fn build_comparison_inner(
    inner: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    let rule = inner.as_rule();
    let mut inner_pairs = inner.into_inner();

    let first_pair = inner_pairs.next().unwrap();
    let first_text = first_pair.as_str().to_string();
    let first_op = build_operand(first_pair, warnings)?;
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
                let right_pair = step_inner.next().unwrap();
                let right_text = right_pair.as_str().to_string();
                let right_op = build_operand(right_pair, warnings)?;

                // 密着比較（空白なし）の未クォート右辺が ':' で終わる場合、
                // Projection のつもりが文字列として解釈されている可能性を警告する。
                // 空白ありの label_op / label_op_to_proj は右辺に type_ref を許容するため
                // ここには到達せず（Operand::TypeRef になる）、密着形のみを検出する。
                if matches!(&right_op, Operand::Literal(Label { value: Bitical::String(_), .. }))
                    && !right_text.starts_with('"')
                    && right_text.ends_with(':')
                {
                    warnings.warn(error::stuck_comparison_unquoted_colon_msg(
                        &first_text,
                        op_str,
                        &right_text,
                    ));
                }

                (ComparisonOp::Label(basic_op), right_op)
            }
            Rule::scalar_comparison => {
                // 不適切なスカラー演算のチェック (Example: size: > 100)
                // Post-processing error check has been moved to error::map_grammar_error

                let basic_op_str = actual_step.as_str();
                let basic_op = parse_scalar_basic_op(basic_op_str)?;
                let right_op =
                    build_operand(inner_pairs.next().unwrap(), warnings)?;

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

fn build_operand(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<Operand> {
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
        | Rule::scalar_calculation_inner
        | Rule::label_calc
        | Rule::label_calc_inner => Ok(Operand::Calculation(Box::new(
            build_calculation(inner, warnings)?,
        ))),
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
        Rule::aggregation => Ok(Operand::Aggregation(Box::new(
            build_aggregation(inner, warnings)?,
        ))),
        Rule::parenthesized_expr => {
            let expr_pair = inner.into_inner().next().unwrap();
            let node = build_expr(expr_pair, warnings)?;
            Ok(Operand::Query(Box::new(node)))
        }
        Rule::nest_expr => {
            let node = build_nest_expr(inner, warnings)?;
            Ok(Operand::Query(Box::new(node)))
        }
        Rule::nest_parenthesized_expr => {
            build_nested_parenthesized_operand(inner, warnings)
        }
        _ => Err(anyhow!(
            "{}: {:?}",
            error::UNKNOWN_OPERAND_RULE,
            inner.as_rule()
        )),
    }
}

fn build_nested_parenthesized_operand(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<Operand> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::nest_expr => {
            let node = build_nest_expr(inner, warnings)?;
            Ok(Operand::Query(Box::new(node)))
        }
        Rule::nest_parenthesized_expr => {
            build_nested_parenthesized_operand(inner, warnings)
        }
        _ => unreachable!("nest_parenthesized_expr inner must be nest_expr or nest_parenthesized_expr"),
    }
}

/// `nest_expr = ${ nest_operand ~ (WHITESPACE+ ~ ampersand_colon ~ WHITESPACE+ ~ nest_operand)+ }`
/// を左結合で Nest ノードにビルドします。
fn build_nest_expr(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    let mut inner = pair.into_inner();

    let first = inner.next().unwrap();
    let mut left = build_nest_operand(first, warnings)?;

    // inner yields alternating: ampersand_colon, nest_operand, ampersand_colon, nest_operand, ...
    // (WHITESPACE is silent _{ } so it doesn't produce tokens)
    while let Some(op_pair) = inner.next() {
        // op_pair should be ampersand_colon
        debug_assert_eq!(op_pair.as_rule(), Rule::ampersand_colon);
        let right_pair = inner.next().unwrap();
        let right = build_nest_operand(right_pair, warnings)?;
        left = nest(left, right);
    }
    Ok(left)
}

/// ワイルドカードキー（`*:`）を `&:` の単位元として正規化しつつ Nest ノードを構築します。
/// `X &: * = X`、`* &: X = X`。
fn nest(lhs: QueryNode, rhs: QueryNode) -> QueryNode {
    let right = rhs.into_operand();
    if is_base_key_operand(&right) {
        return lhs;
    }
    let left = match lhs.as_base_projection() {
        Some(op) if is_base_key_operand(op) => None,
        _ => Some(Box::new(lhs)),
    };
    QueryNode::Nest(NestNode { left, right })
}

fn is_base_key_operand(op: &Operand) -> bool {
    matches!(op, Operand::TypeRef(t) if t.is_base_key())
}

/// `nest_operand = { scalar_comparison | aggregation | projection | "(" ~ expr ~ ")" | label }`
fn build_nest_operand(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<QueryNode> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::scalar_comparison => build_comparison_from_scalar(inner, warnings),
        Rule::aggregation => {
            build_aggregation(inner, warnings).map(QueryNode::Aggregation)
        }
        Rule::projection => build_projection(inner, warnings),
        Rule::expr => build_expr(inner, warnings),
        Rule::label => {
            let label = build_label(inner)?;
            Ok(QueryNode::base_nest(Operand::Literal(label)))
        }
        _ => Err(anyhow!(
            "Unexpected rule in nest_operand: {:?}",
            inner.as_rule()
        )),
    }
}

fn build_calculation(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<CalculationNode> {
    let rule = pair.as_rule();
    let inner_pair = if rule == Rule::calculation
        || rule == Rule::scalar_calculation
        || rule == Rule::label_calc
    {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };

    let mut pairs = inner_pair.into_inner();

    let first_pair = pairs.next().unwrap();
    let mut left = build_operand_calc(first_pair, warnings)?;

    while let Some(op_pair) = pairs.next() {
        let op = parse_arithmetic_op(op_pair.as_str())?;
        let right_pair = pairs.next().unwrap();
        let right = build_operand_calc(right_pair, warnings)?;

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

fn build_operand_calc(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<Operand> {
    let rule = pair.as_rule();
    let inner = if rule == Rule::operand_calc
        || rule == Rule::scalar_operand
        || rule == Rule::scalar_operand_calc
        || rule == Rule::label_operand_calc
    {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::aggregation => Ok(Operand::Aggregation(Box::new(
            build_aggregation(inner, warnings)?,
        ))),
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
        | Rule::scalar_calculation_inner
        | Rule::label_calc
        | Rule::label_calc_inner => Ok(Operand::Calculation(Box::new(
            build_calculation(inner, warnings)?,
        ))),
        Rule::parenthesized_expr => {
            // 括弧で囲まれた式: (is_dir:false & size:)
            let expr_pair = inner.into_inner().next().unwrap();
            let node = build_expr(expr_pair, warnings)?;
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
            Ok(Label::from(s))
        }
        Rule::number => {
            let i = inner.as_str().parse::<i64>()?;
            Ok(crate::query::format::attach_formatted_node(Label::from(i)))
        }
        Rule::unquoted_string | Rule::unquoted_tag_string => {
            let s = unescape_glob_string(inner.as_str())?;
            Ok(crate::query::format::attach_formatted_node(Label::from(s)))
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
/// glob メタ文字（*, ?, [, ]）前のバックスラッシュは、リテラル指定として
/// 後段の照合へ渡すため保持します。
/// クエリ文法の `quoted_string` ルールで使用されます。
pub(crate) fn unescape_string(s: &str) -> Result<String> {
    let mut chars = s.chars();
    let mut pending = VecDeque::new();
    std::iter::from_fn(move || {
        if let Some(c) = pending.pop_front() {
            return Some(c);
        }
        match chars.next()? {
            '\\' => match chars.next() {
                Some('n') => Some('\n'),
                Some('r') => Some('\r'),
                Some('t') => Some('\t'),
                Some('\\') => Some('\\'),
                Some('\'') => Some('\''),
                Some('"') => Some('"'),
                Some(c @ ('*' | '?' | '[' | ']')) => {
                    pending.push_back(c);
                    Some('\\')
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

fn build_scalar_arithmetic_expr(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<CalculationNode> {
    let mut inner = pair.into_inner();
    let first = inner.next().unwrap();
    let mut lhs_operand = build_arithmetic_operand(first, warnings)?;

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
        let rhs_operand = build_arithmetic_operand(rhs_pair, warnings)?;

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

fn build_arithmetic_operand(
    pair: Pair<Rule>,
    warnings: &mut dyn error::WarningSink,
) -> Result<Operand> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::aggregation => {
            let agg = build_aggregation(inner, warnings)?;
            Ok(Operand::Aggregation(Box::new(agg)))
        }
        Rule::calculation => {
            let calc_inner = inner.into_inner().next().unwrap();
            let calc = build_calculation(calc_inner, warnings)?;
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
            let node = build_expr(expr_pair, warnings)?;
            Ok(Operand::Query(Box::new(node)))
        }
        Rule::number
        | Rule::label
        | Rule::quoted_string
        | Rule::unquoted_string => {
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
    fn test_literal_first_size_comparison_interprets_unit_string() {
        use crate::query::ast::{BasicOp, ComparisonNode, Operand};
        use crate::query::lens_resolver::Resolver;
        use crate::tag::TagRegistry;
        use crate::types::{Bitical, Label, SType, TagType};
        let resolver =
            Resolver::new_nowarn("\"1MB\" :> size:", &TagRegistry::with_standard())
                .expect("should expand");
        assert_eq!(
            resolver.expanded_query,
            QueryNode::Comparison(ComparisonNode {
                first: Operand::Literal(Label::other(Bitical::Integer(1_048_576))),
                rest: vec![(
                    ComparisonOp::Label(BasicOp::Gt),
                    Operand::TypeRef(TagType::from(SType::Size)),
                )],
            }),
        );
    }

    #[test]
    fn test_literal_first_mtime_comparison_interprets_date_string() {
        use crate::query::ast::{BasicOp, ComparisonNode, Operand};
        use crate::query::lens_resolver::Resolver;
        use crate::tag::TagRegistry;
        use crate::types::{DateTime, SType, TagType};
        use chrono::NaiveDate;
        let resolver =
            Resolver::new_nowarn("2026-02-01 :> mtime:", &TagRegistry::with_standard())
                .expect("should expand");
        let QueryNode::Comparison(ComparisonNode { first, rest }) = resolver.expanded_query
        else {
            panic!("expected Comparison, got {:?}", resolver.expanded_query)
        };
        let Operand::Literal(label) = first else {
            panic!("expected literal first")
        };
        let date = DateTime::Date(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());
        let (start, _) = date.to_interval().unwrap().as_interval().unwrap();
        assert_eq!(label.as_i64(), start);
        assert_eq!(
            rest,
            vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::TypeRef(TagType::from(SType::Mtime)),
            )]
        );
    }

    #[test]
    fn test_quoted_string_parses_to_label_literal() {
        let node =
            parse_nowarn("\"hello\"").expect("quoted string should parse");
        match node {
            QueryNode::Nest(NestNode { left: None, right: Operand::Literal(Label {
                value: Bitical::String(s),
                ..
            }) }) => {
                assert_eq!(s, "hello");
            }
            other => {
                panic!("Expected Literal(Label::Literal), got {:?}", other)
            }
        }
    }

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
    fn test_unescape_string_keeps_glob_escapes() {
        assert_eq!(unescape_string(r"foo\*bar").unwrap(), r"foo\*bar");
        assert_eq!(unescape_string(r"foo\?bar").unwrap(), r"foo\?bar");
        assert_eq!(unescape_string(r"foo\[bar").unwrap(), r"foo\[bar");
        assert_eq!(unescape_string(r"foo\]bar").unwrap(), r"foo\]bar");
        assert_eq!(unescape_string("foo*bar").unwrap(), "foo*bar");
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
        let node = parse_nowarn("name:test.txt")
            .expect("Failed to parse typed tag");
        match node {
            QueryNode::TypedTag(tt) => {
                assert_eq!(tt.tag_type().as_str(), "name");
                assert_eq!(tt.as_str(), "test.txt");
            }
            _ => panic!("Expected TypedTag, got {:?}", node),
        }
    }

    #[test]
    fn test_numeric_type_name_typed_tag() {
        let node =
            parse_nowarn("5:x").expect("numeric type name should parse");
        match node {
            QueryNode::TypedTag(tt) => {
                assert_eq!(tt.tag_type().as_str(), "5");
                assert_eq!(tt.as_str(), "x");
            }
            _ => panic!("Expected TypedTag, got {:?}", node),
        }
    }

    #[test]
    fn test_numeric_type_name_projection() {
        let node =
            parse_nowarn("5:").expect("numeric type_ref should parse");
        match node {
            QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) }) => {
                assert_eq!(tt.as_str(), "5");
            }
            _ => panic!("Expected Projection(TypeRef), got {:?}", node),
        }
    }

    #[test]
    fn test_numeric_type_name_with_numeric_label() {
        let node = parse_nowarn("123:456")
            .expect("numeric type:label should parse");
        match node {
            QueryNode::TypedTag(tt) => {
                assert_eq!(tt.tag_type().as_str(), "123");
                assert_eq!(tt.as_str(), "456");
            }
            _ => panic!("Expected TypedTag, got {:?}", node),
        }
    }

    #[test]
    fn test_numeric_type_name_bare_arithmetic() {
        parse_nowarn("5: + 3")
            .expect("bare arithmetic with numeric type_ref should parse");
    }

    #[test]
    fn test_numeric_type_name_in_aggregation() {
        parse_nowarn("sum(5:)")
            .expect("aggregation over numeric type_ref should parse");
    }

    #[test]
    fn test_stuck_comparison_rhs_calculation_with_projection_is_error() {
        let result = parse_nowarn("size:>(size: + 1)");
        assert!(
            result.is_err(),
            "expected error, got {:?}",
            result
        );
    }

    #[test]
    fn test_stuck_comparison_rhs_scalar_calculation_still_works() {
        parse_nowarn("size:>(1 + 2)")
            .expect("scalar-only calculation RHS should still parse");
    }

    #[test]
    fn test_stuck_comparison_unquoted_colon_rhs_warns() {
        let mut warnings: Vec<error::Warning> = Vec::new();
        parse("width:>height:", &mut warnings).expect("Failed to parse");
        assert!(
            warnings.iter().any(|w| w.0.contains("width: :> height:")),
            "Expected a warning suggesting 'width: :> height:', got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_stuck_comparison_quoted_rhs_does_not_warn() {
        let mut warnings: Vec<error::Warning> = Vec::new();
        parse(r#"width:>"height:""#, &mut warnings)
            .expect("Failed to parse");
        assert!(
            warnings.is_empty(),
            "Quoted RHS is an explicit literal, should not warn: {:?}",
            warnings
        );
    }

    #[test]
    fn test_spaced_comparison_rhs_ending_in_colon_does_not_warn() {
        let mut warnings: Vec<error::Warning> = Vec::new();
        parse("width: :> height:", &mut warnings)
            .expect("Failed to parse");
        assert!(
            warnings.is_empty(),
            "Spaced form's RHS parses as TypeRef, not a literal, so no warning should fire: {:?}",
            warnings
        );
    }

    #[test]
    fn test_projection_intersection_warns() {
        let mut warnings: Vec<error::Warning> = Vec::new();
        parse("parentdir: & extension:", &mut warnings)
            .expect("Failed to parse");
        assert!(
            warnings.iter().any(|w| w.0.contains("&:")),
            "Expected Projection intersection warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_typed_tag_intersection_does_not_warn() {
        let mut warnings: Vec<error::Warning> = Vec::new();
        parse("extension:rs & size:>100", &mut warnings)
            .expect("Failed to parse");
        assert!(
            warnings.is_empty(),
            "Non-projection And should not warn: {:?}",
            warnings
        );
    }

    #[test]
    fn test_parse_comparison_simple() {
        // Using colon-operator format for explicit Label comparison
        let node =
            parse_nowarn("size:>100").expect("Failed to parse comparison");
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
    fn test_arithmetic_operand_number_is_integer_not_string() {
        use crate::types::Bitical;
        let node = parse_nowarn("count() + 1").expect("should parse");
        match node {
            QueryNode::Nest(NestNode { left: None, right: Operand::Calculation(calc) }) => match calc.right {
                Operand::Literal(l) => {
                    assert_eq!(l.value(), Bitical::Integer(1));
                }
                other => panic!("Expected Literal, got {:?}", other),
            },
            other => panic!("Expected Projection(Calculation), got {:?}", other),
        }
    }

    // --- build_label: Format 解釈の付与 ---

    #[test]
    fn test_unquoted_label_attaches_formatted_when_format_claims() {
        use crate::query::format::{ByteSizeRange, Formatted};
        use crate::types::LabelNode;
        let node = parse_nowarn("size:1MB").expect("should parse");
        match node {
            QueryNode::TypedTag(tt) => {
                assert_eq!(
                    tt.label.node(),
                    &LabelNode::Formatted(Formatted::ByteSizeRange(
                        ByteSizeRange::Range { lo: 1_048_576, hi: 1_048_576 }
                    ))
                );
            }
            _ => panic!("Expected TypedTag, got {:?}", node),
        }
    }

    #[test]
    fn test_number_label_attaches_formatted_bitical_integer() {
        use crate::query::format::Formatted;
        use crate::types::LabelNode;
        let node = parse_nowarn("cat:42").expect("should parse");
        match node {
            QueryNode::TypedTag(tt) => {
                assert_eq!(
                    tt.label.node(),
                    &LabelNode::Formatted(Formatted::Bitical(Bitical::Integer(42)))
                );
            }
            _ => panic!("Expected TypedTag, got {:?}", node),
        }
    }

    #[test]
    fn test_unquoted_label_stays_default_when_no_format_claims() {
        use crate::types::LabelNode;
        let node = parse_nowarn("cat:hello").expect("should parse");
        match node {
            QueryNode::TypedTag(tt) => {
                assert_eq!(tt.label.node(), &LabelNode::DefaultLabelNode);
            }
            _ => panic!("Expected TypedTag, got {:?}", node),
        }
    }

    #[test]
    fn test_quoted_label_stays_default_even_when_format_would_claim() {
        use crate::types::LabelNode;
        let node = parse_nowarn("size:\"1MB\"").expect("should parse");
        match node {
            QueryNode::TypedTag(tt) => {
                assert_eq!(tt.label.node(), &LabelNode::DefaultLabelNode);
            }
            _ => panic!("Expected TypedTag, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_logical_ops() {
        // AND
        let node = parse_nowarn("a:1 & b:2").expect("Failed to parse AND");
        match node {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
            }
            _ => panic!("Expected And, got {:?}", node),
        }

        // Parentheses
        let node = parse_nowarn("(a:1 | b:2)")
            .expect("Failed to parse parentheses");
        match node {
            QueryNode::Or(nodes) => assert_eq!(nodes.len(), 2),
            _ => panic!("Expected Or, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_origin_projection() {
        let node =
            parse_nowarn("origin:").expect("Failed to parse origin:");
        match node {
            QueryNode::Nest(NestNode { left: None, right: op }) => match op {
                Operand::TypeRef(tt) => assert_eq!(tt.as_str(), "origin"),
                _ => panic!("Expected TypeRef operand, got {:?}", op),
            },
            _ => panic!("Expected Projection(origin), got {:?}", node),
        }
    }

    #[test]
    fn test_parse_count_items() {
        let node = parse_nowarn("count(name:*.rs)")
            .expect("Failed to parse count(name:*.rs)");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Count(inner) => match &*inner {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.tag_type().as_str(), "name");
                        assert_eq!(tt.as_str(), "*.rs");
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
        let node = parse_nowarn("count(extension:)")
            .expect("Failed to parse count(extension:)");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Count(inner) => match &*inner {
                    QueryNode::Nest(NestNode { left: None, right: op }) => match op {
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
        let node =
            parse_nowarn("sum(size:)").expect("Failed to parse sum(size:)");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    match &**inner {
                        QueryNode::Nest(NestNode { left: None, right: op }) => match op {
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
        let node = parse_nowarn("sum(size:) > 100")
            .expect("Failed to parse comparison");
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
        let result = parse_nowarn("size: > 100");
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
        let node = parse_nowarn("sum(size: - 100)")
            .expect("bare_calc sum should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    match &**inner {
                        QueryNode::Nest(NestNode { left: None, right: operand }) => match operand {
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
        let node = parse_nowarn("sum(size: + 100 - 50)")
            .expect("bare_calc multiop should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    match &**inner {
                        QueryNode::Nest(NestNode { left: None, right: operand }) => match operand {
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
        let node =
            parse_nowarn("sum(size:)").expect("sum(size:) should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    match &**inner {
                        QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) }) => {
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
        let node = parse_nowarn("count(extension:txt)")
            .expect("count(extension:txt) should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Count(ref inner) => match &**inner {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.tag_type().as_str(), "extension");
                        assert_eq!(tt.as_str(), "txt");
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
        let node = parse_nowarn("count(extension:txt & size:>100)")
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
        let node = parse_nowarn("sum((is_dir:false & size:) + 1000)")
            .expect("parenthesized expr with arithmetic should parse");
        match node {
            QueryNode::Aggregation(agg) => match agg {
                AggregationNode::Arithmetic { op, ref inner } => {
                    assert_eq!(op, ArithmeticAggOp::Sum);
                    // inner は Projection(Calculation(...))
                    match &**inner {
                        QueryNode::Nest(NestNode { left: None, right: Operand::Calculation(calc) }) => {
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
        let node = parse_nowarn("count()").expect("count() should parse");
        match node {
            QueryNode::Aggregation(AggregationNode::Count(inner)) => {
                match &*inner {
                    QueryNode::TypedTag(tt) => {
                        assert_eq!(tt.tag_type().as_str(), "*");
                        assert_eq!(tt.as_str(), "*");
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
        let result = parse_nowarn("sum()");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires an argument"));
    }

    // ========== Nest (&:) パーサーテスト ==========

    #[test]
    fn test_parse_nest_basic() {
        let node = parse_nowarn("project: &: extension:")
            .expect("nest basic should parse");
        match node {
            QueryNode::Nest(nest) => {
                match nest.left.as_deref() {
                    Some(QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) })) => {
                        assert_eq!(tt.as_str(), "project");
                    }
                    _ => panic!(
                        "Expected Projection(project) as left, got {:?}",
                        nest.left
                    ),
                }
                match &nest.right {
                    Operand::TypeRef(tt) => {
                        assert_eq!(tt.as_str(), "extension");
                    }
                    _ => panic!(
                        "Expected TypeRef(extension) as right, got {:?}",
                        nest.right
                    ),
                }
            }
            _ => panic!("Expected Nest, got {:?}", node),
        }
    }

    #[test]
    fn test_parse_nest_with_agg() {
        let node = parse_nowarn("parentdir: &: count(extension:jpg)")
            .expect("nest with agg should parse");
        match node {
            QueryNode::Nest(nest) => {
                match nest.left.as_deref() {
                    Some(QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) })) => {
                        assert_eq!(tt.as_str(), "parentdir");
                    }
                    _ => panic!(
                        "Expected Projection(parentdir), got {:?}",
                        nest.left
                    ),
                }
                match &nest.right {
                    Operand::Aggregation(agg)
                        if matches!(agg.as_ref(), AggregationNode::Count(_)) => {}
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
        let node = parse_nowarn("project: &: (count(extension:jpg) > 10)")
            .expect("nest with comparison should parse");
        match node {
            QueryNode::Nest(nest) => {
                match nest.left.as_deref() {
                    Some(QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) })) => {
                        assert_eq!(tt.as_str(), "project");
                    }
                    _ => panic!(
                        "Expected Projection(project), got {:?}",
                        nest.left
                    ),
                }
                match &nest.right {
                    Operand::Query(q)
                        if matches!(q.as_ref(), QueryNode::Comparison(_)) => {}
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
        let node = parse_nowarn("a: &: b: &: c:")
            .expect("nest chain should parse");
        match node {
            QueryNode::Nest(outer) => {
                match &outer.right {
                    Operand::TypeRef(tt) => {
                        assert_eq!(tt.as_str(), "c");
                    }
                    _ => panic!(
                        "Expected TypeRef(c) as outer right, got {:?}",
                        outer.right
                    ),
                }
                match outer.left.as_deref() {
                    Some(QueryNode::Nest(inner)) => {
                        match inner.left.as_deref() {
                            Some(QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) })) => {
                                assert_eq!(tt.as_str(), "a");
                            }
                            _ => panic!("Expected Projection(a) as inner left, got {:?}", inner.left),
                        }
                        match &inner.right {
                            Operand::TypeRef(tt) => {
                                assert_eq!(tt.as_str(), "b");
                            }
                            _ => panic!("Expected TypeRef(b) as inner right, got {:?}", inner.right),
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
    fn bare_projection_is_a_nest_without_a_left_operand() {
        let node = parse_nowarn("extension:").expect("bare key should parse");
        let QueryNode::Nest(n) = node else {
            panic!("Expected Nest, got {:?}", node);
        };
        assert!(n.left.is_none(), "bare key must have no left operand");
        assert_eq!(n.right, Operand::TypeRef(TagType::from("extension")));
    }

    #[test]
    fn wildcard_key_is_the_identity_of_nest() {
        assert_eq!(
            parse_nowarn("*: &: extension:").unwrap(),
            parse_nowarn("extension:").unwrap()
        );
        assert_eq!(
            parse_nowarn("extension: &: *:").unwrap(),
            parse_nowarn("extension:").unwrap()
        );
        assert_eq!(
            parse_nowarn("*: &: *:").unwrap(),
            parse_nowarn("*:").unwrap()
        );
        assert_eq!(
            parse_nowarn("*: &: a: &: b:").unwrap(),
            parse_nowarn("a: &: b:").unwrap()
        );
        assert_eq!(
            parse_nowarn("a: &: *: &: b:").unwrap(),
            parse_nowarn("a: &: b:").unwrap()
        );
    }

    #[test]
    fn test_parse_nest_priority_over_and() {
        // a: &: b: & c:d → And(Nest(a, b), TypedTag(c:d))
        // &: has higher priority than &
        let node = parse_nowarn("a: &: b: & c:d")
            .expect("nest priority should parse");
        match node {
            QueryNode::And(nodes) => {
                assert_eq!(nodes.len(), 2);
                match &nodes[0] {
                    QueryNode::Nest(nest) => {
                        match nest.left.as_deref() {
                            Some(QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) })) => {
                                assert_eq!(tt.as_str(), "a");
                            }
                            _ => panic!(
                                "Expected Projection(a), got {:?}",
                                nest.left
                            ),
                        }
                        match &nest.right {
                            Operand::TypeRef(tt) => {
                                assert_eq!(tt.as_str(), "b");
                            }
                            _ => panic!(
                                "Expected TypeRef(b), got {:?}",
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
                        assert_eq!(tt.tag_type().as_str(), "c");
                        assert_eq!(tt.as_str(), "d");
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
        let node = parse_nowarn("(a: &: b:) &: (c: &: d:)")
            .expect("nest-nest join should parse");
        match node {
            QueryNode::Nest(outer) => {
                match outer.left.as_deref() {
                    Some(QueryNode::Nest(left)) => {
                        match left.left.as_deref() {
                            Some(QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) })) => {
                                assert_eq!(tt.as_str(), "a")
                            }
                            _ => panic!("Expected a"),
                        }
                        match &left.right {
                            Operand::TypeRef(tt) => {
                                assert_eq!(tt.as_str(), "b")
                            }
                            _ => panic!("Expected b"),
                        }
                    }
                    _ => panic!("Expected Nest as left, got {:?}", outer.left),
                }
                match &outer.right {
                    Operand::Query(q) => match q.as_ref() {
                        QueryNode::Nest(right) => {
                            match right.left.as_deref() {
                                Some(QueryNode::Nest(NestNode { left: None, right: Operand::TypeRef(tt) })) => {
                                    assert_eq!(tt.as_str(), "c")
                                }
                                _ => panic!("Expected c"),
                            }
                            match &right.right {
                                Operand::TypeRef(tt) => {
                                    assert_eq!(tt.as_str(), "d")
                                }
                                _ => panic!("Expected d"),
                            }
                        }
                        _ => panic!(
                            "Expected Nest as right, got {:?}",
                            outer.right
                        ),
                    },
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
        let node = parse_nowarn("project: &: extension:")
            .expect("should parse");
        let types = node.get_all_types();
        assert!(types.contains(&"project".to_string()));
        assert!(types.contains(&"extension".to_string()));
    }

    /// Nest 右辺の括弧内でラベル比較演算子 `:>` を使うとパースエラー。
    /// 括弧内はスカラー式なので `>` を使うべき。
    #[test]
    fn test_parse_nest_right_label_op_in_scalar_context_is_error() {
        let result =
            parse_nowarn("parentdir: &: (count(extension:jpg) :> 1)");
        assert!(
            result.is_err(),
            "Using label op :> inside Nest right scalar context should be a parse error"
        );
    }

    #[test]
    fn test_parse_nested_arithmetic_in_nest() {
        // スペースなし
        let query_no_inner_space = "parentdir: &: ((sum(size:) + count()) / 2)";
        assert!(parse_nowarn(query_no_inner_space).is_ok());

        // スペースあり (以前は失敗していたケース)
        let query_with_inner_space =
            "parentdir: &: ( (sum(size:) + count()) / 2 )";
        let res = parse_nowarn(query_with_inner_space);
        assert!(res.is_ok(), "Should now parse with spaces: {:?}", res.err());
    }
}
