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

// =========================================================================
// Logical Resolver Errors
// =========================================================================

pub const ARITHMETIC_ONLY_NUMERIC: &str = "Arithmetic operations are only possible for numeric types.";
