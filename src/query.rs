use anyhow::Result;
use std::iter::Peekable;
use std::str::Chars;
use crate::types::{Label, TagType, TypedTag};
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
                if c == '|' || c == ')' { break; }
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
            },
            Some(&'-') => {
                self.chars.next();
                let node = self.parse_factor()?;
                Ok(QueryNode::Not(Box::new(node)))
            },
            Some(_) => {
                // key:value 形式の解析
                let key = self.read_string_until(':')?;
                if self.chars.peek() == Some(&':') {
                    self.chars.next();
                    let value = self.read_value()?;
                    Ok(QueryNode::TypedTag(self.create_typed_tag(&key, &value)))
                } else {
                    Err(anyhow::anyhow!("Expected ':' after tag key '{}'", key))
                }
            },
            None => Err(anyhow::anyhow!("Unexpected end of input")),
        }
    }

    /// 通常の文字列、またはクォートされた文字列を読み込みます。
    fn read_value(&mut self) -> Result<String> {
        self.skip_whitespace();
        if let Some(&'"') = self.chars.peek() {
            self.chars.next();
            let mut s = String::new();
            while let Some(&c) = self.chars.peek() {
                if c == '"' {
                    self.chars.next();
                    return Ok(s);
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
            } else {
                Ok(s)
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
            if c.is_whitespace() { self.chars.next(); } else { break; }
        }
    }

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
            label: Label(val_str),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_types() {
        let tt = TypedTag {
            tagtype: TagType("extension".into()),
            label: Label("rs".into()),
        };
        assert_eq!(tt.tagtype.0, "extension");
        assert_eq!(tt.label.0, "rs");
    }

    #[test]
    fn test_normalization_parse() {
        let node = QueryParser::parse("EXTENSION:RS").unwrap();
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.tagtype.0, "extension");
            assert_eq!(tt.label.0, "rs");
        } else {
            panic!("Should be a TypedTag");
        }
    }

    #[test]
    fn test_quoted_value_with_special_chars() {
        let input = "file_id:\"Inode { device_id: 2096 }\"";
        let node = QueryParser::parse(input).unwrap();
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.tagtype.0, "file_id");
            assert_eq!(tt.label.0, "Inode { device_id: 2096 }");
        } else {
            panic!("Should be a TypedTag");
        }
    }
}