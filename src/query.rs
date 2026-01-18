use crate::db::{Col, Tbl};
use crate::types::{Label, SType, TagType, TypedTag};
use crate::util::DotOk;
use anyhow::{anyhow, Result};
use pest::Parser;
use pest_derive::Parser;
use sea_query::{Alias, BinOper, Condition, Expr, Query, SelectStatement};
use std::collections::{HashMap, VecDeque};

#[derive(Parser)]
#[grammar = "query.pest"]
pub struct PestQueryParser;

use pest::iterators::Pair;
use pest::pratt_parser::{Assoc, Op, PrattParser};
use std::sync::OnceLock;

static PRATT_PARSER: OnceLock<PrattParser<Rule>> = OnceLock::new();

fn get_parser() -> &'static PrattParser<Rule> {
    PRATT_PARSER.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::pipe, Assoc::Left)
                | Op::infix(Rule::minus, Assoc::Left))
            .op(Op::infix(Rule::ampersand, Assoc::Left))
    })
}

/// クエリ文字列を解析し、QueryNode AST を構築します。
pub fn parse(input: &str) -> Result<QueryNode> {
    let mut pairs = PestQueryParser::parse(Rule::query, input)
        .map_err(|e| anyhow!("Parse error: {}", e))?;
    let expr_pair = pairs
        .next()
        .ok_or_else(|| anyhow!("No query found"))?
        .into_inner()
        .next()
        .ok_or_else(|| anyhow!("No expression found"))?;
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
                        Rule::minus => Ok(QueryNode::Difference(
                            Box::new(lhs),
                            Box::new(rhs),
                        )),
                        _ => Err(anyhow!(
                            "Unknown infix rule: {:?}",
                            op.as_rule()
                        )),
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
                Rule::projection => build_projection(inner),
                _ => {
                    Err(anyhow!("Unknown factor inner: {:?}", inner.as_rule()))
                }
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
            let expr_pair = inner
                .next()
                .ok_or_else(|| anyhow!("Complement missing expr"))?;
            Ok(QueryNode::Complement(Box::new(build_ast(expr_pair)?)))
        }
        _ => Err(anyhow!(
            "Unexpected rule in build_ast: {:?}",
            pair.as_rule()
        )),
    }
}

fn build_typed_tag(pair: Pair<Rule>) -> Result<QueryNode> {
    // typed_tag = ${ tag_type ~ ":" ~ label }
    let mut inner = pair.into_inner();
    let type_pair = inner.next().ok_or_else(|| anyhow!("Missing tag key"))?;
    let tagtype = build_tag_type(type_pair)?;

    let label_pair =
        inner.next().ok_or_else(|| anyhow!("Missing tag label"))?;
    let label = build_tag_label(label_pair)?;
    Ok(QueryNode::TypedTag(TypedTag::new(tagtype, label)))
}

fn build_projection(pair: Pair<Rule>) -> Result<QueryNode> {
    // projection = { type_ref }
    // type_ref = ${ tag_type ~ ":" }
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| anyhow!("Missing projection inner"))?;
    let mut type_ref_inner = inner.into_inner();
    let type_pair = type_ref_inner
        .next()
        .ok_or_else(|| anyhow!("Missing tag key in projection"))?;
    let tagtype = build_tag_type(type_pair)?;
    Ok(QueryNode::Projection(tagtype))
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
        let right_pair = inner
            .next()
            .ok_or_else(|| anyhow!("Missing comparison operand"))?;
        let right_op = build_operand(right_pair)?;
        rest.push((op, right_op));
    }
    Ok(QueryNode::Comparison(ComparisonNode {
        first: first_op,
        rest,
    }))
}

fn build_operand(pair: Pair<Rule>) -> Result<Operand> {
    // operand = { calculation | type_ref | label }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::calculation => {
            Ok(Operand::Calculation(Box::new(build_calculation(inner)?)))
        }
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

        left =
            Operand::Calculation(Box::new(CalculationNode { left, op, right }));
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
        Rule::calculation => {
            Ok(Operand::Calculation(Box::new(build_calculation(inner)?)))
        }
        _ => Err(anyhow!("Unknown operand_calc rule")),
    }
}

fn build_label(pair: Pair<Rule>) -> Result<Label> {
    // label = { quoted_string | number | unquoted_string }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::quoted_string => {
            // Remove outer quotes before unescaping
            let content = &inner.as_str()[1..inner.as_str().len() - 1];
            let s = unescape_string(content)?;
            Label::Literal(s).to_ok()
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

fn build_tag_label(pair: Pair<Rule>) -> Result<Label> {
    // tag_label = { quoted_string | number | unquoted_tag_string }
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::quoted_string => {
            let content = &inner.as_str()[1..inner.as_str().len() - 1];
            let s = unescape_string(content)?;
            Label::Literal(s).to_ok()
        }
        Rule::number => {
            let i = inner.as_str().parse::<i64>()?;
            Ok(Label::Integer(i))
        }
        Rule::unquoted_tag_string => {
            let s = unescape_unquoted(inner.as_str())?;
            Ok(Label::String(s))
        }
        _ => Err(anyhow!("Unknown tag_label rule: {:?}", inner.as_rule())),
    }
}

fn unescape_string(s: &str) -> Result<String> {
    let mut chars = s.chars();
    std::iter::from_fn(move || match chars.next()? {
        '\\' => match chars.next() {
            Some('n') => Some('\n'),
            Some('r') => Some('\r'),
            Some('t') => Some('\t'),
            Some('\\') => Some('\\'),
            Some('\'') => Some('\''),
            Some('"') => Some('"'),
            Some(c) => Some(c),
            None => Some('\\'),
        },
        c => Some(c),
    })
    .collect::<String>()
    .to_ok()
}

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

/// 検索クエリの展開を行う抽象化単位。
pub trait QueryFunction: Send + Sync {
    /// この関数の名前（例: "directory", "filename"）
    fn name(&self) -> &str;
    /// タグを別のクエリ構造（QueryNode）へ展開します。
    fn expand(&self, label: &Label) -> QueryNode;

    /// ラベルの値を正規化します（例: "1MB" -> 1048576）。
    /// デフォルトでは元のラベルをそのまま返します。
    fn normalize_label(&self, label: &Label) -> Label {
        label.clone()
    }

    /// ラベル取得を基本構造へ展開します。
    fn expand_projection(&self, _tagtype: TagType) -> QueryNode {
        QueryNode::Projection(_tagtype)
    }
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
            return QueryNode::And(vec![QueryNode::TypedTag(TypedTag {
                tagtype,
                label,
            })]);
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

    /// 指定された TagType に対応する QueryFunction を返します。
    pub fn get_function(
        &self,
        tagtype: &TagType,
    ) -> Option<&dyn QueryFunction> {
        if let TagType::Base(stag) = tagtype {
            let key_str: &'static str = (*stag).into();
            return self.functions.get(key_str).map(|f| f.as_ref());
        }
        None
    }

    pub fn expand_projection(&self, tagtype: TagType) -> QueryNode {
        // LiteralCustom の場合は魔法（展開関数）をスキップする
        if let TagType::LiteralCustom(_) = tagtype {
            return QueryNode::Projection(tagtype);
        }

        // Baseタグ（SType）であれば、レジストリから展開関数を探す
        if let TagType::Base(stag) = tagtype {
            let key_str: &'static str = stag.into();
            if let Some(f) = self.functions.get(key_str) {
                return f.expand_projection(tagtype);
            }
        }

        // それ以外（カスタムタグまたは未登録の標準タグ）はそのまま Projection として保持
        QueryNode::Projection(tagtype)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ComparisonOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ArithmeticOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
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

impl ComparisonNode {
    /// 比較演算内のリテラルを、関係するタグの QueryFunction に従って正規化します。
    pub fn expand(mut self, registry: &QueryFunctionRegistry) -> Self {
        // この比較に関係する代表的な QueryFunction を探す
        let mut rep_func = None;

        if let Operand::TypeRef(tt) = &self.first {
            rep_func = registry.get_function(tt);
        }

        if rep_func.is_none() {
            for (_, op) in &self.rest {
                if let Operand::TypeRef(tt) = op {
                    if let Some(f) = registry.get_function(tt) {
                        rep_func = Some(f);
                        break;
                    }
                }
            }
        }

        // 関数が見つかった場合、すべてのリテラルオペランドを正規化する
        if let Some(f) = rep_func {
            if let Operand::Literal(label) = &mut self.first {
                *label = f.normalize_label(label);
            }
            for (_, op) in &mut self.rest {
                if let Operand::Literal(label) = op {
                    *label = f.normalize_label(label);
                }
            }
        }

        self
    }
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
    /// ラベル取得 (Projection)
    Projection(TagType),
}

impl QueryNode {
    /// 特殊なタグ（QueryFunction）を基本構造へ展開します。
    pub fn expand(self, registry: &QueryFunctionRegistry) -> QueryNode {
        match self {
            QueryNode::And(nodes) => QueryNode::And(
                nodes.into_iter().map(|n| n.expand(registry)).collect(),
            ),
            QueryNode::Or(nodes) => QueryNode::Or(
                nodes.into_iter().map(|n| n.expand(registry)).collect(),
            ),
            QueryNode::Difference(l, r) => QueryNode::Difference(
                Box::new(l.expand(registry)),
                Box::new(r.expand(registry)),
            ),
            QueryNode::Complement(c) => {
                QueryNode::Complement(Box::new(c.expand(registry)))
            }
            QueryNode::Comparison(cmp) => {
                QueryNode::Comparison(cmp.expand(registry))
            }
            QueryNode::ColumnMatch { tag, label } => {
                QueryNode::ColumnMatch { tag, label }
            }
            QueryNode::TypedTag(tt) => {
                registry.process_tag(tt.tagtype, tt.label)
            }
            QueryNode::Projection(tt) => registry.expand_projection(tt),
        }
    }

    /// クエリ構造を SQL (SelectStatement) へ変換します。
    pub fn to_sql(&self, view_name: &str) -> SelectStatement {
        match self {
            QueryNode::And(nodes) => self.build_and_sql(nodes, view_name),
            QueryNode::Or(nodes) => self.build_or_sql(nodes, view_name),
            QueryNode::Difference(l, r) => self.build_diff_sql(l, r, view_name),
            QueryNode::Complement(c) => self.build_comp_sql(c, view_name),
            QueryNode::Comparison(cmp) => {
                self.build_comparison_sql(cmp, view_name)
            }
            QueryNode::ColumnMatch { tag, label } => {
                self.build_column_match_sql(*tag, label, view_name)
            }
            QueryNode::TypedTag(tt) => {
                self.build_typed_tag_sql(&tt.tagtype, &tt.label, view_name)
            }
            QueryNode::Projection(tt) => {
                self.build_projection_sql(&tt, view_name)
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
            q.union(
                sea_query::UnionType::Intersect,
                self.wrap_in_subquery(node.to_sql(view)),
            );
        }
        q
    }

    fn build_or_sql(&self, nodes: &[QueryNode], view: &str) -> SelectStatement {
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
        q.union(
            sea_query::UnionType::Except,
            self.wrap_in_subquery(r.to_sql(view)),
        );
        q
    }

    fn wrap_in_subquery(&self, q: SelectStatement) -> SelectStatement {
        Query::select()
            .column(Col::ItemId)
            .from_subquery(q, Tbl::Sub)
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

    fn build_comparison_sql(
        &self,
        node: &ComparisonNode,
        view: &str,
    ) -> SelectStatement {
        let mut operands = vec![&node.first];
        for (_, opd) in &node.rest {
            operands.push(opd);
        }

        let mut subqueries = Vec::new();
        for (i, (op, _)) in node.rest.iter().enumerate() {
            let left = operands[i];
            let right = operands[i + 1];
            subqueries
                .push(self.build_binary_comparison_sql(left, *op, right, view));
        }

        if subqueries.len() == 1 {
            subqueries.pop().unwrap()
        } else {
            let mut first = subqueries.remove(0);
            for next in subqueries {
                first.union(sea_query::UnionType::Intersect, next);
            }
            first
        }
    }

    fn build_binary_comparison_sql(
        &self,
        left: &Operand,
        op: ComparisonOp,
        right: &Operand,
        view: &str,
    ) -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId).distinct().from(Alias::new(view));

        let bin_op = self.to_bin_op(op);

        // オペランドを (TagType, Label, 補正済み演算子) の形に正規化
        let (tt, lab, effective_op) =
            match self.normalize_comparison(left, bin_op, right) {
                Some(res) => res,
                None => {
                    q.and_where(Expr::val(1).eq(0));
                    return q;
                }
            };

        // 特権カラム（物理カラム）への最適化パスは廃止。
        // OneView は全てのメタデータを EAV (type, label) 形式で提供するため、
        // 物理カラムへの直接アクセスは行わない。
        // すべて apply_generic_comparison にフォールバックさせる。

        // 一般的なタグへのフォールバックパス
        self.apply_generic_comparison(q, tt, effective_op, lab)
    }

    fn to_bin_op(&self, op: ComparisonOp) -> BinOper {
        match op {
            ComparisonOp::Eq => BinOper::Equal,
            ComparisonOp::Ne => BinOper::NotEqual,
            ComparisonOp::Gt => BinOper::GreaterThan,
            ComparisonOp::Ge => BinOper::GreaterThanOrEqual,
            ComparisonOp::Lt => BinOper::SmallerThan,
            ComparisonOp::Le => BinOper::SmallerThanOrEqual,
        }
    }

    fn flip_bin_op(&self, op: BinOper) -> BinOper {
        match op {
            BinOper::GreaterThan => BinOper::SmallerThan,
            BinOper::GreaterThanOrEqual => BinOper::SmallerThanOrEqual,
            BinOper::SmallerThan => BinOper::GreaterThan,
            BinOper::SmallerThanOrEqual => BinOper::GreaterThanOrEqual,
            other => other,
        }
    }

    fn normalize_comparison(
        &self,
        left: &Operand,
        op: BinOper,
        right: &Operand,
    ) -> Option<(TagType, Label, BinOper)> {
        match (left, right) {
            (Operand::TypeRef(tt), Operand::Literal(lab)) => {
                Some((tt.clone(), lab.clone(), op))
            }
            (Operand::Literal(lab), Operand::TypeRef(tt)) => {
                Some((tt.clone(), lab.clone(), self.flip_bin_op(op)))
            }
            _ => None,
        }
    }

    fn apply_generic_comparison(
        &self,
        mut q: SelectStatement,
        tagtype: TagType,
        op: BinOper,
        label: Label,
    ) -> SelectStatement {
        let mut condition = Condition::any();
        match label {
            Label::Integer(i) => {
                condition = condition
                    .add(Expr::col(Col::LabelInt).binary(op, Expr::val(i)))
                    .add(Expr::col(Col::LabelDouble).binary(op, Expr::val(i)));
            }
            Label::String(s) | Label::Literal(s) => {
                condition = condition.add(
                    Expr::col(Col::LabelStr).binary(op, Expr::val(s.as_str())),
                );

                // もし文字列が数値やブーリアンとして解釈可能なら、それらのカラムも対象にする
                if let Ok(i) = s.parse::<i64>() {
                    condition = condition
                        .add(Expr::col(Col::LabelInt).binary(op, Expr::val(i)))
                        .add(
                            Expr::col(Col::LabelDouble)
                                .binary(op, Expr::val(i)),
                        );
                } else if let Ok(f) = s.parse::<f64>() {
                    condition = condition.add(
                        Expr::col(Col::LabelDouble).binary(op, Expr::val(f)),
                    );
                } else if s == "true" || s == "false" {
                    let b = s == "true";
                    condition = condition.add(
                        Expr::col(Col::LabelBool).binary(op, Expr::val(b)),
                    );
                }
            }
        };

        q.and_where(Expr::col(Col::Type).eq(tagtype.as_str()))
            .and_where(condition.into());
        q
    }

    fn build_projection_sql(
        &self,
        tagtype: &TagType,
        view: &str,
    ) -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId).distinct().from(Alias::new(view));

        q.and_where(Expr::col(Col::Type).eq(tagtype.as_str()));

        // ラベル値が NULL でないことを確認する。
        // oneview は物理カラム（extension, path等）を unpivot するため、
        // 値がなくても行自体は存在する可能性がある。
        let mut cond = Condition::any();
        cond = cond.add(Expr::col(Col::LabelStr).is_not_null());
        cond = cond.add(Expr::col(Col::LabelInt).is_not_null());
        cond = cond.add(Expr::col(Col::LabelDouble).is_not_null());
        cond = cond.add(Expr::col(Col::LabelBool).is_not_null());

        q.and_where(cond.into());
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

        // Logic handles SType::Label redirection internally

        match label {
            Label::Integer(i) => {
                let t = if matches!(tag, SType::Label) {
                    Col::LabelInt.into()
                } else {
                    tag
                };
                q.and_where(Expr::col(t).eq(*i));
            }
            Label::String(s) => {
                let t = if matches!(tag, SType::Label) {
                    Col::LabelStr.into()
                } else {
                    tag
                };
                q.and_where(
                    Expr::col(t)
                        .binary(BinOper::Custom("GLOB"), Expr::val(s.as_str())),
                );
            }
            Label::Literal(s) => {
                let t = if matches!(tag, SType::Label) {
                    Col::LabelStr.into()
                } else {
                    tag
                };
                q.and_where(Expr::col(t).eq(s.as_str()));
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
                q.and_where(
                    Expr::col(Col::Type)
                        .binary(glob.clone(), Expr::val(tagtype.as_str())),
                );
            }
        }

        let mut cond = Condition::any();
        match label {
            Label::Integer(i) => {
                cond = cond
                    .add(Expr::col(Col::LabelInt).eq(*i))
                    .add(Expr::col(Col::LabelDouble).eq(*i as f64));
            }
            Label::String(s) => {
                let val_str = if s.starts_with('^') {
                    format!("{}*", &s[1..])
                } else {
                    s.clone()
                };

                cond = cond.add(
                    Expr::col(Col::LabelStr).binary(glob, Expr::val(val_str)),
                );
                // 数値やブーリアンとしてもチェック
                if let Ok(i) = s.parse::<i64>() {
                    cond = cond.add(Expr::col(Col::LabelInt).eq(i));
                }
                if s == "true" || s == "false" {
                    cond = cond.add(Expr::col(Col::LabelBool).eq(s == "true"));
                }
            }
            Label::Literal(s) => {
                cond = cond.add(Expr::col(Col::LabelStr).eq(s.as_str()));
                if let Ok(i) = s.parse::<i64>() {
                    cond = cond.add(Expr::col(Col::LabelInt).eq(i));
                }
                if s == "true" || s == "false" {
                    cond = cond.add(Expr::col(Col::LabelBool).eq(s == "true"));
                }
            }
        }
        q.and_where(cond.into());
        q
    }

    /// このノードおよび子ノードに含まれるすべてのタグの型（`key`）を収集します。
    pub fn get_all_types(&self) -> Vec<String> {
        let mut types = std::collections::HashSet::new();
        self.collect_types(&mut types);
        types.into_iter().collect()
    }

    /// このクエリで投影（Projection）されているタグ型の一覧を取得します。
    pub fn get_projections(&self) -> Vec<String> {
        let mut projections = std::collections::HashSet::new();
        self.collect_projections(&mut projections);
        projections.into_iter().collect()
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
            QueryNode::Projection(tt) => {
                types.insert(tt.as_str().to_string());
            }
        }
    }

    fn collect_projections(
        &self,
        projections: &mut std::collections::HashSet<String>,
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
            "size: >= 1024",
            "size: < 2048",
            "size: <= 2048",
            "rank: == 5",
            "rank: ^= 1", // Not Equal
            "50 < width: < 100",
            "10 <= height: <= 20",
            "(size: + 1024) > 2048",
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
}
