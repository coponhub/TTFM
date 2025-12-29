use anyhow::Result;
use std::iter::Peekable;
use std::str::Chars;
use crate::types::{Tag, TagType, TypedTag};

#[derive(Debug, PartialEq)]
pub enum QueryNode {
    And(Box<QueryNode>, Box<QueryNode>),
    Or(Box<QueryNode>, Box<QueryNode>),
    Not(Box<QueryNode>), 
    Term(Tag),
    TypedTag(TypedTag),
}

// --- Parsing Logic ---

pub struct QueryParser<'a> {
    chars: Peekable<Chars<'a>>,
}

impl<'a> QueryParser<'a> {
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
                    let right = self.parse_factor()?;
                    left = QueryNode::And(Box::new(left), Box::new(right));
                } else {
                    break;
                }
            } else { break; }
        }
        Ok(left)
    }

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
                        Ok(QueryNode::Term(Tag(term)))
                    }
                } else {
                    Ok(QueryNode::Term(Tag(term)))
                }
            },
            None => Err(anyhow::anyhow!("Unexpected end of input")),
        }
    }

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

    fn skip_whitespace(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() { self.chars.next(); } else { break; }
        }
    }

    fn create_typed_tag(&self, key: &str, value: &str) -> TypedTag {
        let mut key_str = key.to_lowercase();
        let mut val_str = value.to_string();

        // 互換性のための内部正規化 (ロードマップの「入力の手間の軽減」でエイリアス化予定)
        if key_str == "ext" { key_str = TagType::EXTENSION.to_string(); }
        if key_str == "parent" { key_str = TagType::PARENT_DIR.to_string(); }

        if key_str == TagType::EXTENSION {
            val_str = val_str.to_lowercase().trim_start_matches('.').to_string();
        }
        if key_str == TagType::PATH || key_str == TagType::PARENT_DIR {
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
            tagtype: TagType(TagType::EXTENSION.to_string()),
            tag: Tag("rs".to_string()),
        };
        assert_eq!(tt.tagtype.0, TagType::EXTENSION);
        assert_eq!(tt.tag.0, "rs");
    }

    #[test]
    fn test_parser_with_new_types() {
        let node = QueryParser::parse("ext:rs").unwrap();
        if let QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.tagtype.0, TagType::EXTENSION);
            assert_eq!(tt.tag.0, "rs");
        } else {
            panic!("Should be a TypedTag");
        }
    }
}
