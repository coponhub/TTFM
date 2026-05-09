use crate::types::Rank;
use crate::tag::TagRegistry;
use sea_query::{CaseStatement, Condition, SimpleExpr};

/// システムにおける標準的なランク（優先度）の定義。
/// 数値が大きいほど、検索結果やカラム表示において優先されます。
pub struct SystemRank;

impl SystemRank {
    pub const NAME: Rank = 10;
    pub const TYPE_FROM_EXT: Rank = 9;
    pub const SIZE_STR: Rank = 8;
    pub const MODIFIED_STR: Rank = 7;
    pub const PARENT_DIR: Rank = 6;
    pub const ITEM_KIND: Rank = 5;
    pub const CONTENT: Rank = 4;
    pub const FILENAME: Rank = 1;
    pub const DEFAULT: Rank = 0;
    pub const PATH: Rank = -1;
}

/// FunctionRegistry の情報に基づき、ランク決定用の SQL 式を構築します。
///
/// # Arguments
/// * `registry` - IndexingFunction の定義（名前とデフォルトランク）を持つレジストリ
/// * `guard_condition` - ランク付けルールを適用するための条件（例: `ItemKind == "type"`）
/// * `key_expr` - ランク決定のキーとなる値を持つ式（例: `Content`）
/// * `default_rank` - 条件に合致しない、またはキーに対応するランクがない場合のデフォルト値
pub fn build_rank_expr(
    registry: &TagRegistry,
    guard_condition: Condition,
    key_expr: impl Into<SimpleExpr>,
    default_rank: Rank,
) -> SimpleExpr {
    let key: SimpleExpr = key_expr.into();
    let mut key_case = CaseStatement::new();

    // 1. Registryからランク情報を収集してCASE文を構築
    for func in registry.all_indexing_functions() {
        let rank = func.default_rank();
        // 0 (デフォルト) 以外の場合のみ明示的にルール化
        if rank != 0 {
            key_case = key_case.case(key.clone().eq(func.name()), rank);
        }
    }

    // 2. キーに対するCASE文を完成させる
    let key_rank_expr = key_case.finally(default_rank);

    // 3. ガード条件で包む
    CaseStatement::new()
        .case(guard_condition, key_rank_expr)
        .finally(default_rank)
        .into()
}

/// 指定されたタグ名に対応するデフォルトランクを取得します。
/// CLI (main.rs) 等で、単一のランク値を知りたい場合に使用します。
pub fn get_rank_by_name(registry: &TagRegistry, name: &str) -> Rank {
    // Registry内を検索
    for func in registry.all_indexing_functions() {
        if func.name() == name {
            return func.default_rank();
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Col, Pronoun::*};
    use crate::indexing::functions::IndexingFunction;
    use crate::taggers::{ColumnDef, TagValue, Tagger};
    use sea_query::{Expr, PostgresQueryBuilder, Query};

    // Mock IndexingFunction implementation
    struct MockTagger;
    impl Tagger for MockTagger {
        fn get_columns(&self) -> Vec<ColumnDef> {
            vec![]
        }
        fn tag_file(
            &self,
            _path: &std::path::Path,
        ) -> anyhow::Result<Vec<TagValue>> {
            Ok(vec![])
        }
    }

    struct MockFunc {
        name: String,
        rank: Rank,
    }
    impl IndexingFunction for MockFunc {
        fn name(&self) -> &str {
            &self.name
        }
        fn tagger(&self) -> Option<&dyn Tagger> {
            Some(&MockTagger)
        }
        fn default_rank(&self) -> Rank {
            self.rank
        }
    }

    fn create_registry() -> TagRegistry {
        let mut reg = TagRegistry::new();
        reg.register_plugin(Box::new(MockFunc {
            name: "high".to_string(),
            rank: 100,
        }));
        reg.register_plugin(Box::new(MockFunc {
            name: "low".to_string(),
            rank: 1,
        }));
        reg.register_plugin(Box::new(MockFunc {
            name: "zero".to_string(),
            rank: 0,
        }));
        // Add system defaults for testing
        reg.register_plugin(Box::new(MockFunc {
            name: "name".to_string(),
            rank: 10,
        }));
        reg.register_plugin(Box::new(MockFunc {
            name: "kind".to_string(),
            rank: 5,
        }));
        reg
    }

    #[test]
    fn test_get_rank_by_name() {
        let reg = create_registry();
        assert_eq!(get_rank_by_name(&reg, "high"), 100);
        assert_eq!(get_rank_by_name(&reg, "low"), 1);
        assert_eq!(get_rank_by_name(&reg, "zero"), 0);
        assert_eq!(get_rank_by_name(&reg, "unknown"), 0); // Default for unknown

        // System defaults check
        assert_eq!(get_rank_by_name(&reg, "name"), 10);
        assert_eq!(get_rank_by_name(&reg, "kind"), 5);
    }

    #[test]
    fn test_build_rank_expr_sql_generation() {
        let reg = create_registry();

        // Build the expression
        // Guard: col("kind") = "type"
        // Key: col("content")
        let expr = build_rank_expr(
            &reg,
            Condition::all().add(Expr::col(Kind).eq("type")),
            Expr::col(Col::Content),
            0,
        );

        // Convert to SQL string for verification
        let sql = Query::select().expr(expr).to_string(PostgresQueryBuilder);

        // Verification
        // "SELECT CASE WHEN "kind" = 'type' THEN CASE ... END ELSE 0 END"
        assert!(sql.contains(r#"CASE WHEN ("kind" = 'type') THEN"#));

        // Verify inner cases (Registry items)
        assert!(sql.contains(r#"WHEN ("content" = 'high') THEN 100"#));
        assert!(sql.contains(r#"WHEN ("content" = 'low') THEN 1"#));

        // "zero" (rank 0) should NOT be in the CASE statement (optimization)
        assert!(!sql.contains(r#"WHEN ("content" = 'zero'"#));

        // Verify system defaults
        assert!(sql.contains(r#"WHEN ("content" = 'name') THEN 10"#));
    }
}
