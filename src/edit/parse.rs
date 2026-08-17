use super::{EditStrategy, QueryType};
use crate::query::parser::{unescape_glob_string, unescape_string};
use crate::tag::TagRegistry;
use anyhow::{anyhow, bail, Result};
use pest::iterators::Pair;
use pest::Parser;
use pest_derive::Parser;
use std::collections::HashSet;
use std::fmt;

#[derive(Parser)]
#[grammar = "query/lexical.pest"]
#[grammar = "edit/edit_query.pest"]
struct EditParser;

// Text はメタ文字前の `\` を保持したまま持つ（クォート内外を問わない。
// `*` はクォートを貫通するので、リテラルかどうかは `\` の有無でしか判らない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafPart {
    Text(String),
    Braced(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditQueryLeaf {
    pub parts: Vec<LeafPart>,
    pub quoted: bool,
}

impl EditQueryLeaf {
    /// 既定の読み方。Braced は波括弧ごとリテラルに書き戻す。`\` は保持。
    pub fn render(&self) -> String {
        self.parts
            .iter()
            .map(|p| match p {
                LeafPart::Text(s) => s.clone(),
                LeafPart::Braced(s) => format!("{{{s}}}"),
            })
            .collect()
    }

    /// 値としての読み方。メタ文字前の `\` を落とす。
    pub fn value(&self) -> String {
        drop_glob_escapes(&self.render())
    }

    pub fn has_braced(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, LeafPart::Braced(_)))
    }
}

fn drop_glob_escapes(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if matches!(next, '*' | '?' | '[' | ']') {
                    result.push(chars.next().unwrap());
                    continue;
                }
            }
        }
        result.push(c);
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditQueryNode {
    pub tag_type: EditQueryLeaf,
    pub label: Option<EditQueryLeaf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditQuery {
    pub nodes: Vec<EditQueryNode>,
}

impl fmt::Display for EditQueryNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.tag_type.render())?;
        if let Some(label) = &self.label {
            write!(f, "{}", label.render())?;
        }
        Ok(())
    }
}

fn strip_braces(s: &str) -> String {
    s[1..s.len() - 1].to_string()
}

fn build_unquoted_part(pair: Pair<Rule>) -> Result<LeafPart> {
    match pair.as_rule() {
        Rule::braced => Ok(LeafPart::Braced(strip_braces(pair.as_str()))),
        Rule::identifier | Rule::unquoted_tag_string => {
            Ok(LeafPart::Text(unescape_glob_string(pair.as_str())?))
        }
        r => Err(anyhow!("unexpected rule in unquoted template part: {r:?}")),
    }
}

fn build_quoted_part(pair: Pair<Rule>) -> Result<LeafPart> {
    match pair.as_rule() {
        Rule::braced_d | Rule::braced_s => {
            Ok(LeafPart::Braced(strip_braces(pair.as_str())))
        }
        Rule::escape | Rule::text_d | Rule::text_s | Rule::lbrace => {
            Ok(LeafPart::Text(unescape_string(pair.as_str())?))
        }
        r => Err(anyhow!("unexpected rule in quoted template part: {r:?}")),
    }
}

// pair: type_tpl | label_tpl
fn build_leaf(pair: Pair<Rule>) -> Result<EditQueryLeaf> {
    let mut inner = pair.into_inner();
    let Some(first) = inner.next() else {
        return Ok(EditQueryLeaf {
            parts: vec![],
            quoted: false,
        });
    };
    if first.as_rule() == Rule::quoted_tpl {
        let parts = first
            .into_inner()
            .map(build_quoted_part)
            .collect::<Result<Vec<_>>>()?;
        return Ok(EditQueryLeaf {
            parts,
            quoted: true,
        });
    }
    let mut parts = vec![build_unquoted_part(first)?];
    for p in inner {
        parts.push(build_unquoted_part(p)?);
    }
    Ok(EditQueryLeaf {
        parts,
        quoted: false,
    })
}

const META_TYPES: [&str; 3] = ["type", "label", "tag"];

// 未登録型は既定 Append で許可。登録済みかつ edit() が None のときだけ Forbidden。
fn is_forbidden(reg: &TagRegistry, name: &str) -> bool {
    reg.get(name).is_some_and(|f| f.edit().is_none())
}

// 型・ラベルとも波括弧を含まない静的な指定だけを対象に、Replace 型が同じ型名で
// 複数指定されていないかを見る。データ起因の多値（`{n}` を含む指定）は confirm 側の担当。
fn reject_static_multi_literal(
    nodes: &[EditQueryNode],
    reg: &TagRegistry,
) -> Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    for n in nodes {
        if n.tag_type.has_braced() {
            continue;
        }
        let Some(label) = &n.label else { continue };
        if label.has_braced() {
            continue;
        }
        let name = n.tag_type.value();
        let strategy =
            reg.get(&name).and_then(|f| f.edit()).map(|e| e.strategy());
        if !matches!(strategy, Some(EditStrategy::Replace)) {
            continue;
        }
        if !seen.insert(name.clone()) {
            bail!(
                "tag type '{name}' is specified multiple times with static values (ambiguous Replace)"
            );
        }
    }
    Ok(())
}

pub fn parse_edit_query(
    q: &str,
    qt: QueryType,
    reg: &TagRegistry,
) -> Result<EditQuery> {
    let mut pairs = EditParser::parse(Rule::edit_query, q)
        .map_err(|e| anyhow!("invalid edit query {q:?}: {e}"))?;
    let edit_query_pair = pairs.next().unwrap();

    let mut nodes = Vec::new();
    for tag_tpl_pair in edit_query_pair.into_inner() {
        if tag_tpl_pair.as_rule() != Rule::tag_tpl {
            continue;
        }
        let mut inner = tag_tpl_pair.into_inner();
        let type_pair = inner.next().unwrap();
        let tag_type = build_leaf(type_pair)?;
        let label = inner.next().map(build_leaf).transpose()?;
        nodes.push(EditQueryNode { tag_type, label });
    }

    for n in &nodes {
        if n.tag_type.has_braced() {
            continue;
        }
        let name = n.tag_type.value();
        if name.is_empty() {
            bail!("empty tag type in EditQuery");
        }
        if META_TYPES.contains(&name.as_str()) {
            bail!("'{name}' is a meta type and cannot be used as a literal type in EditQuery");
        }
        if is_forbidden(reg, &name) {
            bail!(
                "tag type '{name}' is registered but not editable (Forbidden)"
            );
        }
        if matches!(qt, QueryType::Tag) && n.label.is_none() {
            bail!("Projection '{name}:' is not allowed in EditQuery (Tag direction)");
        }
    }

    reject_static_multi_literal(&nodes, reg)?;

    Ok(EditQuery { nodes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(q: &str) -> EditQueryNode {
        parse_edit_query(q, QueryType::Tag, &TagRegistry::with_standard())
            .unwrap()
            .nodes
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn space_and_piped_separators_are_same() {
        let reg = TagRegistry::with_standard();
        let a =
            parse_edit_query("project:a note:b", QueryType::Tag, &reg).unwrap();
        let b = parse_edit_query("project:a | note:b", QueryType::Tag, &reg)
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn pipe_without_trailing_space_is_error() {
        let reg = TagRegistry::with_standard();
        assert!(parse_edit_query("project:a |note:b", QueryType::Tag, &reg)
            .is_err());
    }

    #[test]
    fn spaced_ampersand_is_error() {
        let reg = TagRegistry::with_standard();
        assert!(parse_edit_query("project:a & note:b", QueryType::Tag, &reg)
            .is_err());
    }

    #[test]
    fn unspaced_pipe_and_ampersand_are_part_of_label() {
        let n = node("project:x&y|z");
        assert_eq!(n.label.unwrap().value(), "x&y|z");
    }

    #[test]
    fn unquoted_backslash_escape_drops_the_backslash() {
        let n = node("note:x\\ y");
        assert_eq!(n.label.unwrap().render(), "x y");
    }

    #[test]
    fn glob_escape_survives_until_value() {
        let leaf = node("note:my\\*file").label.unwrap();
        assert_eq!(leaf.render(), "my\\*file");
        assert_eq!(leaf.value(), "my*file");
    }

    #[test]
    fn quoted_glob_escape_survives_too() {
        let leaf = node("note:\"my\\*file\"").label.unwrap();
        assert!(leaf.quoted);
        assert_eq!(leaf.render(), "my\\*file");
        assert_eq!(leaf.value(), "my*file");
    }

    #[test]
    fn braced_inside_quotes_is_a_braced_part() {
        let leaf = node("note:\"a{1}b\"").label.unwrap();
        assert!(leaf
            .parts
            .iter()
            .any(|p| *p == LeafPart::Braced("1".to_string())));
    }

    #[test]
    fn escaped_brace_inside_quotes_is_text() {
        let leaf = node("note:\"a\\{1}b\"").label.unwrap();
        assert!(!leaf.has_braced());
        assert_eq!(leaf.render(), "a{1}b");
    }

    #[test]
    fn non_numeric_braced_parses_successfully() {
        let leaf = node("note:{abc}").label.unwrap();
        assert!(leaf
            .parts
            .iter()
            .any(|p| *p == LeafPart::Braced("abc".to_string())));
    }

    #[test]
    fn unclosed_brace_inside_quotes_is_text() {
        let leaf = node("note:\"a{b\"").label.unwrap();
        assert!(!leaf.has_braced());
        assert_eq!(leaf.render(), "a{b");
    }

    #[test]
    fn bare_unclosed_brace_unquoted_is_error() {
        let reg = TagRegistry::with_standard();
        assert!(parse_edit_query("note:a{b", QueryType::Tag, &reg).is_err());
    }

    #[test]
    fn label_splits_into_text_and_braced_parts() {
        let leaf = node("note:abc{1}def").label.unwrap();
        assert_eq!(
            leaf.parts,
            vec![
                LeafPart::Text("abc".to_string()),
                LeafPart::Braced("1".to_string()),
                LeafPart::Text("def".to_string()),
            ]
        );
    }

    #[test]
    fn braced_in_type_position_parses() {
        let n = node("{1}:val");
        assert!(n.tag_type.has_braced());
        assert_eq!(n.label.unwrap().value(), "val");
    }

    #[test]
    fn quoted_leaf_keeps_quoted_flag() {
        let n = node("note:\"hello\"");
        assert!(n.label.unwrap().quoted);
        let n2 = node("note:hello");
        assert!(!n2.label.unwrap().quoted);
    }

    #[test]
    fn render_writes_braced_back_as_literal() {
        let leaf = node("note:{1}").label.unwrap();
        assert_eq!(leaf.render(), "{1}");
    }

    #[test]
    fn forbidden_type_without_braced_is_error() {
        let reg = TagRegistry::with_standard();
        assert!(parse_edit_query("size:100", QueryType::Tag, &reg).is_err());
    }

    #[test]
    fn unregistered_custom_type_is_accepted() {
        let reg = TagRegistry::with_standard();
        assert!(parse_edit_query("myproj:foo", QueryType::Tag, &reg).is_ok());
    }

    #[test]
    fn meta_type_without_braced_is_error() {
        let reg = TagRegistry::with_standard();
        assert!(parse_edit_query("tag:foo", QueryType::Tag, &reg).is_err());
    }

    #[test]
    fn empty_type_without_braced_is_error() {
        let reg = TagRegistry::with_standard();
        assert!(parse_edit_query("\"\":val", QueryType::Tag, &reg).is_err());
    }

    #[test]
    fn static_multi_literal_on_replace_type_is_error() {
        let reg = TagRegistry::with_standard();
        assert!(
            parse_edit_query("name:a name:b", QueryType::Tag, &reg).is_err()
        );
    }
}
