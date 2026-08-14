use super::glob_capture;
use super::parse::{EditQuery, EditQueryLeaf, EditQueryNode, LeafPart};
use crate::db::Store;
use crate::query::error::WarningSink;
use crate::query::parser::{self, PestQueryParser, Rule};
use crate::response::Item;
use crate::search::SearchOptions;
use crate::tag::{self, TagRegistry};
use crate::types::TagType;
use anyhow::{anyhow, bail, Result};
use pest::iterators::{Pair, Pairs};
use pest::Parser as _;
use std::collections::{HashMap, HashSet};

pub fn search_and_apply_captures(
    store: &Store,
    registry: &TagRegistry,
    search_query: &str,
    parsed: Option<&EditQuery>,
    sink: &mut dyn WarningSink,
) -> Result<Vec<(Item, Option<EditQuery>)>> {
    if let Some(p) = parsed {
        reject_glob(p)?;
    }

    let mut resp = crate::search::search(
        store,
        registry,
        search_query,
        SearchOptions::default(),
        sink,
    )?;
    resp.query_into_tags();

    let units = parsed.map(|_| collect_units(search_query)).transpose()?;

    resp.results
        .into_iter()
        .map(|item| {
            let edit = match (parsed, &units) {
                (Some(p), Some((units, total))) => {
                    Some(apply_captures(&item, p, units, *total, registry)?)
                }
                _ => parsed.cloned(),
            };
            Ok((item, edit))
        })
        .collect()
}

#[derive(Debug, PartialEq)]
enum Unit {
    /// `typed_tag`: type と label は同じ1トークン組として集約する。
    Pair { ty: Pattern, label: Pattern },
    /// `type_ref`: ラベルを持たない type 単独。
    Type(Pattern),
    /// typed_tag/type_ref の外に裸で現れるラベル相当トークン
    /// （stuck_operand、label_comparison 第3選択肢など）。
    Label(Pattern),
}

#[derive(Debug, PartialEq)]
enum Pattern {
    Glob { text: String, numbers: Vec<usize> },
    Literal(String),
}

fn number_metachars(text: &str, next: &mut usize) -> Vec<usize> {
    glob_capture::lex(text)
        .iter()
        .filter(|a| a.is_metachar())
        .map(|_| {
            let n = *next;
            *next += 1;
            n
        })
        .collect()
}

fn pattern_text(pair: &Pair<Rule>) -> Result<String> {
    if pair.as_rule() == Rule::quoted_string {
        let raw = pair.as_str();
        let content = &raw[1..raw.len() - 1];
        return parser::unescape_string(content);
    }
    Ok(pair.as_str().to_string())
}

fn build_pattern(pair: Pair<Rule>, next: &mut usize) -> Result<Pattern> {
    if pair.as_rule() == Rule::number {
        return Ok(Pattern::Literal(pair.as_str().to_string()));
    }
    let text = pattern_text(&pair)?;
    let numbers = number_metachars(&text, next);
    Ok(Pattern::Glob { text, numbers })
}

/// `tag_type`/`tag_label` の中身（quoted_string/identifier/number/unquoted_tag_string）
/// を取り出す。両ルールとも choice が1つだけ子として現れる。
fn leaf(pair: Pair<Rule>) -> Pair<Rule> {
    pair.into_inner().next().unwrap()
}

fn walk(
    pairs: Pairs<Rule>,
    units: &mut Vec<Unit>,
    next: &mut usize,
) -> Result<()> {
    for pair in pairs {
        match pair.as_rule() {
            Rule::typed_tag => {
                let mut inner = pair.into_inner();
                let ty = build_pattern(leaf(inner.next().unwrap()), next)?;
                let label = build_pattern(leaf(inner.next().unwrap()), next)?;
                units.push(Unit::Pair { ty, label });
            }
            Rule::type_ref => {
                let ty = build_pattern(
                    leaf(pair.into_inner().next().unwrap()),
                    next,
                )?;
                units.push(Unit::Type(ty));
            }
            Rule::quoted_string
            | Rule::unquoted_string
            | Rule::unquoted_tag_string
            | Rule::number => {
                units.push(Unit::Label(build_pattern(pair, next)?));
            }
            _ => walk(pair.into_inner(), units, next)?,
        }
    }
    Ok(())
}

fn collect_units(search_query: &str) -> Result<(Vec<Unit>, usize)> {
    let pairs = PestQueryParser::parse(Rule::query, search_query)
        .map_err(|e| anyhow!("{e}"))?;
    let mut units = Vec::new();
    let mut next = 1usize;
    walk(pairs, &mut units, &mut next)?;
    Ok((units, next - 1))
}

impl Pattern {
    fn numbers(&self) -> &[usize] {
        match self {
            Pattern::Glob { numbers, .. } => numbers,
            Pattern::Literal(_) => &[],
        }
    }
}

impl Unit {
    fn numbers(&self) -> Vec<usize> {
        match self {
            Unit::Pair { ty, label } => ty
                .numbers()
                .iter()
                .chain(label.numbers().iter())
                .copied()
                .collect(),
            Unit::Type(p) | Unit::Label(p) => p.numbers().to_vec(),
        }
    }
}

fn braced_parts(q: &EditQuery) -> impl Iterator<Item = &str> {
    q.nodes.iter().flat_map(|n| {
        n.tag_type
            .parts
            .iter()
            .chain(n.label.iter().flat_map(|l| l.parts.iter()))
            .filter_map(|p| match p {
                LeafPart::Braced(s) => Some(s.as_str()),
                LeafPart::Text(_) => None,
            })
    })
}

fn resolve_refs(parsed: &EditQuery, total: usize) -> Result<HashSet<usize>> {
    let mut referenced = HashSet::new();
    for braced in braced_parts(parsed) {
        let n: usize = braced.parse().map_err(|_| {
            anyhow!("capture reference {{{braced}}} is not a number")
        })?;
        if n == 0 {
            bail!(
                "capture reference {{0}} is not allowed; numbering starts at 1"
            );
        }
        if n > total {
            bail!("capture reference {{{n}}} exceeds the number of captures ({total})");
        }
        referenced.insert(n);
    }
    Ok(referenced)
}

fn fit_type(p: &Pattern, ty: &TagType) -> Option<Vec<String>> {
    match p {
        Pattern::Literal(s) => (ty.as_str() == s).then(Vec::new),
        Pattern::Glob { text, .. } => {
            glob_capture::glob_captures(text, ty.as_str())
        }
    }
}

fn all_types(reg: &TagRegistry, item: &Item) -> Vec<TagType> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for ty in reg
        .iter_arcs()
        .map(|f| TagType::from(f.name()))
        .chain(item.tags.entries.iter().map(|e| e.typed_tag.tag_type()))
    {
        if seen.insert(ty.clone()) {
            out.push(ty);
        }
    }
    out
}

fn candidate_types(
    reg: &TagRegistry,
    item: &Item,
    p: &Pattern,
) -> Vec<TagType> {
    all_types(reg, item)
        .into_iter()
        .filter(|ty| fit_type(p, ty).is_some())
        .collect()
}

fn fit_label(
    reg: &TagRegistry,
    p: &Pattern,
    ty: &TagType,
    item: &Item,
) -> Vec<Vec<String>> {
    match p {
        Pattern::Literal(s) => {
            if tag::entries_of(item, ty).any(|e| e.typed_tag.as_str() == *s) {
                vec![Vec::new()]
            } else {
                Vec::new()
            }
        }
        Pattern::Glob { text, .. } => match reg.get(ty.as_str()) {
            Some(f) => f.query().capture(ty, text, item),
            None => tag::default_capture(ty, text, item),
        },
    }
}

#[derive(Debug, Clone)]
struct Binding(Vec<(usize, String)>);

fn hits_for_unit(reg: &TagRegistry, item: &Item, unit: &Unit) -> Vec<Binding> {
    let raw: Vec<Vec<String>> = match unit {
        Unit::Pair { ty, label } => candidate_types(reg, item, ty)
            .into_iter()
            .flat_map(|t| {
                let head = fit_type(ty, &t).unwrap();
                fit_label(reg, label, &t, item)
                    .into_iter()
                    .map(|tail| {
                        head.iter()
                            .cloned()
                            .chain(tail)
                            .collect::<Vec<String>>()
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
        Unit::Type(p) => candidate_types(reg, item, p)
            .into_iter()
            .filter_map(|t| fit_type(p, &t))
            .collect(),
        Unit::Label(p) => all_types(reg, item)
            .into_iter()
            .flat_map(|t| fit_label(reg, p, &t, item))
            .collect(),
    };
    let numbers = unit.numbers();
    if raw.is_empty() {
        return vec![Binding(Vec::new())];
    }
    raw.into_iter()
        .map(|values| Binding(numbers.iter().copied().zip(values).collect()))
        .collect()
}

fn cartesian_bindings(axes: Vec<Vec<Binding>>) -> Vec<HashMap<usize, String>> {
    axes.into_iter().fold(vec![HashMap::new()], |acc, hits| {
        acc.into_iter()
            .flat_map(|base| {
                hits.iter().map(move |b| {
                    let mut merged = base.clone();
                    merged.extend(b.0.iter().cloned());
                    merged
                })
            })
            .collect()
    })
}

fn materialise_leaf(
    leaf: &EditQueryLeaf,
    bindings: &HashMap<usize, String>,
) -> Option<EditQueryLeaf> {
    let mut parts = Vec::with_capacity(leaf.parts.len());
    for part in &leaf.parts {
        match part {
            LeafPart::Text(s) => parts.push(LeafPart::Text(s.clone())),
            LeafPart::Braced(s) => {
                let n: usize =
                    s.parse().expect("resolve_refs already validated this");
                parts.push(LeafPart::Text(bindings.get(&n)?.clone()));
            }
        }
    }
    Some(EditQueryLeaf {
        parts,
        quoted: leaf.quoted,
    })
}

fn has_unescaped_metachar(text: &str) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            continue;
        }
        if matches!(c, '*' | '?' | '[' | ']') {
            return true;
        }
    }
    false
}

// 捕捉値（Braced）は対象外。ユーザーが書いた Text だけを見る。
fn reject_glob(parsed: &EditQuery) -> Result<()> {
    for node in &parsed.nodes {
        let leaves = std::iter::once(&node.tag_type).chain(node.label.iter());
        for leaf in leaves {
            for part in &leaf.parts {
                if let LeafPart::Text(s) = part {
                    if has_unescaped_metachar(s) {
                        bail!("EditQuery value {s:?} contains an unescaped glob metacharacter");
                    }
                }
            }
        }
    }
    Ok(())
}

fn materialise(
    parsed: &EditQuery,
    bindings: &HashMap<usize, String>,
) -> Vec<EditQueryNode> {
    let mut out = Vec::new();
    for n in &parsed.nodes {
        let Some(tag_type) = materialise_leaf(&n.tag_type, bindings) else {
            continue;
        };
        let label = match &n.label {
            Some(l) => match materialise_leaf(l, bindings) {
                Some(l) => Some(l),
                None => continue,
            },
            None => None,
        };
        out.push(EditQueryNode { tag_type, label });
    }
    out
}

fn apply_captures(
    item: &Item,
    parsed: &EditQuery,
    units: &[Unit],
    total: usize,
    reg: &TagRegistry,
) -> Result<EditQuery> {
    let referenced = resolve_refs(parsed, total)?;
    let axes: Vec<&Unit> = units
        .iter()
        .filter(|u| u.numbers().iter().any(|n| referenced.contains(n)))
        .collect();

    if axes.is_empty() {
        return Ok(EditQuery {
            nodes: materialise(parsed, &HashMap::new()),
        });
    }

    let candidates: Vec<Vec<Binding>> = axes
        .into_iter()
        .map(|u| hits_for_unit(reg, item, u))
        .collect();

    let mut out: Vec<EditQueryNode> = Vec::new();
    for bindings in cartesian_bindings(candidates) {
        for node in materialise(parsed, &bindings) {
            if !out.contains(&node) {
                out.push(node);
            }
        }
    }
    Ok(EditQuery { nodes: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(text: &str, numbers: &[usize]) -> Pattern {
        Pattern::Glob {
            text: text.to_string(),
            numbers: numbers.to_vec(),
        }
    }

    fn literal(text: &str) -> Pattern {
        Pattern::Literal(text.to_string())
    }

    #[test]
    fn numbers_run_left_to_right_across_label_and_type() {
        let (units, total) = collect_units("a*:b* | c*:d*").unwrap();
        assert_eq!(
            units,
            vec![
                Unit::Pair {
                    ty: glob("a*", &[1]),
                    label: glob("b*", &[2]),
                },
                Unit::Pair {
                    ty: glob("c*", &[3]),
                    label: glob("d*", &[4]),
                },
            ]
        );
        assert_eq!(total, 4);
    }

    #[test]
    fn metachar_inside_quotes_is_numbered() {
        let (units, total) = collect_units(r#"project:"al*pha""#).unwrap();
        assert_eq!(
            units,
            vec![Unit::Pair {
                ty: glob("project", &[]),
                label: glob("al*pha", &[1]),
            }]
        );
        assert_eq!(total, 1);
    }

    #[test]
    fn escaped_metachar_is_not_numbered() {
        let (units, total) = collect_units(r"project:al\*pha").unwrap();
        assert_eq!(
            units,
            vec![Unit::Pair {
                ty: glob("project", &[]),
                label: glob(r"al\*pha", &[]),
            }]
        );
        assert_eq!(total, 0);
    }

    #[test]
    fn count_shorthand_consumes_no_number() {
        let (units, total) = collect_units("count() > 0").unwrap();
        assert_eq!(units, vec![Unit::Label(literal("0"))]);
        assert_eq!(total, 0);
    }

    #[test]
    fn explicit_count_star_star_consumes_two() {
        let (units, total) = collect_units("count(*:*) > 0").unwrap();
        assert_eq!(
            units,
            vec![
                Unit::Pair {
                    ty: glob("*", &[1]),
                    label: glob("*", &[2]),
                },
                Unit::Label(literal("0")),
            ]
        );
        assert_eq!(total, 2);
    }

    #[test]
    fn bare_token_on_stuck_comparison_rhs_is_numbered() {
        let (units, total) = collect_units("size:>al*pha").unwrap();
        assert_eq!(
            units,
            vec![
                Unit::Type(glob("size", &[])),
                Unit::Label(glob("al*pha", &[1])),
            ]
        );
        assert_eq!(total, 1);
    }

    #[test]
    fn arithmetic_star_is_not_numbered() {
        let (units, total) = collect_units("(size: * 2)").unwrap();
        assert_eq!(
            units,
            vec![Unit::Type(glob("size", &[])), Unit::Label(literal("2"))]
        );
        assert_eq!(total, 0);
    }

    #[test]
    fn aggregate_and_difference_operands_follow_same_rule() {
        let (units, total) =
            collect_units("count(pro*:al*) - count(a*:b*)").unwrap();
        assert_eq!(
            units,
            vec![
                Unit::Pair {
                    ty: glob("pro*", &[1]),
                    label: glob("al*", &[2]),
                },
                Unit::Pair {
                    ty: glob("a*", &[3]),
                    label: glob("b*", &[4]),
                },
            ]
        );
        assert_eq!(total, 4);
    }

    #[test]
    fn or_branches_are_collected_like_any_other_position() {
        let (units, total) = collect_units("x*:y* | z*:w*").unwrap();
        assert_eq!(
            units,
            vec![
                Unit::Pair {
                    ty: glob("x*", &[1]),
                    label: glob("y*", &[2]),
                },
                Unit::Pair {
                    ty: glob("z*", &[3]),
                    label: glob("w*", &[4]),
                },
            ]
        );
        assert_eq!(total, 4);
    }

    #[test]
    fn escaped_metachar_in_type_position_is_not_numbered() {
        let (units, total) = collect_units(r"\*:x").unwrap();
        assert_eq!(
            units,
            vec![Unit::Pair {
                ty: glob(r"\*", &[]),
                label: glob("x", &[]),
            }]
        );
        assert_eq!(total, 0);
    }

    #[test]
    fn quoted_type_pattern_is_numbered() {
        let (units, total) = collect_units(r#""ext*":txt"#).unwrap();
        assert_eq!(
            units,
            vec![Unit::Pair {
                ty: glob("ext*", &[1]),
                label: glob("txt", &[]),
            }]
        );
        assert_eq!(total, 1);
    }

    #[test]
    fn type_side_precedes_label_side_in_a_pair() {
        let (units, _total) = collect_units("pro*:al*").unwrap();
        assert_eq!(
            units,
            vec![Unit::Pair {
                ty: glob("pro*", &[1]),
                label: glob("al*", &[2]),
            }]
        );
    }

    // --- apply_captures ---

    use crate::edit::QueryType;
    use crate::types::{
        Intrinsic, ItemId, ItemKind, Origin, SType, Tags, TypedTag,
    };

    fn reg() -> TagRegistry {
        TagRegistry::with_standard()
    }

    fn item_with(entries: Vec<TypedTag>) -> Item {
        let mut tags = Tags::new();
        for t in entries {
            tags.push(t, Origin::User);
        }
        Item {
            id: ItemId::Stored(1),
            item_kind: ItemKind::File,
            representative: vec![].into(),
            rank: 0,
            intrinsic: Intrinsic::default(),
            tags,
            item_count: None,
        }
    }

    fn edit_query(s: &str) -> EditQuery {
        super::super::parse::parse_edit_query(s, QueryType::Tag, &reg())
            .unwrap()
    }

    fn local_at(
        y: i32,
        m: u32,
        d: u32,
        h: u32,
    ) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        chrono::Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn single_match_binds_one_value() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "alpha")]);
        let parsed = edit_query("note:{1}");
        let (units, total) = collect_units("project:al*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "pha");
    }

    #[test]
    fn multiple_matches_expand_as_product() {
        let reg = reg();
        let item = item_with(vec![
            TypedTag::new("cat", "a"),
            TypedTag::new("cat", "b"),
            TypedTag::new("grp", "x"),
            TypedTag::new("grp", "y"),
        ]);
        let parsed = edit_query("note:{1}-{2}");
        let (units, total) = collect_units("cat:* | grp:*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        let mut values: Vec<String> = out
            .nodes
            .iter()
            .map(|n| n.label.as_ref().unwrap().value())
            .collect();
        values.sort();
        assert_eq!(values, vec!["a-x", "a-y", "b-x", "b-y"]);
    }

    #[test]
    fn axis_without_hits_stays_one_unbound_candidate() {
        let reg = reg();
        let item = item_with(vec![]);
        let parsed = edit_query("project:{1} static:y");
        let (units, total) = collect_units("project:*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].tag_type.value(), "static");
    }

    #[test]
    fn node_with_unbound_ref_is_dropped_from_output() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "alpha")]);
        let parsed = edit_query("x:{1} y:{2}");
        let (units, total) = collect_units("cat:* | project:al*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].tag_type.value(), "y");
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "pha");
    }

    #[test]
    fn bound_ref_rendering_to_empty_string_is_kept() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "tt")]);
        let parsed = edit_query("note:pre{1}post");
        let (units, total) = collect_units("project:tt*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "prepost");
    }

    #[test]
    fn pattern_inside_aggregate_binds_from_item_tags() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "alpha")]);
        let parsed = edit_query("x:{1}-{2}");
        let (units, total) = collect_units("count(pro*:al*) > 0").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "ject-pha");
    }

    #[test]
    fn pair_binds_only_when_type_and_label_hit_same_entry() {
        let reg = reg();
        let item = item_with(vec![
            TypedTag::new("zz_cat", "a1"),
            TypedTag::new("zz_dog", "b1"),
        ]);
        let parsed = edit_query("note:{1}_{2}");
        let (units, total) = collect_units("zz_*:a*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "cat_1");
    }

    #[test]
    fn pair_with_literal_side_skips_entries_of_other_types() {
        let reg = reg();
        let item = item_with(vec![
            TypedTag::new("zz_cat", "100"),
            TypedTag::new("zz_dog", "200"),
        ]);
        let parsed = edit_query("note:{1}");
        let (units, total) = collect_units("zz_*:100").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "cat");
    }

    #[test]
    fn capture_free_node_appears_once_across_sets() {
        let reg = reg();
        let item = item_with(vec![
            TypedTag::new("cat", "a"),
            TypedTag::new("cat", "b"),
        ]);
        let parsed = edit_query("note:{1} static:z");
        let (units, total) = collect_units("cat:*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        let static_count = out
            .nodes
            .iter()
            .filter(|n| n.tag_type.value() == "static")
            .count();
        let note_count = out
            .nodes
            .iter()
            .filter(|n| n.tag_type.value() == "note")
            .count();
        assert_eq!(static_count, 1);
        assert_eq!(note_count, 2);
    }

    #[test]
    fn identical_binding_from_two_entries_yields_one_node() {
        let reg = reg();
        let item = item_with(vec![
            TypedTag::new("project", "alpha"),
            TypedTag::new("project", "alpha"),
        ]);
        let parsed = edit_query("note:{1}");
        let (units, total) = collect_units("project:al*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "pha");
    }

    #[test]
    fn zero_reference_is_error() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "alpha")]);
        let parsed = edit_query("note:{0}");
        let (units, total) = collect_units("project:al*").unwrap();
        assert!(apply_captures(&item, &parsed, &units, total, &reg).is_err());
    }

    #[test]
    fn non_numeric_reference_is_error() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "alpha")]);
        let parsed = edit_query("note:{abc}");
        let (units, total) = collect_units("project:al*").unwrap();
        assert!(apply_captures(&item, &parsed, &units, total, &reg).is_err());
    }

    #[test]
    fn overflowing_reference_is_error() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "alpha")]);
        let parsed = edit_query("note:{99999999999999999999999999}");
        let (units, total) = collect_units("project:al*").unwrap();
        assert!(apply_captures(&item, &parsed, &units, total, &reg).is_err());
    }

    #[test]
    fn leading_zero_reference_is_accepted() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "alpha")]);
        let parsed = edit_query("note:{01}");
        let (units, total) = collect_units("project:al*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "pha");
    }

    #[test]
    fn reference_above_total_is_error() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "alpha")]);
        let parsed = edit_query("note:{2}");
        let (units, total) = collect_units("project:al*").unwrap();
        assert!(apply_captures(&item, &parsed, &units, total, &reg).is_err());
    }

    #[test]
    fn mtime_slot_glob_binds_slot_value() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new(
            SType::Mtime,
            local_at(2026, 8, 3, 12).timestamp(),
        )]);
        let parsed = edit_query("note:{1}");
        let (units, total) = collect_units("mtime:*-08-03").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "2026");
    }

    #[test]
    fn type_position_glob_expands_over_candidate_types() {
        let reg = reg();
        let item = item_with(vec![
            TypedTag::new("zz_extra", "txt"),
            TypedTag::new("zz_ex2", "nope"),
            TypedTag::new("zz_ex3", "txt"),
        ]);
        let parsed = edit_query("note:{1}");
        let (units, total) = collect_units("zz_*:txt").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        let mut values: Vec<String> = out
            .nodes
            .iter()
            .map(|n| n.label.as_ref().unwrap().value())
            .collect();
        values.sort();
        assert_eq!(values, vec!["ex3", "extra"]);
    }

    #[test]
    fn literal_type_yields_exactly_one_candidate() {
        let reg = reg();
        let item = item_with(vec![
            TypedTag::new("project", "alpha"),
            TypedTag::new("projectx", "foo"),
        ]);
        let p = Pattern::Glob {
            text: "project".to_string(),
            numbers: vec![],
        };
        let candidates = candidate_types(&reg, &item, &p);
        assert_eq!(candidates, vec![TagType::from("project")]);
    }

    #[test]
    fn unregistered_type_uses_default_capture() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("myproj", "alpha")]);
        let ty = TagType::from("myproj");
        let p = Pattern::Glob {
            text: "al*".to_string(),
            numbers: vec![1],
        };
        let via_fit = fit_label(&reg, &p, &ty, &item);
        let direct = tag::default_capture(&ty, "al*", &item);
        assert_eq!(via_fit, direct);
        assert_eq!(via_fit, vec![vec!["pha".to_string()]]);
    }

    #[test]
    fn definition_item_binds_via_type_fn() {
        let reg = reg();
        let item = Item {
            id: ItemId::Stored(1),
            item_kind: ItemKind::File,
            representative: vec![TypedTag::new(SType::Type, "project")].into(),
            rank: 0,
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            item_count: None,
        };
        let parsed = edit_query("note:{1}");
        let (units, total) = collect_units("type:proj*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "ect");
    }

    #[test]
    fn unquoted_glob_in_value_is_error() {
        let parsed = edit_query("cat:*");
        assert!(reject_glob(&parsed).is_err());
    }

    #[test]
    fn quoted_glob_in_value_is_error() {
        let parsed = edit_query(r#"cat:"*""#);
        assert!(reject_glob(&parsed).is_err());
    }

    #[test]
    fn escaped_glob_in_value_is_accepted() {
        let reg = reg();
        let item = item_with(vec![]);
        let parsed = edit_query(r"cat:\*");
        assert!(reject_glob(&parsed).is_ok());
        let (units, total) = collect_units("x:y").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "*");
    }

    #[test]
    fn metachar_from_bound_value_is_accepted() {
        let reg = reg();
        let item = item_with(vec![TypedTag::new("project", "a*b")]);
        let parsed = edit_query("note:{1}");
        assert!(reject_glob(&parsed).is_ok());
        let (units, total) = collect_units("project:a*").unwrap();
        let out = apply_captures(&item, &parsed, &units, total, &reg).unwrap();
        assert_eq!(out.nodes.len(), 1);
        assert_eq!(out.nodes[0].label.as_ref().unwrap().value(), "*b");
    }

    #[test]
    fn reject_glob_does_not_look_at_bindings() {
        let parsed = edit_query("note:x*{1}");
        assert!(reject_glob(&parsed).is_err());
    }
}
