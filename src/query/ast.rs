use crate::types::{Label, SType, TagType, TypedTag};
use std::collections::HashSet;

// ========== Type Aliases ==========
/// 比較演算子とオペランドのペアのリスト（連鎖比較用）
pub type ComparisonChain = Vec<(ComparisonOp, Operand)>;

// ========== Enums & Structs ==========

/// 比較演算子の種類。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ComparisonOp {
    Scalar(BasicOp), // スカラー比較 (>)
    Label(BasicOp),  // ラベル比較 (:>)
}

/// 基本的な比較演算子。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BasicOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

/// 算術演算子の種類。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ArithmeticOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// 比較演算または算術演算のオペランド。
///
/// タグへの参照（TypeRef）またはリテラル値（Literal）のいずれか。
#[derive(Debug, PartialEq, Clone)]
pub enum Operand {
    Literal(Label),
    TypeRef(TagType),
    Calculation(Box<CalculationNode>),
}

/// 算術演算ノード（加算、減算、乗算、除算）。
#[derive(Debug, PartialEq, Clone)]
pub struct CalculationNode {
    pub left: Operand,
    pub op: ArithmeticOp,
    pub right: Operand,
}

/// 比較演算ノード（`a == b`, `a < b < c` など）。
///
/// 最初のオペランドと、それに続く比較演算子とオペランドのチェーン。
#[derive(Debug, PartialEq, Clone)]
pub struct ComparisonNode {
    pub first: Operand,
    pub rest: ComparisonChain,
}

// ComparisonNode impl (expandメソッド) は functions.rs または mod.rs で扱うのが適切だが、
// ここではデータ構造に専念させ、ロジックは分離したい。
// しかし expand は QueryFunctionRegistry に依存するため、ast.rs には置けない（循環依存回避）。
// したがって、expand は struct impl から削除し、外部関数として定義するか、
// functions.rs に置く。

/// 検索クエリの構造を表す抽象構文木（AST）ノード。
/// 論理演算（AND, OR, NOT）や検索語（単語、型付きタグ）を保持します。
#[derive(Debug, PartialEq, Clone)]
pub enum QueryNode {
    /// AND条件 (`A & B` または `A B`)。多分木構造。
    And(Vec<QueryNode>),
    /// OR条件 (`A | B`)。多分木構造。
    Or(Vec<QueryNode>),
    /// 二項差集合 (`A - B`)
    Difference(Box<QueryNode>, Box<QueryNode>),
    /// 補集合 (`^(A)`)
    Complement(Box<QueryNode>),
    /// 比較演算
    Comparison(ComparisonNode),
    /// 汎用タグ検索 (TypedTag 型を使用)
    TypedTag(TypedTag),
    /// 物理カラムに対する検索 (rank, size, mtime, name, id 等)
    ColumnMatch { tag: SType, label: Label },
    /// ラベル取得 (Projection)
    Projection(TagType),
}

impl QueryNode {
    /// このノードおよび子ノードに含まれるすべてのタグの型（`key`）を収集します。
    pub fn get_all_types(&self) -> Vec<String> {
        let mut types = HashSet::new();
        self.collect_types(&mut types);
        types.into_iter().collect()
    }

    /// このクエリで投影（Projection）されているタグ型の一覧を取得します。
    pub fn get_projections(&self) -> Vec<String> {
        let mut projections = HashSet::new();
        self.collect_projections(&mut projections);
        projections.into_iter().collect()
    }

    fn collect_types(&self, types: &mut HashSet<String>) {
        match self {
            QueryNode::And(nodes) => {
                for node in nodes {
                    node.collect_types(types);
                }
            }
            QueryNode::Or(nodes) => {
                for node in nodes {
                    node.collect_types(types);
                }
            }
            QueryNode::Difference(l, r) => {
                l.collect_types(types);
                r.collect_types(types);
            }
            QueryNode::Complement(c) => {
                c.collect_types(types);
            }
            QueryNode::Comparison(cmp) => {
                cmp.collect_types(types);
            }
            QueryNode::ColumnMatch { tag, label } => {
                if *tag == SType::Type {
                    types.insert(label.as_str());
                } else {
                    types.insert(tag.to_string());
                }
            }
            QueryNode::TypedTag(tt) => {
                types.insert(tt.label.tag_type().as_str().to_string());
            }
            QueryNode::Projection(tt) => {
                types.insert(tt.as_str().to_string());
            }
        }
    }

    fn collect_projections(&self, projections: &mut HashSet<String>) {
        match self {
            QueryNode::And(nodes) | QueryNode::Or(nodes) => {
                for node in nodes {
                    node.collect_projections(projections);
                }
            }
            QueryNode::Difference(l, _) => {
                l.collect_projections(projections);
            }
            QueryNode::Complement(c) => {
                c.collect_projections(projections);
            }
            QueryNode::Projection(tt) => {
                projections.insert(tt.as_str().to_string());
            }
            _ => {}
        }
    }
}

impl ComparisonNode {
    fn collect_types(&self, types: &mut HashSet<String>) {
        self.first.collect_types(types);
        for (_, op) in &self.rest {
            op.collect_types(types);
        }
    }
}

impl Operand {
    fn collect_types(&self, types: &mut HashSet<String>) {
        match self {
            Operand::Literal(_) => {}
            Operand::TypeRef(tt) => {
                types.insert(tt.as_str().to_string());
            }
            Operand::Calculation(calc) => {
                calc.left.collect_types(types);
                calc.right.collect_types(types);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TagType;

    #[test]
    fn test_get_all_types_simple() {
        // Simple TypedTag
        let node = QueryNode::TypedTag(TypedTag::new("size", "100"));
        let types = node.get_all_types();
        assert_eq!(types.len(), 1);
        assert!(types.contains(&"size".to_string()));

        // Projection
        let node = QueryNode::Projection(TagType::from("rank"));
        let types = node.get_all_types();
        assert_eq!(types.len(), 1);
        assert!(types.contains(&"rank".to_string()));
    }

    #[test]
    fn test_get_all_types_nested() {
        // And(TypedTag(name), Or(TypedTag(size), TypedTag(mtime)))
        let node = QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new("name", "foo")),
            QueryNode::Or(vec![
                QueryNode::TypedTag(TypedTag::new("size", "100")),
                QueryNode::TypedTag(TypedTag::new("mtime", "today")),
            ]),
        ]);
        let types = node.get_all_types();
        assert_eq!(types.len(), 3);
        assert!(types.contains(&"name".to_string()));
        assert!(types.contains(&"size".to_string()));
        assert!(types.contains(&"mtime".to_string()));
    }

    #[test]
    fn test_get_all_types_ops() {
        // Comparison
        // size > 100
        let node = QueryNode::Comparison(ComparisonNode {
            first: Operand::TypeRef(TagType::from("size")),
            rest: vec![(ComparisonOp::Label(BasicOp::Gt), Operand::Literal("100".into()))],
        });
        let types = node.get_all_types();
        assert_eq!(types.len(), 1);
        assert!(types.contains(&"size".to_string()));

        // Difference
        // name:foo - extension:txt
        let node = QueryNode::Difference(
            Box::new(QueryNode::TypedTag(TypedTag::new("name", "foo"))),
            Box::new(QueryNode::TypedTag(TypedTag::new("extension", "txt"))),
        );
        let types = node.get_all_types();
        assert!(types.contains(&"name".to_string()));
        assert!(types.contains(&"extension".to_string()));
    }

    #[test]
    fn test_get_projections() {
        // Projection only
        let node = QueryNode::Projection(TagType::from("path"));
        let projs = node.get_projections();
        assert_eq!(projs.len(), 1);
        assert!(projs.contains(&"path".to_string()));

        // Mixed with filters (projections should still be collected)
        // name:foo AND project:size
        let node = QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new("name", "foo")),
            QueryNode::Projection(TagType::from("size")),
        ]);
        let projs = node.get_projections();
        assert_eq!(projs.len(), 1);
        assert!(projs.contains(&"size".to_string()));
    }

    #[test]
    fn test_operand_calculation_collect() {
        // Arithmetic operand using types: width * height
        let calc = CalculationNode {
            left: Operand::TypeRef(TagType::from("width")),
            op: ArithmeticOp::Mul,
            right: Operand::TypeRef(TagType::from("height")),
        };
        let op = Operand::Calculation(Box::new(calc));

        // Wrap in a comparison to test via QueryNode or manually helper
        // Let's test helper directly via ComparisonNode
        let node = QueryNode::Comparison(ComparisonNode {
            first: op,
            rest: vec![(ComparisonOp::Label(BasicOp::Gt), Operand::Literal("100".into()))],
        });

        let types = node.get_all_types();
        assert!(types.contains(&"width".to_string()));
        assert!(types.contains(&"height".to_string()));
    }
}
