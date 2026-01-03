use anyhow::Result;
use std::iter::Peekable;
use std::str::Chars;
use crate::types::{Tag, TagType, TypedTag};
use crate::functions::{ExtensionFunction, ParentDirFunction, PathFunction};

/// 検索クエリの構造を表す抽象構文木（AST）ノード。
/// 論理演算（AND, OR, NOT）や検索語（単語、型付きタグ）を保持します。
#[derive(Debug, PartialEq)]
pub enum QueryNode {
    /// AND条件 (`A & B` または `A B`)
    And(Box<QueryNode>, Box<QueryNode>),
    /// OR条件 (`A | B`)
    Or(Box<QueryNode>, Box<QueryNode>),
    /// NOT条件 (`-A`)
    Not(Box<QueryNode>),
    /// 型付きタグ検索 (`key:value`)
    TypedTag(TypedTag),
}

impl QueryNode {
    /// このノードおよび子ノードに含まれるすべてのタグの型（`tagtype`）を収集します。
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
            QueryNode::TypedTag(tt) => {
                types.insert(tt.tagtype.0.clone());
            }
        }
    }
}

// --- Parsing Logic ---

/// クエリ文字列を解析し、抽象構文木（AST）を構築する再帰下降パーサ。
///
/// # 演算子の優先順位（高い順）
/// 1. 括弧 `()`
/// 2. NOT `-`
/// 3. AND `&` (または空白)
/// 4. OR `|`
pub struct QueryParser<'a> {
    chars: Peekable<Chars<'a>>,
}

impl<'a> QueryParser<'a> {
    /// 検索クエリ文字列を解析して `QueryNode` を返します。
    ///
    /// # 構文
    /// - `&` (AND), `|` (OR), `-` (NOT) の論理演算子をサポート
    /// - `()` によるグループ化が可能
    /// - `key:value` 形式の型付き検索をサポート
    ///
    /// # Examples
    ///
    /// ```
    /// use ttfm::{QueryParser, QueryNode};
    /// 
    /// // 拡張子が "rs" かつ ("project" または "report" タグを持つ)
    /// let node = QueryParser::parse("extension:rs & (tag:project | tag:report)").unwrap();
    /// ```
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
            if let Some(&c) = self.chars.peek() {
                if c == '|' {
                    self.chars.next();
                    let right = self.parse_and_term()?;
                    left = QueryNode::Or(Box::new(left), Box::new(right));
                } else { break; }
            } else { break; }
        }
        Ok(left)
    }

    /// AND項を解析します。`&` 演算子または暗黙のANDを処理します。
    fn parse_and_term(&mut self) -> Result<QueryNode> {
        let mut left = self.parse_factor()?;
        loop {
            self.skip_whitespace();
            if let Some(&c) = self.chars.peek() {
                if c == '|' || c == ')' { break; }
                if c == '&' {
                    self.chars.next();
                    let right = self.parse_factor()?;
                    left = QueryNode::And(Box::new(left), Box::new(right));
                } else if c == '(' || c == '-' {
                    // 括弧やマイナスの前のみ、暗黙のANDとして扱う
                    let right = self.parse_factor()?;
                    left = QueryNode::And(Box::new(left), Box::new(right));
                } else {
                    // ここに到達するのは、次の単語が続く場合など
                    // しかし、parse_factorで単語を処理するので、
                    // 明示的な演算子がなくても連続するタームはANDとして扱う必要があるかもしれない。
                    // 現状のロジックでは Term がなくなったので、
                    // "key:val key2:val2" のようなケースをどう扱うか。
                    // 以前は Term で処理されていたが、ここでは parse_factor が呼ばれるはず。
                    
                    // 試しに parse_factor を呼んでみて、成功すれば AND として繋ぐ
                    // ただし、もし parse_factor が失敗するならループを抜けるべき。
                    // 現状の parse_factor は "term" が ":" を含まないとエラーになる。
                    
                    // 以前のロジック:
                    // } else { break; } 
                    // だった。
                    
                    // "key:val key2:val2" をパースする場合、
                    // 1. parse_factor -> key:val
                    // 2. loop -> peek は 'k'
                    // 3. else -> parse_factor -> key2:val2 (成功) -> AND
                    
                    let right = self.parse_factor()?;
                    left = QueryNode::And(Box::new(left), Box::new(right));
                }
            } else { break; }
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
            },
            Some(&'-') => {
                self.chars.next();
                let node = self.parse_factor()?;
                Ok(QueryNode::Not(Box::new(node)))
            },
            Some(_) => {
                let term = self.read_term()?;
                if let Some((key, value)) = term.split_once(':') {
                    if !key.is_empty() && !value.is_empty() {
                        Ok(QueryNode::TypedTag(self.create_typed_tag(key, value)))
                    } else {
                         // "key:" or ":val" case
                         Err(anyhow::anyhow!("Invalid tag format. Use 'key:value'. Found: '{}'", term))
                    }
                } else {
                    Err(anyhow::anyhow!("Missing tag type. Use 'key:value' format. Found: '{}'", term))
                }
            },
            None => Err(anyhow::anyhow!("Unexpected end of input")),
        }
    }

    /// 文字列リテラルを読み込みます。
    fn read_term(&mut self) -> Result<String> {
        self.skip_whitespace();
        let mut term = String::new();
        while let Some(&c) = self.chars.peek() {
            if c == '&' || c == '|' || c == '(' || c == ')' || c.is_whitespace() { break; }
            term.push(c);
            self.chars.next();
        }
        if term.is_empty() { Err(anyhow::anyhow!("Empty term")) } else { Ok(term) }
    }

    /// 空白文字をスキップします。
    fn skip_whitespace(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() { self.chars.next(); } else { break; }
        }
    }

    /// キーと値から `TypedTag` を生成し、正規化を行います。
    fn create_typed_tag(&self, key: &str, value: &str) -> TypedTag {
        let key_str = key.to_lowercase();
        let mut val_str = value.to_string();

        if key_str == ExtensionFunction::NAME {
            val_str = val_str.to_lowercase().trim_start_matches('.').to_string();
        }
        if key_str == PathFunction::NAME || key_str == ParentDirFunction::NAME {
            val_str = val_str.replace('\\', "/");
        }

        TypedTag {
            tagtype: TagType(key_str),
            tag: Tag(val_str),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_types() {
        let tt = TypedTag {
            tagtype: TagType(ExtensionFunction::NAME.to_string()),
            tag: Tag("rs".to_string()),
        };
        assert_eq!(tt.tagtype.0, ExtensionFunction::NAME);
        assert_eq!(tt.tag.0, "rs");
    }

    #[test]
    fn test_normalization_parse() {
        let node = QueryParser::parse("EXTENSION:RS").unwrap();
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.tagtype.0, ExtensionFunction::NAME);
            assert_eq!(tt.tag.0, "rs");
        } else {
            panic!("Should be a TypedTag");
        }
    }
}