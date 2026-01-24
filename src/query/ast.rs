use crate::types::{Label, SType, TagType, TypedTag};
use std::collections::HashSet;

// ========== Type Aliases ==========
/// 比較演算子とオペランドのペアのリスト（連鎖比較用）
pub type ComparisonChain = Vec<(ComparisonOp, Operand)>;

// ========== Enums & Structs ==========

/// 比較演算子の種類。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ComparisonOp {
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
            QueryNode::ColumnMatch { .. } => {}
            QueryNode::TypedTag(tt) => {
                types.insert(tt.tagtype.as_str().to_string());
            }
            QueryNode::Projection(tt) => {
                types.insert(tt.as_str().to_string());
            }
        }
    }

    fn collect_projections(
        &self,
        projections: &mut HashSet<String>,
    ) {
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
