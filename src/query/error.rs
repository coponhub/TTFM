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
pub const CALC_REQUIRES_OP: &str = "Calculation must contain at least one operation";
pub const UNKNOWN_OPERAND_CALC_RULE: &str = "Unknown operand_calc rule";
pub const UNKNOWN_LABEL_RULE: &str = "Unknown label rule";

// Helper functions for dynamic error messages (Parser)

pub fn invalid_scalar_comparison_msg(op: &str, proj: &str, rhs: &str) -> String {
    format!(
        "Invalid operator '{}': Scalar comparison cannot be applied to a Projection ('{}:'). \nDid you mean: '{}: :{} {}'",
        op, proj, proj, op, rhs
    )
}

pub fn map_grammar_error(input: &str, mut e: pest::error::Error<crate::query::parser::Rule>) -> anyhow::Error {
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

    // col is 1-based index to the error character/position
    let prefix: String = line_str.chars().take(col - 1).collect();
    
    // Check conditions for "Scalar comparison on Projection"
    if prefix.trim_end().ends_with(':') {
        let error_char = line_str.chars().nth(col - 1).unwrap_or(' ');
        
        if "><=^".contains(error_char) {
             let proj = prefix.trim_end().split_whitespace().last().unwrap_or("?")
                .trim_end_matches(':');
             // Skip the error character and any subsequent operator characters to get the true RHS
             let rhs: String = line_str.chars().skip(col).collect::<String>()
                .trim_start_matches(|c: char| "><=^".contains(c)).to_string();

             let message = invalid_scalar_comparison_msg(
                &error_char.to_string(),
                proj,
                rhs.trim()
             );
             
             // Replace the error variant with our custom message
             // This preserves the line/col and line_string inside 'e', ensuring pretty printing.
             e.variant = ErrorVariant::CustomError { message };
             return anyhow::anyhow!(e);
        }
    }

    anyhow::anyhow!(e)
}

// =========================================================================
// Logical Resolver Errors
// =========================================================================

pub const ARITHMETIC_ONLY_NUMERIC: &str = "Arithmetic operations are only possible for numeric types.";
