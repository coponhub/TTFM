// Error messages for the query module

use crate::query::ast::{AggregationNode, ArithmeticAggOp, Operand, QueryNode};

// =========================================================================
// Parser Errors
// =========================================================================

pub const PARSE_ERROR: &str = "Parse error";
pub const NO_QUERY_FOUND: &str = "No query found";
pub const NO_EXPRESSION_FOUND: &str = "No expression found";
pub const UNKNOWN_INFIX_RULE: &str = "Unknown infix rule";
pub const UNKNOWN_FACTOR_INNER: &str = "Unknown factor inner";
pub const MISSING_TAG_KEY: &str = "Missing tag key";
pub const MISSING_TAG_LABEL: &str = "Missing tag label";
pub const UNEXPECTED_TAG_TYPE_RULE: &str = "Unexpected tag_type rule";
pub const UNKNOWN_COMPARISON_OP: &str = "Unknown comparison op";
pub const UNKNOWN_OPERAND_RULE: &str = "Unknown operand rule";
pub const UNKNOWN_ARITHMETIC_OP: &str = "Unknown arithmetic op";
pub const CALC_REQUIRES_OP: &str =
    "Calculation must contain at least one operation";
pub const UNKNOWN_OPERAND_CALC_RULE: &str = "Unknown operand_calc rule";
pub const UNKNOWN_LABEL_RULE: &str = "Unknown label rule";

pub fn aggregator_requires_argument(agg: &str) -> anyhow::Error {
    anyhow::anyhow!("Aggregator '{}' requires an argument", agg)
}

// Helper functions for dynamic error messages (Parser)

pub fn invalid_scalar_comparison_msg(
    op: &str,
    proj: &str,
    rhs: &str,
) -> String {
    format!(
        "Invalid operator '{}': Scalar comparison cannot be applied to a Projection ('{}:'). \nDid you mean: '{}: :{} {}'",
        op, proj, proj, op, rhs
    )
}

/// QueryNodeを簡易的に文字列化（提案生成用）
fn node_to_simple_string(node: &QueryNode) -> String {
    match node {
        QueryNode::TypedTag(tt) => {
            format!("{}:{}", tt.label.tag_type().as_str(), tt.label.as_str())
        }
        QueryNode::Projection(Operand::TypeRef(tt)) => {
            format!("{}:", tt.as_str())
        }
        QueryNode::Aggregation(agg) => match agg {
            AggregationNode::Count(inner) => {
                format!("count({})", node_to_simple_string(inner))
            }
            AggregationNode::Arithmetic { op, inner } => {
                let op_str = match op {
                    ArithmeticAggOp::Sum => "sum",
                    ArithmeticAggOp::Avg => "avg",
                    ArithmeticAggOp::Max => "max",
                    ArithmeticAggOp::Min => "min",
                };
                format!("{}({})", op_str, node_to_simple_string(inner))
            }
        },
        QueryNode::And(_) => "...".to_string(),
        _ => "...".to_string(),
    }
}

/// 集合演算のスカラーオペランドエラーに対する修正提案を生成
fn generate_suggestion(
    nodes: &[QueryNode],
    operation: &str,
    left_is_set: bool,
    right_is_set: bool,
) -> Option<String> {
    if nodes.len() != 2 {
        return None;
    }

    let left = &nodes[0];
    let right = &nodes[1];

    // 片方が集約で、もう片方が集合の場合のみ提案を生成
    match (left, right, left_is_set, right_is_set, operation) {
        // 左が集約、右が集合、演算子が & の場合：sum(size:) & type:file → sum(type:file & size:)
        (QueryNode::Aggregation(agg), set_node, false, true, "&") => {
            match agg {
                AggregationNode::Count(inner) => Some(format!(
                    "count({} & {})",
                    node_to_simple_string(set_node),
                    node_to_simple_string(inner)
                )),
                AggregationNode::Arithmetic { op, inner } => {
                    let op_str = match op {
                        ArithmeticAggOp::Sum => "sum",
                        ArithmeticAggOp::Avg => "avg",
                        ArithmeticAggOp::Max => "max",
                        ArithmeticAggOp::Min => "min",
                    };
                    Some(format!(
                        "{}({} & {})",
                        op_str,
                        node_to_simple_string(set_node),
                        node_to_simple_string(inner)
                    ))
                }
            }
        }
        // 左が集合、右が集約、演算子が & の場合：type:file & sum(size:) → sum(type:file & size:)
        (set_node, QueryNode::Aggregation(agg), true, false, "&") => {
            match agg {
                AggregationNode::Count(inner) => Some(format!(
                    "count({} & {})",
                    node_to_simple_string(set_node),
                    node_to_simple_string(inner)
                )),
                AggregationNode::Arithmetic { op, inner } => {
                    let op_str = match op {
                        ArithmeticAggOp::Sum => "sum",
                        ArithmeticAggOp::Avg => "avg",
                        ArithmeticAggOp::Max => "max",
                        ArithmeticAggOp::Min => "min",
                    };
                    Some(format!(
                        "{}({} & {})",
                        op_str,
                        node_to_simple_string(set_node),
                        node_to_simple_string(inner)
                    ))
                }
            }
        }
        _ => None,
    }
}

pub fn invalid_set_operation_operand_msg(
    nodes: &[QueryNode],
    op: &str,
    operand_type: &str,
    left_is_set: bool,
    right_is_set: bool,
) -> String {
    let mut msg = if !left_is_set && !right_is_set {
        // 両方がスカラーの場合
        format!(
            "Set operations between scalars are not implemented.\n\
             Set operation '{}' contains only scalar values ({}).",
            op, operand_type
        )
    } else {
        // 片方がスカラー、片方が集合の場合
        format!(
            "Set operations between sets and scalars are not implemented.\n\
             Set operation '{}' contains a scalar value ({}).",
            op, operand_type
        )
    };

    if let Some(hint) =
        generate_suggestion(nodes, op, left_is_set, right_is_set)
    {
        msg.push_str(&format!("\n\nDid you mean?: '{}'", hint));
    }

    msg
}

pub fn map_grammar_error(
    input: &str,
    mut e: pest::error::Error<crate::query::parser::Rule>,
) -> anyhow::Error {
    use pest::error::{ErrorVariant, LineColLocation};

    // Fallback error (e.g. "Parse error: ...") if we don't match our specific cases
    // We return anyhow!(e) at the end to keep the pretty formatting.

    // Check if we can improve the error message for invalid scalar comparisons
    let (line, col) = match e.line_col {
        LineColLocation::Pos((l, c)) => (l, c),
        _ => return anyhow::anyhow!(e),
    };

    let line_str = match input.lines().nth(line - 1) {
        Some(s) => s,
        None => return anyhow::anyhow!(e),
    };

    // --- 1. Try original position first ---
    let prefix_orig: String = line_str.chars().take(col - 1).collect();
    let error_char_orig = line_str.chars().nth(col - 1).unwrap_or(' ');

    if let Some(msg) =
        check_proj_scalar_misuse(&prefix_orig, error_char_orig, line_str, col)
    {
        e.variant = ErrorVariant::CustomError { message: msg };
        return anyhow::anyhow!(e);
    }

    if let Some(msg) = check_mismatched_operator_usage(
        &prefix_orig,
        error_char_orig,
        line_str,
        col,
    ) {
        e.variant = ErrorVariant::CustomError { message: msg };
        return anyhow::anyhow!(e);
    }

    if let Some(msg) =
        check_arithmetic_parentheses_misuse(line_str, error_char_orig, col)
    {
        e.variant = ErrorVariant::CustomError { message: msg };
        return anyhow::anyhow!(e);
    }

    // --- 2. Enhanced Lookback for Redirection ---
    // Pest often reports errors at the deepest point. For "1 :> 100", Case 2 matches until ":"
    // but fails at "100" (expecting Proj). We want to treat this error at ":" for friendly message.
    let mut actual_col = col;
    let mut error_char = error_char_orig;

    if !is_scalar_op_char(error_char) && error_char != ':' {
        // Look back for the start of the operator sequence
        let mut lookback = actual_col;
        while lookback > 1 {
            let prev = line_str.chars().nth(lookback - 2).unwrap_or(' ');
            if prev == ':' || is_scalar_op_char(prev) {
                // Found an operator! Now crawl to its start.
                actual_col = lookback - 1;
                error_char = prev;

                let mut start = lookback;
                while start > 1 {
                    let p = line_str.chars().nth(start - 2).unwrap_or(' ');
                    if p == ':' || is_scalar_op_char(p) {
                        actual_col = start - 1;
                        error_char = p;
                        if p == ':' {
                            break;
                        } // reached start of label op
                        start -= 1;
                    } else if p.is_whitespace() {
                        start -= 1;
                    } else {
                        break; // Hit something else (like operand)
                    }
                }
                break;
            }
            // Skip alphanumeric or spaces until we find an operator
            lookback -= 1;
        }
    }

    if actual_col != col {
        let prefix_new: String =
            line_str.chars().take(actual_col - 1).collect();

        // 1. Check conditions for "Scalar comparison on Projection" (e.g. size: > 100)
        if let Some(msg) = check_proj_scalar_misuse(
            &prefix_new,
            error_char,
            line_str,
            actual_col,
        ) {
            e.variant = ErrorVariant::CustomError { message: msg };
            return anyhow::anyhow!(e);
        }

        // 2. Check for mismatched patterns "Scalar > Projection" or "Aggr :> Scalar"
        if let Some(msg) = check_mismatched_operator_usage(
            &prefix_new,
            error_char,
            line_str,
            actual_col,
        ) {
            e.variant = ErrorVariant::CustomError { message: msg };
            return anyhow::anyhow!(e);
        }
    }

    anyhow::anyhow!(e)
}

fn is_scalar_op_char(c: char) -> bool {
    "><=^".contains(c)
}

fn check_proj_scalar_misuse(
    prefix: &str,
    error_char: char,
    line_str: &str,
    col: usize,
) -> Option<String> {
    if !is_scalar_op_char(error_char) {
        return None;
    }

    // Extract projection name from prefix (handles both direct and calculation cases)
    let proj_name = extract_projection_name(prefix)?;

    // Get the RHS after the operator
    let rhs: String = line_str
        .chars()
        .skip(col)
        .collect::<String>()
        .trim_start_matches(is_scalar_op_char)
        .trim()
        .to_string();

    Some(invalid_scalar_comparison_msg(
        &error_char.to_string(),
        proj_name,
        rhs.trim(),
    ))
}

/// Extract projection name from prefix string
/// Handles two cases:
/// 1. Direct projection: "size: " -> "size"
/// 2. Calculation with projection: "(size: + 1) " -> "size"
fn extract_projection_name(prefix: &str) -> Option<&str> {
    let trimmed = prefix.trim_end();

    // Case 1: Direct projection (ends with ':')
    if trimmed.ends_with(':') {
        let full_proj = trimmed.split_whitespace().last().unwrap_or("?");
        let proj = full_proj.trim_end_matches(':');

        // Check if it starts with a digit - tags shouldn't start with digits!
        if proj.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            return None;
        }

        return Some(proj);
    }

    // Case 2: Calculation with projection (ends with ')')
    if !trimmed.ends_with(')') {
        return None;
    }

    // Find the matching opening paren
    let mut depth = 0;
    let mut start_idx = None;
    for (idx, ch) in trimmed.chars().rev().enumerate() {
        if ch == ')' {
            depth += 1;
        } else if ch == '(' {
            depth -= 1;
            if depth == 0 {
                start_idx = Some(trimmed.len() - idx - 1);
                break;
            }
        }
    }

    let start = start_idx?;
    let calc_expr = &trimmed[start..];

    // Check if the calculation contains a projection
    if !calc_expr.contains(':') {
        return None;
    }

    // Extract projection name from the calculation
    // Look for pattern like "size:" or "mtime:"
    calc_expr
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':')
        .find(|s| s.ends_with(':') && s.len() > 1)
        .map(|s| s.trim_end_matches(':'))
}

fn check_mismatched_operator_usage(
    prefix: &str,
    error_char: char,
    line_str: &str,
    col: usize,
) -> Option<String> {
    if error_char == ':' {
        let tokens: Vec<&str> = prefix.split_whitespace().collect();

        // Case A: 100 > size: (Error at :)
        if let Some(msg) = check_scalar_op_target_proj(&tokens, prefix) {
            return Some(msg);
        }

        // Case B: sum(size:) :> 100 (Error at :)
        if let Some(msg) = check_label_op_misuse(&tokens, line_str, col) {
            return Some(msg);
        }
        return None;
    }

    // Case C: 100 > [s]ize: (Error at start of word)
    check_scalar_op_target_proj_start(prefix, line_str, col)
}

fn check_scalar_op_target_proj(
    tokens: &[&str],
    _prefix: &str,
) -> Option<String> {
    let prop_name = tokens.last()?;
    if tokens.len() < 2 {
        return None;
    }

    let prev_op = tokens[tokens.len() - 2];
    // Check if the token looks like a scalar operator (composed of valid chars)
    if prev_op.is_empty() || !prev_op.chars().all(is_scalar_op_char) {
        return None;
    }

    // Likely "Scalar > Proj". Suggest "Scalar :> Proj"
    let lhs_raw = tokens[..tokens.len() - 2].join(" ");
    let lhs = lhs_raw
        .trim_end_matches(|c: char| c == ':' || is_scalar_op_char(c))
        .trim();
    let scalar_op = prev_op.trim_start_matches(':');
    let proj = format!("{}:", prop_name);

    Some(format!(
        "Invalid usage: Projection ('{}') cannot be used in Scalar Comparison with '{}'. \nDid you mean: '{} :{} {}'",
        proj, scalar_op, lhs, scalar_op, proj
    ))
}

fn check_label_op_misuse(
    tokens: &[&str],
    line_str: &str,
    col: usize,
) -> Option<String> {
    // col points to ':' (1-indexed). line_str.chars().nth(col-1) is ':'
    let label_op_char = line_str.chars().nth(col).unwrap_or(' ');
    if !is_scalar_op_char(label_op_char) {
        return None;
    }

    let full_op = format!(":{}", label_op_char);

    // lhs is from start of line until just before the ':' (col - 1 chars)
    let lhs = line_str.chars().take(col - 1).collect::<String>();
    // rhs starts after the ':' and the operator character (skip(col + 1))
    let rhs = line_str.chars().skip(col + 1).collect::<String>();

    // Check LHS type
    let lhs_token = tokens.last()?;

    let object_type = if lhs_token.ends_with(')') {
        "Aggregation/Calculation"
    } else if lhs_token.ends_with(':') {
        "Projection"
    } else {
        "Scalar/Value"
    };

    // Check if it looks like an arithmetic expression that needs parentheses
    let is_arithmetic = tokens
        .iter()
        .any(|t| ["/", "*", "+", "-", "x", "%"].contains(t));

    if is_arithmetic && object_type == "Aggregation/Calculation" {
        // Concrete suggestion for arithmetic context
        Some(format!(
            "Invalid operator '{}': Cannot apply label comparison to an unparenthesized arithmetic expression.\n\
             Did you mean: '({}) {}{}'",
            full_op,
            lhs.trim(),
            full_op,
            rhs
        ))
    } else {
        // Standard message with specific suggestion
        Some(format!(
            "Invalid operator '{}': Label Comparison cannot be applied to {} ('{}'). \nDid you mean: '{} {} {}'",
            full_op,
            object_type,
            lhs_token,
            lhs_token,
            label_op_char,
            rhs.trim()
        ))
    }
}

fn check_scalar_op_target_proj_start(
    prefix: &str,
    line_str: &str,
    col: usize,
) -> Option<String> {
    let remainder: String = line_str.chars().skip(col - 1).collect();
    let word = remainder.split_whitespace().next().unwrap_or("");

    if !word.ends_with(':') || word.len() <= 1 {
        return None;
    }

    let trimmed_prefix = prefix.trim_end();
    let last_char = trimmed_prefix.chars().last()?;

    if !is_scalar_op_char(last_char) {
        return None;
    }

    let scalar_op = last_char;
    let lhs = trimmed_prefix
        .trim_end_matches(|c: char| c == ':' || is_scalar_op_char(c))
        .trim();
    let proj = word;

    Some(format!(
        "Invalid usage: Projection ('{}') cannot be used in Scalar Comparison with '{}'. \nDid you mean: '{} :{} {}'",
        proj, scalar_op, lhs, scalar_op, proj
    ))
}

// =========================================================================
// Logical Resolver Errors
// =========================================================================

pub const ARITHMETIC_ONLY_NUMERIC: &str =
    "Arithmetic operations are only possible for numeric types.";

pub const PARENTHESIZED_EXPR_MUST_RETURN_PROJECTION: &str =
    "Parenthesized expression '(...)' in arithmetic context must return a Projection. \
    For example, '(is_dir:false & size:)' returns a Projection and is valid, \
    but '(is_dir:false & is_dir:true)' returns an item set and is invalid.";

pub const PARENTHESIZED_EXPR_IN_COMPARISON_MUST_RETURN_PROJECTION: &str =
    "Parenthesized expression '(...)' in comparison or arithmetic context must return a Projection";

// =========================================================================
// Lens Resolver Errors
// =========================================================================

pub const ARITHMETIC_STRING_UNSUPPORTED: &str =
    "Unsupported arithmetic operation for String (only '+' and '*' are allowed)";
pub const ARITHMETIC_MIXED_TYPES: &str =
    "Arithmetic between String and non-String is not allowed";
pub const AGGREGATION_STRING_UNSUPPORTED: &str =
    "Unsupported aggregation for String type";

pub fn unsupported_string_arithmetic(op: &str) -> anyhow::Error {
    anyhow::anyhow!("{}: '{}'", ARITHMETIC_STRING_UNSUPPORTED, op)
}

pub fn unsupported_mixed_type_arithmetic(
    left: &str,
    right: &str,
) -> anyhow::Error {
    anyhow::anyhow!("{}: {} and {}", ARITHMETIC_MIXED_TYPES, left, right)
}

pub fn unsupported_string_aggregation(op: &str) -> anyhow::Error {
    anyhow::anyhow!("{}: '{}'", AGGREGATION_STRING_UNSUPPORTED, op)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_suggestion_no_double_colon() {
        // Case: "width: :> height:" (Fails at 'h')
        let prefix = "width: :> ";
        let line_str = "width: :> height:";
        let col = 11; // pointing to 'h'

        let msg =
            check_scalar_op_target_proj_start(prefix, line_str, col).unwrap();
        println!("Message: {}", msg);
        assert!(!msg.contains("width: : :>"), "Should not have double colon");
        assert!(
            msg.contains("width: :> height:"),
            "Should have correct suggestion"
        );
    }

    #[test]
    fn test_error_suggestion_scalar_proj() {
        // Case: "width: > height:" (Fails at 'h')
        let prefix = "width: > ";
        let line_str = "width: > height:";
        let col = 10; // pointing to 'h'

        let msg =
            check_scalar_op_target_proj_start(prefix, line_str, col).unwrap();
        assert!(msg.contains("width: :> height:"));
    }

    #[test]
    fn test_aggregator_requires_argument_msg() {
        let err = aggregator_requires_argument("sum");
        assert_eq!(err.to_string(), "Aggregator 'sum' requires an argument");
    }
}
fn check_arithmetic_parentheses_misuse(
    line_str: &str,
    error_char: char,
    col: usize,
) -> Option<String> {
    if !"+-*/%x".contains(error_char) {
        return None;
    }

    // Identify operand boundaries
    let op_idx = col - 1;
    let lhs_start = find_lhs_boundary(line_str, op_idx);
    let rhs_end = find_rhs_boundary(line_str, op_idx);

    let lhs = &line_str[lhs_start..op_idx];
    let rhs = &line_str[op_idx + 1..rhs_end];

    let prefix = &line_str[..lhs_start];
    let suffix = &line_str[rhs_end..];

    let lhs_trimmed = lhs.trim();
    let rhs_trimmed = rhs.trim();

    // The core suggestion: (LHS) OP (RHS)
    // We add an outer wrapper ( ... ) ONLY if we are not already inside a matching pair
    // of parentheses that define the extent of this arithmetic operation.
    let suggestion_body = format!("({}) {} ({})", lhs_trimmed, error_char, rhs_trimmed);

    // If we're not at the top level (prefix or suffix exist), we should ensure
    // the whole calculation is parenthesized to be a valid calculation node.
    let full_suggestion = if prefix.ends_with('(') && suffix.starts_with(')') {
        // Already looks wrapped in ( ... )
        format!("{}{}{}", prefix, suggestion_body, suffix)
    } else {
        format!("{}({}){}", prefix, suggestion_body, suffix)
    };

    Some(format!(
        "Syntax Error: Arithmetic operations require parentheses when mixed with other operations at the same level.\n\
         Did you mean: '{}'",
        full_suggestion
    ))
}

fn find_lhs_boundary(line_str: &str, op_idx: usize) -> usize {
    let mut paren_count = 0;
    let chars: Vec<char> = line_str.chars().collect();

    for i in (0..op_idx).rev() {
        let c = chars[i];
        match c {
            ')' => paren_count += 1,
            '(' if paren_count == 0 => return i + 1,
            '(' => paren_count -= 1,
            _ if paren_count == 0 && c.is_whitespace() => {
                if check_boundary_before_whitespace(&chars, i) {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    0
}

fn check_boundary_before_whitespace(chars: &[char], space_idx: usize) -> bool {
    let mut prev_idx = space_idx;
    while prev_idx > 0 && chars[prev_idx - 1].is_whitespace() {
        prev_idx -= 1;
    }
    if prev_idx == 0 {
        return false;
    }

    let pc = chars[prev_idx - 1];
    if "&|-".contains(pc) {
        // Avoid stopping at &: or -:
        return !(prev_idx < chars.len() && chars[prev_idx] == ':');
    }
    "> < =".contains(pc)
}

fn find_rhs_boundary(line_str: &str, op_idx: usize) -> usize {
    let mut paren_count = 0;
    let chars: Vec<char> = line_str.chars().collect();

    for i in (op_idx + 1)..chars.len() {
        let c = chars[i];
        match c {
            '(' => paren_count += 1,
            ')' if paren_count == 0 => return i,
            ')' => paren_count -= 1,
            _ if paren_count == 0 && c.is_whitespace() => {
                if check_boundary_at_whitespace(&chars, i) {
                    return i;
                }
            }
            _ => {}
        }
    }
    chars.len()
}

fn check_boundary_at_whitespace(chars: &[char], space_idx: usize) -> bool {
    let mut next_idx = space_idx;
    while next_idx < chars.len() && chars[next_idx].is_whitespace() {
        next_idx += 1;
    }
    if next_idx >= chars.len() {
        return false;
    }

    let nc = chars[next_idx];
    if "&|-".contains(nc) {
        // Avoid stopping at &: or -:
        return !(next_idx + 1 < chars.len() && chars[next_idx + 1] == ':');
    }
    ":><=".contains(nc)
}
