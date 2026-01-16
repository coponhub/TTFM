use anyhow::{Result, anyhow};
use std::collections::HashMap;
// Unused imports removed
use sea_query::{Alias, BinOper, Expr, Query, SelectStatement};
use crate::types::{Label, SType, TagType, TypedTag};
use crate::db::{Tbl, Col};
use pest_derive::Parser;
use pest::Parser;

#[derive(Parser)]
#[grammar = "query.pest"]
pub struct PestQueryParser;

use pest::iterators::Pair;
use pest::pratt_parser::{PrattParser, Op, Assoc};
use std::sync::OnceLock;

static PRATT_PARSER: OnceLock<PrattParser<Rule>> = OnceLock::new();

fn get_parser() -> &'static PrattParser<Rule> {
    PRATT_PARSER.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::pipe, Assoc::Left) | Op::infix(Rule::minus, Assoc::Left))
            .op(Op::infix(Rule::ampersand, Assoc::Left))
    })
}

/// クエリ文字列を解析し、QueryNode AST を構築します。
pub fn parse(input: &str) -> Result<QueryNode> {
    let mut pairs = PestQueryParser::parse(Rule::query, input)
        .map_err(|e| anyhow!("Parse error: {}", e))?;
    let expr_pair = pairs.next().ok_or_else(|| anyhow!("No query found"))?
        .into_inner().next().ok_or_else(|| anyhow!("No expression found"))?;
    build_ast(expr_pair)
}

fn build_ast(pair: Pair<Rule>) -> Result<QueryNode> {
    match pair.as_rule() {
        Rule::expr => {
            let pairs = pair.into_inner();
            get_parser()
                .map_primary(|primary| build_ast(primary))
                .map_infix(|lhs, op, rhs| {
                    let lhs = lhs?;
                    let rhs = rhs?;
                    match op.as_rule() {
                        Rule::ampersand => {
                             // Combine And nodes if possible, but strict binary is fine for now.
                             // Wait, previously And was Vec.
                             // Binary reduction: And(vec![l, r])
                             // If lhs is And, we can flatten? 
                             // Pratt reduces binary. A & B & C -> ((A & B) & C).
                             // We can merge if we want, or just build nested binary trees and flatten later, 
                             // OR check types here.
                             // For simplicity and matching prior multi-child structure:
                             match lhs {
                                 QueryNode::And(mut v) => {
                                     v.push(rhs);
                                     Ok(QueryNode::And(v))
                                 }
                                 _ => Ok(QueryNode::And(vec![lhs, rhs]))
                             }
                        }
                        Rule::pipe => {
                             match lhs {
                                 QueryNode::Or(mut v) => {
                                     v.push(rhs);
                                     Ok(QueryNode::Or(v))
                                 }
                                 _ => Ok(QueryNode::Or(vec![lhs, rhs]))
                             }
                        }
                        Rule::minus => {
                             Ok(QueryNode::Difference(Box::new(lhs), Box::new(rhs)))
                        }
                        _ => Err(anyhow!("Unknown infix rule: {:?}", op.as_rule())),
                    }
                })
                .parse(pairs)
        }
        Rule::primary => {
            let inner = pair.into_inner().next().unwrap();
            build_ast(inner)
        }
        Rule::factor => {
             // factor = { "(" ~ expr ~ ")" | typed_tag | comparison }
             let inner = pair.into_inner().next().unwrap();
             match inner.as_rule() {
                 Rule::expr => build_ast(inner),
                 Rule::typed_tag => build_typed_tag(inner),
                 Rule::comparison => build_comparison(inner),
                 _ => Err(anyhow!("Unknown factor inner: {:?}", inner.as_rule())),
             }
        }
        Rule::complement => {
             let mut inner = pair.into_inner();
             let _ = inner.next(); // ^
             // The grammar for complement: "^" ~ "(" ~ expr ~ ")"
             // inner pairs: (expr)
             // Wait. `complement = { "^" ~ "(" ~ expr ~ ")" }`
             // Pest pairs: Literal "^", Literal "(", Rule expr, Literal ")"
             // If rules are atomic or silent, it changes.
             // complement is normal rule.
             // Literals don't show up in `into_inner()` unless strict coverage?
             // Default: no.
             // So `inner` contains `expr`.
             // Let's debug if needed, but usually inner.next() is expr.
             let expr_pair = inner.next().ok_or_else(|| anyhow!("Complement missing expr"))?;
             Ok(QueryNode::Complement(Box::new(build_ast(expr_pair)?)))
        }
        _ => Err(anyhow!("Unexpected rule in build_ast: {:?}", pair.as_rule())),
    }
}

fn build_typed_tag(pair: Pair<Rule>) -> Result<QueryNode> {
    // typed_tag = ${ tag_type ~ ":" ~ label }
    let mut inner = pair.into_inner();
    let type_pair = inner.next().ok_or_else(|| anyhow!("Missing tag key"))?;
    let tagtype = build_tag_type(type_pair)?;
    
    let label_pair = inner.next().ok_or_else(|| anyhow!("Missing tag label"))?;
    let label = build_label(label_pair)?;
    Ok(QueryNode::TypedTag(TypedTag::new(tagtype, label)))
}

fn build_tag_type(pair: Pair<Rule>) -> Result<TagType> {
    // tag_type = { quoted_string | identifier }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::quoted_string => {
            let s = unescape_string(inner.as_str())?;
            Ok(TagType::LiteralCustom(s))
        }
        Rule::identifier => {
            let s = unescape_unquoted(inner.as_str())?;
            Ok(TagType::from(s))
        }
        _ => Err(anyhow!("Unexpected tag_type rule: {:?}", inner.as_rule())),
    }
}

fn build_comparison(pair: Pair<Rule>) -> Result<QueryNode> {
    // comparison = { operand ~ (cmp_op ~ operand)+ }
    // AST Node: ComparisonNode { first: Operand, rest: Vec<(ComparisonOp, Operand)> }
    let mut inner = pair.into_inner();
    let first_op = build_operand(inner.next().unwrap())?;
    let mut rest = Vec::new();

    while let Some(op_pair) = inner.next() {
        let op = match op_pair.as_str() {
            "==" => ComparisonOp::Eq,
            "^=" | "^" => ComparisonOp::Ne, // ^ and ^= are NotEqual
            ">" => ComparisonOp::Gt,
            ">=" => ComparisonOp::Ge,
            "<" => ComparisonOp::Lt,
            "<=" => ComparisonOp::Le,
            s => return Err(anyhow!("Unknown comparison op: {}", s)),
        };
        let right_pair = inner.next().ok_or_else(|| anyhow!("Missing comparison operand"))?;
        let right_op = build_operand(right_pair)?;
        rest.push((op, right_op));
    }
    Ok(QueryNode::Comparison(ComparisonNode { first: first_op, rest }))
}

fn build_operand(pair: Pair<Rule>) -> Result<Operand> {
    // operand = { calculation | type_ref | label }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::calculation => Ok(Operand::Calculation(Box::new(build_calculation(inner)?))),
        Rule::type_ref => {
            // type_ref = ${ tag_type ~ ":" }
            let inner_tag = inner.into_inner().next().unwrap();
            Ok(Operand::TypeRef(build_tag_type(inner_tag)?))
        }
        Rule::label => Ok(Operand::Literal(build_label(inner)?)),
        _ => Err(anyhow!("Unknown operand rule: {:?}", inner.as_rule())),
    }
}

fn build_calculation(pair: Pair<Rule>) -> Result<CalculationNode> {
    // calculation = { "(" ~ calculation_inner ~ ")" }
    // calculation_inner = { operand_calc ~ (arith_op ~ operand_calc)+ }
    // NOTE: This implementation only supports simple binary calculation for now or left-associative chain.
    // But CalculationNode definition is binary: left, op, right.
    // If the grammar allows chaining (A + B + C), current AST doesn't fully support clean chaining unless nested.
    // Grammar: operand_calc ~ (arith_op ~ operand_calc)+
    // For MVP, handling first binary op chain as nested.

    let inner_pair = pair.into_inner().next().unwrap(); // calculation_inner
    let mut pairs = inner_pair.into_inner();
    
    let first_pair = pairs.next().unwrap();
    let mut left = build_operand_calc(first_pair)?;

    while let Some(op_pair) = pairs.next() {
        let op = match op_pair.as_str() {
            "+" => ArithmeticOp::Add,
            "-" => ArithmeticOp::Sub,
            "*" => ArithmeticOp::Mul,
            "/" => ArithmeticOp::Div,
            "%" => ArithmeticOp::Mod,
            _ => return Err(anyhow!("Unknown arithmetic op")),
        };
        let right_pair = pairs.next().unwrap();
        let right = build_operand_calc(right_pair)?;
        
        // Nesting for left associativity: (left op right)
        // But `Operand::Calculation` holds `CalculationNode`.
        // We wrap the current `left` (which might be an Operand) into a new CalculationNode as needed?
        // Wait, CalculationNode { left: Operand, ... }.
        // If we have A + B + C -> (A + B) + C
        // left = A. right = B. new_node = Calc(A, +, B).
        // Next op: +. right = C.
        // We need 'left' to reference the previous result.
        // Operand has `Calculation(Box<CalculationNode>)`.
        
        left = Operand::Calculation(Box::new(CalculationNode {
            left,
            op,
            right,
        }));
    }

    // The result is an Operand. But we need to return CalculationNode?
    // Wait, build_calculation returns Result<CalculationNode>.
    // But my loop potentially wrapped everything in Operand::Calculation.
    // If left is Operand::Calculation(box node), verify content.
    match left {
        Operand::Calculation(node) => Ok(*node),
         // If there was no operation (just one operand), grammar says (op ~ operand)+ so at least one op?
         // No, grammar: operand_calc ~ (arith_op ~ operand_calc)+
         // Actually, if there is NO op, it's just an operand_calc.
         // But `calculation` rule implies it's a calculation. 
         // If "label" is passed as calculation, logic above handles it?
         // If only one operand and no ops, logic returns the operand. 
         // But we must return CalculationNode.
         // This implies we can't represent a single operand as CalculationNode plainly.
         // Or maybe we treat (A) as A + 0? No.
         // Assuming valid calculation always has op.
         _ => Err(anyhow!("Calculation must contain at least one operation")),
    }
}

fn build_operand_calc(pair: Pair<Rule>) -> Result<Operand> {
    // operand_calc = { type_ref | label | calculation }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::type_ref => {
            let s = inner.as_str();
            let key = s.trim_end_matches(':');
            Ok(Operand::TypeRef(TagType::from(key)))
        }
        Rule::label => Ok(Operand::Literal(build_label(inner)?)),
        Rule::calculation => Ok(Operand::Calculation(Box::new(build_calculation(inner)?))),
        _ => Err(anyhow!("Unknown operand_calc rule")),
    }
}

fn build_label(pair: Pair<Rule>) -> Result<Label> {
    // label = { quoted_string | number | unquoted_string }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::quoted_string => {
            let s = unescape_string(inner.as_str())?;
            Ok(Label::Literal(s))
        }
        Rule::number => {
            let i = inner.as_str().parse::<i64>()?;
            Ok(Label::Integer(i))
        }
        Rule::unquoted_string => {
            let s = unescape_unquoted(inner.as_str())?;
            Ok(Label::String(s))
        }
        _ => Err(anyhow!("Unknown label rule")),
    }
}

fn unescape_string(s: &str) -> Result<String> {
    // Remove outer quotes
    let content = &s[1..s.len()-1];
    let mut res = String::new();
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                res.push(next);
            } else {
                res.push('\\');
            }
        } else {
            res.push(c);
        }
    }
    Ok(res)
}

fn unescape_unquoted(s: &str) -> Result<String> {
    let mut res = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                // Convert \* to [*] for DuckDB GLOB
                match next {
                    '*' | '?' | '[' | ']' | '!' => {
                        res.push('[');
                        res.push(next);
                        res.push(']');
                    }
                    _ => res.push(next),
                }
            } else {
                res.push('\\');
            }
        } else {
            res.push(c);
        }
    }
    Ok(res)
}


/// 検索クエリの展開を行う抽象化単位。
pub trait QueryFunction: Send + Sync {
    /// この関数の名前（例: "directory", "filename"）
    fn name(&self) -> &str;
    /// タグを別のクエリ構造（QueryNode）へ展開します。
    fn expand(&self, label: &Label) -> QueryNode;
}

/// QueryFunction を管理するレジストリ。
pub struct QueryFunctionRegistry {
    functions: HashMap<String, Box<dyn QueryFunction>>,
}

impl Default for QueryFunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryFunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
        }
    }

    /// 標準的なクエリ展開関数を登録済みのレジストリを返します。
    pub fn with_standard() -> Self {
        use crate::query_functions::*;
        let mut reg = Self::new();
        reg.register(Box::new(DirectoryQuery));
        reg.register(Box::new(FilenameQuery));
        reg.register(Box::new(ExtensionQuery));
        reg.register(Box::new(PathQuery));
        reg.register(Box::new(ParentDirQuery));
        reg.register(Box::new(NameQuery));
        reg.register(Box::new(ItemKindQuery));
        reg.register(Box::new(RankQuery));
        reg.register(Box::new(SizeQuery));
        reg.register(Box::new(MtimeQuery));
        reg.register(Box::new(OriginQuery));
        reg
    }

    pub fn register(&mut self, func: Box<dyn QueryFunction>) {
        self.functions.insert(func.name().to_string(), func);
    }

    /// タグを検索し、登録された関数があれば適用します。
    pub fn process_tag(&self, tagtype: TagType, label: Label) -> QueryNode {
        // LiteralCustom の場合は魔法（展開関数）をスキップする
        if let TagType::LiteralCustom(_) = tagtype {
            return QueryNode::And(vec![
                QueryNode::TypedTag(TypedTag { tagtype, label })
            ]);
        }

        // Baseタグ（SType）であれば、レジストリから展開関数を探す
        if let TagType::Base(stag) = tagtype {
            let key_str: &'static str = stag.into();
            if let Some(f) = self.functions.get(key_str) {
                return f.expand(&label);
            }
        }

        // それ以外（カスタムタグまたは未登録の標準タグ）はそのまま TypedTag として保持
        QueryNode::TypedTag(TypedTag { tagtype, label })
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ComparisonOp {
    Eq, Ne, Gt, Ge, Lt, Le
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ArithmeticOp {
    Add, Sub, Mul, Div, Mod
}

#[derive(Debug, PartialEq, Clone)]
pub enum Operand {
    Literal(Label),
    TypeRef(TagType),
    Calculation(Box<CalculationNode>),
}

#[derive(Debug, PartialEq, Clone)]
pub struct CalculationNode {
    pub left: Operand,
    pub op: ArithmeticOp,
    pub right: Operand,
}

#[derive(Debug, PartialEq, Clone)]
pub struct ComparisonNode {
    pub first: Operand,
    pub rest: Vec<(ComparisonOp, Operand)>,
}

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
}

impl QueryNode {
    /// 特殊なタグ（QueryFunction）を基本構造へ展開します。
    pub fn expand(self, registry: &QueryFunctionRegistry) -> QueryNode {
        match self {
            QueryNode::And(nodes) => {
                QueryNode::And(nodes.into_iter()
                    .map(|n| n.expand(registry))
                    .collect())
            }
            QueryNode::Or(nodes) => {
                QueryNode::Or(nodes.into_iter()
                    .map(|n| n.expand(registry))
                    .collect())
            }
            QueryNode::Difference(l, r) => {
                QueryNode::Difference(
                    Box::new(l.expand(registry)),
                    Box::new(r.expand(registry))
                )
            }
            QueryNode::Complement(c) => {
                QueryNode::Complement(Box::new(c.expand(registry)))
            }
            QueryNode::Comparison(cmp) => QueryNode::Comparison(cmp),
            QueryNode::ColumnMatch { tag, label } => {
                QueryNode::ColumnMatch { tag, label }
            }
            QueryNode::TypedTag(tt) => {
                registry.process_tag(tt.tagtype, tt.label)
            }
        }
    }

    /// クエリ構造を SQL (SelectStatement) へ変換します。
    pub fn to_sql(&self, view_name: &str) -> SelectStatement {
        match self {
            QueryNode::And(nodes) => {
                self.build_and_sql(nodes, view_name)
            }
            QueryNode::Or(nodes) => {
                self.build_or_sql(nodes, view_name)
            }
            QueryNode::Difference(l, r) => {
                self.build_diff_sql(l, r, view_name)
            }
            QueryNode::Complement(c) => {
                self.build_comp_sql(c, view_name)
            }
            QueryNode::Comparison(_cmp) => {
                // Scope Restriction: ラベル比較・計算は本フェーズでは対象外
                unimplemented!("Comparison logic is deferred to next phase");
            }
            QueryNode::ColumnMatch { tag, label } => {
                self.build_column_match_sql(*tag, label, view_name)
            }
            QueryNode::TypedTag(tt) => {
                self.build_typed_tag_sql(&tt.tagtype, &tt.label, view_name)
            }
        }
    }

    fn build_and_sql(
        &self,
        nodes: &[QueryNode],
        view: &str,
    ) -> SelectStatement {
        let mut it = nodes.iter();
        let first = it.next().expect("And nodes must not be empty");
        // Precedence Safety: Wrap children in subqueries to enforce (A | B) & C logic
        let mut q = self.wrap_in_subquery(first.to_sql(view));
        for node in it {
            q.union(sea_query::UnionType::Intersect, self.wrap_in_subquery(node.to_sql(view)));
        }
        q
    }

    fn build_or_sql(
        &self,
        nodes: &[QueryNode],
        view: &str,
    ) -> SelectStatement {
        let mut it = nodes.iter();
        let first = it.next().expect("Or nodes must not be empty");
        let mut q = first.to_sql(view);
        for node in it {
            q.union(sea_query::UnionType::Distinct, node.to_sql(view));
        }
        q
    }

    fn build_diff_sql(
        &self,
        l: &QueryNode,
        r: &QueryNode,
        view: &str,
    ) -> SelectStatement {
        let mut q = self.wrap_in_subquery(l.to_sql(view));
        q.union(sea_query::UnionType::Except, self.wrap_in_subquery(r.to_sql(view)));
        q
    }

    fn wrap_in_subquery(&self, q: SelectStatement) -> SelectStatement {
        Query::select()
            .column(Col::ItemId)
            .from_subquery(q, Alias::new("sub"))
            .to_owned()
    }

    fn build_comp_sql(&self, c: &QueryNode, view: &str) -> SelectStatement {
        let types = c.get_all_types();
        let mut q = Query::select();
        q.column(Col::ItemId).distinct().from(Alias::new(view));
        if !types.is_empty() {
            q.and_where(Expr::col(Col::Type).is_in(types));
        }
        let mut eq = Query::select();
        eq.column(Col::ItemId)
            .from_subquery(c.to_sql(view), Tbl::NotSide);
        q.union(sea_query::UnionType::Except, eq);
        q
    }

    fn build_column_match_sql(
        &self,
        tag: SType,
        label: &Label,
        view: &str,
    ) -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId).distinct().from(Alias::new(view));
        match label {
            Label::Integer(i) => {
                q.and_where(Expr::col(tag).eq(*i));
            }
            Label::String(s) => {
                q.and_where(Expr::col(tag).binary(
                    BinOper::Custom("GLOB"),
                    Expr::val(s.as_str()),
                ));
            }
            Label::Literal(s) => {
                q.and_where(Expr::col(tag).eq(s.as_str()));
            }
        }
        q
    }

    fn build_typed_tag_sql(
        &self,
        tagtype: &TagType,
        label: &Label,
        view: &str,
    ) -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId).distinct().from(Alias::new(view));
        let glob = BinOper::Custom("GLOB");
        
        // Type side: LiteralCustom uses '='
        match tagtype {
            TagType::LiteralCustom(s) => {
                q.and_where(Expr::col(Col::Type).eq(s.as_str()));
            }
            _ => {
                q.and_where(Expr::col(Col::Type).binary(glob.clone(), Expr::val(tagtype.as_str())));
            }
        }

        match label {
            Label::Integer(i) => {
                q.and_where(Expr::col(Col::Label).eq(*i));
            }
            Label::String(s) => {
                q.and_where(Expr::col(Col::Label).binary(glob, Expr::val(s.as_str())));
            }
            Label::Literal(s) => {
                q.and_where(Expr::col(Col::Label).eq(s.as_str()));
            }
        }
        q
    }

    /// このノードおよび子ノードに含まれるすべてのタグの型（`key`）を収集します。
    pub fn get_all_types(&self) -> Vec<String> {
        let mut types = std::collections::HashSet::new();
        self.collect_types(&mut types);
        types.into_iter().collect()
    }

    fn collect_types(&self, types: &mut std::collections::HashSet<String>) {
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
        }
    }
}

// --- Type Collection Helpers ---

impl ComparisonNode {
    pub fn collect_types(&self, types: &mut std::collections::HashSet<String>) {
        self.first.collect_types(types);
        for (_, op) in &self.rest {
            op.collect_types(types);
        }
    }
}

impl Operand {
    pub fn collect_types(&self, types: &mut std::collections::HashSet<String>) {
        match self {
            Operand::Literal(_) => {}
            Operand::TypeRef(tag_type) => {
                types.insert(tag_type.as_str().to_string());
            }
            Operand::Calculation(calc) => {
                calc.collect_types(types);
            }
        }
    }
}

impl CalculationNode {
    pub fn collect_types(&self, types: &mut std::collections::HashSet<String>) {
        self.left.collect_types(types);
        self.right.collect_types(types);
    }
}

// --- Parsing Logic ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_types() {
        let node = parse("extension:rs").expect("Failed to parse");
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.tagtype.as_str(), "extension");
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
            assert_eq!(tt.tagtype.as_str(), "extension");
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
            "size: > 1024",
            "50 < width: < 100",
            "(size: + 1024) > 2048",
            "name:\"My File\" | name:'Other File'",
            "extension:pdf - filename:test.pdf",
        ];

        for q in queries {
            parse(q).map_err(|e| {
                panic!("Failed to parse query '{}': {}", q, e)
            }).unwrap();
        }
    }

    #[test]
    fn test_pest_grammar_strict_conformance() {
        // Test spaces (should fail according to DESIGN.md)
        let fail_queries = [
            "^ (extension:pdf)", // Space after ^
            "extension : rs",     // Space around :
            "size :> 100",      // Space before :>
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
        assert!(
            parse(q_unary).is_err(),
            "Unary minus should be invalid"
        );
    }

    #[test]
    fn test_pest_grammar_complex_math() {
        // Multi-level math and negative numbers
        let q = "(size: - -100) > (width: * (height: / 2))";
        parse(q).map_err(|e| {
            panic!("Failed to parse math query '{}': {}", q, e)
        }).unwrap();
    }
}
