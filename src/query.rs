use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::iter::Peekable;
use std::str::Chars;
use sea_query::{Alias, BinOper, Expr, Query, SelectStatement};
use crate::types::{Label, SType, TagType, TypedTag};
use crate::db::{Tbl, Col};
use pest_derive::Parser;
use pest::Parser;

#[derive(Parser)]
#[grammar = "query.pest"]
pub struct PestQueryParser;


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

/// 検索クエリの構造を表す抽象構文木（AST）ノード。
/// 論理演算（AND, OR, NOT）や検索語（単語、型付きタグ）を保持します。
#[derive(Debug, PartialEq, Clone)]
pub enum QueryNode {
    /// AND条件 (`A & B` または `A B`)
    And(Box<QueryNode>, Box<QueryNode>),
    /// OR条件 (`A | B`)
    Or(Box<QueryNode>, Box<QueryNode>),
    /// NOT条件 (`-A`)
    Not(Box<QueryNode>),
    /// 物理カラムに対する検索 (rank, size, mtime, name, id 等)
    ColumnMatch { tag: SType, label: Label },
    /// 汎用タグ検索 (TypedTag 型を使用)
    TypedTag(TypedTag),
}

impl QueryNode {
    /// 特殊なタグ（QueryFunction）を基本構造へ展開します。
    pub fn expand(self, registry: &QueryFunctionRegistry) -> QueryNode {
        match self {
            QueryNode::And(l, r) => QueryNode::And(
                Box::new(l.expand(registry)),
                Box::new(r.expand(registry)),
            ),
            QueryNode::Or(l, r) => QueryNode::Or(
                Box::new(l.expand(registry)),
                Box::new(r.expand(registry)),
            ),
            QueryNode::Not(c) => QueryNode::Not(Box::new(c.expand(registry))),
            QueryNode::ColumnMatch { tag, label } => {
                QueryNode::ColumnMatch { tag, label }
            }
            QueryNode::TypedTag(tt) => registry.process_tag(tt.tagtype, tt.label),
        }
    }

    /// クエリ構造を SQL (SelectStatement) へ変換します。
    pub fn to_sql(&self, view_name: &str) -> SelectStatement {
        match self {
            QueryNode::And(l, r) => self.build_and_sql(l, r, view_name),
            QueryNode::Or(l, r) => self.build_or_sql(l, r, view_name),
            QueryNode::Not(c) => self.build_not_sql(c, view_name),
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
        l: &QueryNode,
        r: &QueryNode,
        view: &str,
    ) -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId)
            .from_subquery(l.to_sql(view), Tbl::LeftSide);
        let mut rq = Query::select();
        rq.column(Col::ItemId)
            .from_subquery(r.to_sql(view), Tbl::RightSide);
        q.union(sea_query::UnionType::Intersect, rq);
        q
    }

    fn build_or_sql(
        &self,
        l: &QueryNode,
        r: &QueryNode,
        view: &str,
    ) -> SelectStatement {
        let mut q = Query::select();
        q.column(Col::ItemId)
            .from_subquery(l.to_sql(view), Tbl::LeftSide);
        let mut rq = Query::select();
        rq.column(Col::ItemId)
            .from_subquery(r.to_sql(view), Tbl::RightSide);
        q.union(sea_query::UnionType::Distinct, rq);
        q
    }

    fn build_not_sql(&self, c: &QueryNode, view: &str) -> SelectStatement {
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
        q.and_where(Expr::col(Col::Type).binary(glob.clone(), Expr::val(tagtype.as_str())));
        match label {
            Label::Integer(i) => {
                q.and_where(Expr::col(Col::Label).eq(*i));
            }
            Label::String(s) => {
                q.and_where(Expr::col(Col::Label).binary(glob, Expr::val(s.as_str())));
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
            QueryNode::And(l, r) => {
                l.collect_types(types);
                r.collect_types(types);
            }
            QueryNode::Or(l, r) => {
                l.collect_types(types);
                r.collect_types(types);
            }
            QueryNode::Not(c) => {
                c.collect_types(types);
            }
            QueryNode::ColumnMatch { .. } => {}
            QueryNode::TypedTag(tt) => {
                types.insert(tt.tagtype.as_str().to_string());
            }
        }
    }
}

// --- Parsing Logic ---

/// クエリ文字列を解析し、抽象構文木（AST）を構築する再帰下降パーサ。
pub struct QueryParser<'a> {
    chars: Peekable<Chars<'a>>,
}

impl<'a> QueryParser<'a> {
    /// 検索クエリ文字列を解析して `QueryNode` を返します。
    pub fn parse(input: &'a str) -> Result<QueryNode> {
        let mut parser = QueryParser {
            chars: input.chars().peekable(),
        };
        let node = parser.parse_expression()?;
        parser.skip_whitespace();
        if parser.chars.peek().is_some() {
            return Err(anyhow::anyhow!("Unexpected characters at end of query"));
        }
        Ok(node)
    }

    /// 式（Expression）を解析します。主にOR演算を処理します。
    fn parse_expression(&mut self) -> Result<QueryNode> {
        let mut left = self.parse_and_term()?;
        loop {
            self.skip_whitespace();
            if let Some(&'|') = self.chars.peek() {
                self.chars.next();
                let right = self.parse_and_term()?;
                left = QueryNode::Or(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// AND項を解析します。`&` 演算子または暗黙のANDを処理します。
    fn parse_and_term(&mut self) -> Result<QueryNode> {
        let mut left = self.parse_factor()?;
        loop {
            self.skip_whitespace();
            if let Some(&c) = self.chars.peek() {
                if c == '|' || c == ')' {
                    break;
                }
                if c == '&' {
                    self.chars.next();
                    let right = self.parse_factor()?;
                    left = QueryNode::And(Box::new(left), Box::new(right));
                } else {
                    // 暗黙の AND (スペース等)
                    if let Ok(right) = self.parse_factor() {
                        left = QueryNode::And(Box::new(left), Box::new(right));
                    } else {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// 因子（括弧、NOT、リテラル）を解析します。
    fn parse_factor(&mut self) -> Result<QueryNode> {
        self.skip_whitespace();
        match self.chars.peek() {
            Some(&'(') => {
                self.chars.next();
                let node = self.parse_expression()?;
                self.skip_whitespace();
                if let Some(&')') = self.chars.peek() {
                    self.chars.next();
                    Ok(node)
                } else {
                    Err(anyhow::anyhow!("Missing closing parenthesis"))
                }
            }
            Some(&'-') => {
                self.chars.next();
                let node = self.parse_factor()?;
                Ok(QueryNode::Not(Box::new(node)))
            }
            Some(_) => {
                // key:value 形式の解析
                let key = self.read_string_until(':')?;
                if self.chars.peek() == Some(&':') {
                    self.chars.next();
                    let value = self.read_value()?;
                    Ok(QueryNode::TypedTag(TypedTag::new(TagType::from(key), value)))
                } else {
                    Err(anyhow::anyhow!("Expected ':' after tag key '{}'", key))
                }
            }
            None => Err(anyhow::anyhow!("Unexpected end of input")),
        }
    }

    /// 通常の文字列、またはクォートされた文字列を読み込み、Label を返します。
    fn read_value(&mut self) -> Result<Label> {
        self.skip_whitespace();
        if let Some(&'"') = self.chars.peek() {
            self.chars.next();
            let mut s = String::new();
            while let Some(&c) = self.chars.peek() {
                if c == '"' {
                    self.chars.next();
                    return Ok(Label::String(s));
                }
                s.push(c);
                self.chars.next();
            }
            Err(anyhow::anyhow!("Unclosed double quote"))
        } else {
            let mut s = String::new();
            while let Some(&c) = self.chars.peek() {
                if c == '&' || c == '|' || c == '(' || c == ')' || c.is_whitespace() {
                    break;
                }
                s.push(c);
                self.chars.next();
            }
            if s.is_empty() {
                Err(anyhow::anyhow!("Empty value"))
            } else if let Ok(i) = s.parse::<i64>() {
                Ok(Label::Integer(i))
            } else {
                Ok(Label::String(s))
            }
        }
    }

    /// 指定された文字が現れるまで文字列を読み込みます（キーの解析用）。
    fn read_string_until(&mut self, delimiter: char) -> Result<String> {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c == delimiter || c == '&' || c == '|' || c == '(' || c == ')' || c.is_whitespace() {
                break;
            }
            s.push(c);
            self.chars.next();
        }
        if s.is_empty() {
            Err(anyhow::anyhow!("Empty identifier"))
        } else {
            Ok(s)
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() {
                self.chars.next();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_types() {
        let node = QueryNode::TypedTag(TypedTag::new("extension", Label::String("rs".into())));
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.tagtype.as_str(), "extension");
            assert_eq!(tt.label.as_str(), "rs");
        } else {
            panic!("Should be a TypedTag");
        }
    }

    #[test]
    fn test_case_preservation() {
        let node = QueryParser::parse("Extension:RS").unwrap();
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.tagtype.as_str(), "Extension");
            assert_eq!(tt.label.as_str(), "RS");
            assert!(matches!(tt.label, Label::String(_)));
        } else {
            panic!("Should be a TypedTag");
        }
    }

    #[test]
    fn test_numeric_inference() {
        let node = QueryParser::parse("size:1024").unwrap();
        if let QueryNode::TypedTag(tt) = node {
            assert!(matches!(tt.label, Label::Integer(1024)));
        } else {
            panic!("Should be a TypedTag");
        }
    }

    #[test]
    fn test_quoted_value_with_special_chars() {
        let input = "file_id:\"Inode { device_id: 2096 }\"";
        let node = QueryParser::parse(input).unwrap();
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.tagtype.as_str(), "file_id");
            assert_eq!(tt.label.as_str(), "Inode { device_id: 2096 }");
        } else {
            panic!("Should be a TypedTag");
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
            PestQueryParser::parse(Rule::query, q)
                .map_err(|e| {
                    panic!("Failed to parse query '{}': {}", q, e)
                })
                .unwrap();
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
                PestQueryParser::parse(Rule::query, q).is_err(),
                "Query '{}' should fail due to space constraints",
                q
            );
        }

        // Test unary minus (should fail according to DESIGN.md)
        let q_unary = "-type:file";
        assert!(
            PestQueryParser::parse(Rule::query, q_unary).is_err(),
            "Unary minus should be invalid"
        );
    }

    #[test]
    fn test_pest_grammar_complex_math() {
        // Multi-level math and negative numbers
        let q = "(size: - -100) > (width: * (height: / 2))";
        PestQueryParser::parse(Rule::query, q)
            .map_err(|e| {
                panic!("Failed to parse math query '{}': {}", q, e)
            })
            .unwrap();
    }
}
