use anyhow::Result;
use std::iter::Peekable;
use std::str::Chars;

// --- Schema Definition ---

pub struct ColumnDef {
    pub name: &'static str,
    pub sql_type: &'static str,
}

// Generate schema columns from TagType definitions
pub fn get_schema_columns() -> Vec<ColumnDef> {
    TagType::all_variants().iter()
        .map(|t| ColumnDef {
            name: t.db_column_name(),
            sql_type: t.sql_type(),
        })
        .collect()
}

// --- Search Types ---

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TagType {
    // All System Tags are Searchable
    Path,
    ParentDir,
    FileName,
    Stem,
    Extension,
    Directory,
    SizeBytes,
    ModifiedTs,
    Kind,
    SizeStr,
    ModifiedStr,
    Tags,
}

#[derive(Debug, PartialEq)]
pub struct TypedTag {
    pub tag_type: TagTypeEnum,
    pub value: String,
}

#[derive(Debug, PartialEq)]
pub enum TagTypeEnum {
    System(TagType),
    User(String),
}

impl TagType {
    pub fn db_column_name(&self) -> &'static str {
        match self {
            TagType::Path => "path",
            TagType::ParentDir => "parentdir",
            TagType::FileName => "filename",
            TagType::Stem => "stem",
            TagType::Extension => "extension",
            TagType::Directory => "directory",
            TagType::SizeBytes => "size_bytes",
            TagType::ModifiedTs => "modified_ts",
            TagType::Kind => "kind",
            TagType::SizeStr => "size_str",
            TagType::ModifiedStr => "modified_str",
            TagType::Tags => "tags",
        }
    }

    pub fn sql_type(&self) -> &'static str {
        match self {
            TagType::Directory => "BOOLEAN",
            TagType::SizeBytes | TagType::ModifiedTs => "BIGINT",
            TagType::Tags => "MAP(TEXT, TEXT)",
            _ => "TEXT",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TagType::FileName => "filename",
            TagType::Stem => "stem",
            TagType::Directory => "directory",
            TagType::Extension => "extension",
            TagType::ParentDir => "parentdir",
            TagType::Path => "path",
            TagType::SizeBytes => "size_bytes",
            TagType::ModifiedTs => "modified_ts",
            TagType::Kind => "kind",
            TagType::SizeStr => "size_str",
            TagType::ModifiedStr => "modified_str",
            TagType::Tags => "tags",
        }
    }

    pub fn aliases(&self) -> &'static [&'static str] {
        match self {
            TagType::FileName => &["file", "name"],
            TagType::Directory => &["dir"],
            TagType::Extension => &["ext"],
            TagType::ParentDir => &["parent"],
            TagType::SizeBytes => &["size_b"],
            TagType::ModifiedTs => &["mod_ts"],
            TagType::SizeStr => &["size"],
            TagType::ModifiedStr => &["modified", "date"],
            _ => &[],
        }
    }

    // List of all system variants (used for schema and search parsing)
    pub fn all_variants() -> &'static [TagType] {
        &[
            TagType::Path,
            TagType::ParentDir,
            TagType::FileName,
            TagType::Stem,
            TagType::Extension,
            TagType::Directory,
            TagType::SizeBytes,
            TagType::ModifiedTs,
            TagType::Kind,
            TagType::SizeStr,
            TagType::ModifiedStr,
            TagType::Tags,
        ]
    }
}

impl TypedTag {
    fn from_key_value(key: &str, value: &str) -> Self {
        let lower_key = key.to_lowercase();
        
        let system_type = TagType::all_variants().iter().find(|t| {
            t.as_str() == lower_key || t.aliases().contains(&lower_key.as_str())
        });

        let (tag_type_enum, final_value) = if let Some(&t) = system_type {
            let val = if matches!(t, TagType::Extension) {
                value.to_lowercase().trim_start_matches('.').to_string()
            } else {
                value.to_string()
            };
            (TagTypeEnum::System(t), val)
        } else {
            (TagTypeEnum::User(key.to_string()), value.to_string())
        };

        TypedTag { tag_type: tag_type_enum, value: final_value }
    }

    fn to_sql(&self) -> String {
        let val = Self::escape(&self.value);
        match &self.tag_type {
            TagTypeEnum::System(sys) => match sys {
                TagType::FileName => format!("(directory = FALSE AND {} ILIKE '%{}%')", sys.db_column_name(), val),
                TagType::Stem => format!("(directory = FALSE AND {} ILIKE '%{}%')", sys.db_column_name(), val),
                TagType::Directory => format!("({} = TRUE AND {} ILIKE '%{}%')", TagType::Directory.db_column_name(), TagType::FileName.db_column_name(), val), 
                TagType::Extension => format!("{} = '{}'", sys.db_column_name(), val),
                TagType::ParentDir => format!("({} ILIKE '%/{}' OR {} = '{}')", sys.db_column_name(), val, sys.db_column_name(), val),
                TagType::Path | TagType::Kind | TagType::SizeStr | TagType::ModifiedStr => 
                    format!("{} ILIKE '%{}%'", sys.db_column_name(), val),
                TagType::SizeBytes | TagType::ModifiedTs => 
                    format!("{} = {}", sys.db_column_name(), val), 
                TagType::Tags => format!("1=0"),
            },
            TagTypeEnum::User(key) => format!("element_at(tags, '{}') ILIKE '%{}%'", Self::escape(key), val),
        }
    }

    fn escape(s: &str) -> String {
        s.replace("'", "''")
    }
}

#[derive(Debug, PartialEq)]
pub enum QueryNode {
    And(Box<QueryNode>, Box<QueryNode>),
    Or(Box<QueryNode>, Box<QueryNode>),
    Not(Box<QueryNode>), 
    Term(String),
    Tag(TypedTag),
}

impl QueryNode {
    pub fn to_sql(&self) -> String {
        match self {
            QueryNode::And(left, right) => format!("({} AND {})", left.to_sql(), right.to_sql()),
            QueryNode::Or(left, right) => format!("({} OR {})", left.to_sql(), right.to_sql()),
            QueryNode::Not(node) => format!("NOT ({})", node.to_sql()),
            QueryNode::Term(term) => format!("filename ILIKE '%{}%'", term.replace("'", "''")),
            QueryNode::Tag(tag) => tag.to_sql(),
        }
    }
}

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
                        Ok(QueryNode::Tag(TypedTag::from_key_value(key, value)))
                    } else {
                        Ok(QueryNode::Term(term))
                    }
                } else {
                    Ok(QueryNode::Term(term))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_parser_basic() {
        let node = QueryParser::parse("foo").unwrap();
        assert_eq!(node, QueryNode::Term("foo".to_string()));

        let node = QueryParser::parse("foo & bar").unwrap();
        assert_eq!(node, QueryNode::And(
            Box::new(QueryNode::Term("foo".to_string())),
            Box::new(QueryNode::Term("bar".to_string()))
        ));
    }

    #[test]
    fn test_query_parser_typed_tags() {
        // filename:foo
        let node = QueryParser::parse("filename:foo").unwrap();
        assert_eq!(node, QueryNode::Tag(TypedTag { 
            tag_type: TagTypeEnum::System(TagType::FileName), 
            value: "foo".to_string() 
        }));
        assert_eq!(node.to_sql(), "(directory = FALSE AND filename ILIKE '%foo%')");

        // extension:png
        let node = QueryParser::parse("extension:png").unwrap();
        assert_eq!(node, QueryNode::Tag(TypedTag { 
            tag_type: TagTypeEnum::System(TagType::Extension), 
            value: "png".to_string() 
        }));
        assert_eq!(node.to_sql(), "extension = 'png'");

        // user_tag:value
        let node = QueryParser::parse("project:alpha").unwrap();
        assert_eq!(node, QueryNode::Tag(TypedTag { 
            tag_type: TagTypeEnum::User("project".to_string()), 
            value: "alpha".to_string() 
        }));
        assert_eq!(node.to_sql(), "element_at(tags, 'project') ILIKE '%alpha%'");

        // Newly exposed system tags
        // kind:Folder
        let node = QueryParser::parse("kind:Folder").unwrap();
        assert_eq!(node.to_sql(), "kind ILIKE '%Folder%'");

        // size:10kb (mapped to size_str)
        let node = QueryParser::parse("size:10kb").unwrap();
        assert_eq!(node.to_sql(), "size_str ILIKE '%10kb%'");

        // size_b:100 (numeric)
        let node = QueryParser::parse("size_b:100").unwrap();
        assert_eq!(node.to_sql(), "size_bytes = 100");
    }

    #[test]
    fn test_query_parser_multiple_colons() {
        // filename:my:file.txt
        let node = QueryParser::parse("filename:my:file.txt").unwrap();
        assert_eq!(node, QueryNode::Tag(TypedTag { 
            tag_type: TagTypeEnum::System(TagType::FileName), 
            value: "my:file.txt".to_string() 
        }));
    }

    #[test]
    fn test_query_parser_errors() {
        assert!(QueryParser::parse("foo bar").is_err());
    }
}