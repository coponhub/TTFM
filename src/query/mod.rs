pub mod ast;
pub mod fetcher;
pub mod functions;
pub mod lens;
pub mod parser;
pub mod sql;

pub use ast::*;
pub use functions::*;
pub use parser::*;
pub use sql::*;

// Restore impl QueryNode methods to maintain API compatibility
impl QueryNode {
    pub fn expand(self, registry: &QueryFunctionRegistry) -> QueryNode {
        functions::expand_query_node(self, registry)
    }

    pub fn to_sql(&self, view_name: &str) -> sea_query::SelectStatement {
        sql::to_sql(self, view_name)
    }

    pub fn to_tag_condition(&self) -> sea_query::Condition {
        sql::to_tag_condition(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_types() {
        let node = parse("extension:rs").expect("Failed to parse");
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.label.tag_type().as_str(), "extension");
            assert_eq!(tt.label.as_str(), "rs");
        } else {
            panic!("Should be a TypedTag");
        }
    }

    #[test]
    fn test_basic_structure() {
        let q = "extension:rs";
        let node = parse(q).expect("Failed to parse");
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.label.tag_type().as_str(), "extension");
            assert_eq!(tt.label.as_str(), "rs");
        } else {
            panic!("Expected TypedTag");
        }
    }

    #[test]
    fn test_pest_grammar_basics() {
        // Basic parsing test using the new grammar
        let queries = [
            "type:file",
            "extension:rs & ^(path:*/target/*)",
            "^(extension:pdf)",
            "size:>1024",
            "size:>=1024",
            "size:<2048",
            "size:<=2048",
            "rank:==5",
            "rank:^=1", // Not Equal
            "50:< width: :< 100",
            "10:<= height: :<= 20",
            "name:\"My File\" | name:'Other File'",
            "extension:pdf - filename:test.pdf",
        ];

        for q in queries {
            parse(q)
                .map_err(|e| panic!("Failed to parse query '{}': {}", q, e))
                .unwrap();
        }
    }

    #[test]
    fn test_pest_grammar_strict_conformance() {
        // Test spaces (should fail according to DESIGN.md)
        let fail_queries = [
            "^ (extension:pdf)", // Space after ^
            "extension : rs",    // Space around :
            "size :> 100",       // Space before :>
            "size : > 100",      // Invalid projection with scalar op
        ];
        for q in fail_queries {
            assert!(
                parse(q).is_err(),
                "Query '{}' should fail due to space constraints",
                q
            );
        }

        // Test unary minus (should fail according to DESIGN.md)
        let q_unary = "-type:file";
        assert!(parse(q_unary).is_ok(), "Unary minus should be valid now");
    }

    #[test]
    fn test_pest_grammar_complex_math() {
        // Multi-level math and negative numbers
        let q = "(size: - -100) > (width: * (height: / 2))";
        parse(q)
            .map_err(|e| panic!("Failed to parse math query '{}': {}", q, e))
            .unwrap();
    }

    #[test]
    fn test_query_to_sql_ranking() {
        use sea_query::PostgresQueryBuilder;
        let q = parse("extension:rs").unwrap();
        let sql = q.to_sql("oneview").to_string(PostgresQueryBuilder);

        // rank, item_id, item_kind が選択されていることを確認
        assert!(
            sql.contains("\"rank\""),
            "SQL should select rank column: {}",
            sql
        );
        assert!(
            sql.contains("\"item_id\""),
            "SQL should select item_id column: {}",
            sql
        );
        assert!(
            sql.contains("\"item_kind\""),
            "SQL should select item_kind column: {}",
            sql
        );
        assert!(
            sql.contains("DISTINCT"),
            "SQL should contain DISTINCT for leaf nodes: {}",
            sql
        );
    }

    #[test]
    fn test_query_to_sql_and_precedence() {
        use sea_query::PostgresQueryBuilder;
        let q = parse("type:file & extension:rs").unwrap();
        let sql = q.to_sql("oneview").to_string(PostgresQueryBuilder);

        // INTERSECT が使用されていることを確認
        assert!(
            sql.contains("INTERSECT"),
            "AND query should use INTERSECT: {}",
            sql
        );
        // 各項がサブクエリでラップされ、Rank, ItemKind が引き継がれていることを確認
        assert!(
            sql.contains("\"rank\""),
            "Subqueries should select rank: {}",
            sql
        );
        assert!(
            sql.contains("\"item_kind\""),
            "Subqueries should select item_kind: {}",
            sql
        );
    }
}
