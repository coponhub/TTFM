// Error messages for the query module

// =========================================================================
// Parser Errors
// =========================================================================

pub const PARSE_ERROR: &str = "Parse error";
pub const NO_QUERY_FOUND: &str = "No query found";
pub const NO_EXPRESSION_FOUND: &str = "No expression found";
pub const UNKNOWN_INFIX_RULE: &str = "Unknown infix rule";
pub const UNKNOWN_FACTOR_INNER: &str = "Unknown factor inner";
pub const COMPLEMENT_MISSING_EXPR: &str = "Complement missing expr";
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
        let prefix_new: String = line_str.chars().take(actual_col - 1).collect();

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
    if prefix.trim_end().ends_with(':') && is_scalar_op_char(error_char) {
        let full_proj =
            prefix.trim_end().split_whitespace().last().unwrap_or("?");
        let proj = full_proj.trim_end_matches(':');

        // Check if it starts with a digit - tags shouldn't start with digits!
        if proj.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            return None;
        }

        // Skip the error character and any subsequent operator characters to get the true RHS
        let rhs: String = line_str
            .chars()
            .skip(col)
            .collect::<String>()
            .trim_start_matches(is_scalar_op_char)
            .to_string();

        return Some(invalid_scalar_comparison_msg(
            &error_char.to_string(),
            proj,
            rhs.trim(),
        ));
    }
    None
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
    let next_char = line_str.chars().nth(col).unwrap_or(' ');
    if !is_scalar_op_char(next_char) {
        return None;
    }

    let label_op_suffix = next_char;
    let full_op = format!(":{}", label_op_suffix);

    // Check LHS
    let lhs_token = tokens.last()?;
    // If we are here, it means 'scalar_comparison' failed at ':'
    // behavior. This means LHS was parsed as scalar_operand.
    // If LHS was 'sum(size:)', it is aggregation.
    // If LHS was '1', it is numeric scalar.
    // We want to explain that ':>' (Label Op) cannot be used with these.

     let object_type = if lhs_token.ends_with(')') {
         "Aggregation/Calculation"
     } else if lhs_token.ends_with(':') {
         "Projection"
     } else {
         "Scalar/Value"
     };

     Some(format!(
        "Invalid operator '{}': Label Comparison cannot be applied to {} ('{}'). \nDid you mean: '{} {} ...'",
        full_op, object_type, lhs_token, lhs_token, label_op_suffix
     ))
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

        let msg = check_scalar_op_target_proj_start(prefix, line_str, col).unwrap();
        assert!(msg.contains("width: :> height:"));
    }
}
