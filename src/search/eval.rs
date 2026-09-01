// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::db::Store;
use crate::query::ast::{
    AggregationNode, CalculationNode, ComparisonNode, NestNode, Operand,
    QueryNode,
};
use crate::query::error::WarningSink;
use crate::query::fetcher::Fetcher;
use crate::query::lens_resolver::Resolver;
use crate::response::Item;
use crate::tag::TagRegistry;
use crate::types::{ItemKind, TypedTag};
use anyhow::Result;

/// Glob メタ文字（`\`, `*`, `?`, `[`, `]`）をバックスラッシュでエスケープします。
pub fn escape_glob_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 検索結果の Item 構造体を TTQL の QueryNode 項へ翻訳します。
pub fn item_to_query_term(item: &Item) -> QueryNode {
    // スカラー集約・計算結果が tags 内の value タグに格納されている場合
    if let Some(val_tag) = item
        .tags
        .entries
        .iter()
        .find(|e| e.typed_tag.tag_type().as_str() == "value")
    {
        return QueryNode::TypedTag(val_tag.typed_tag.clone());
    }

    match item.item_kind {
        ItemKind::Tag => {
            let tag_str = item
                .representative
                .tags
                .first()
                .map(|t| t.as_str())
                .or_else(|| {
                    item.tags
                        .entries
                        .iter()
                        .find(|e| {
                            matches!(
                                e.typed_tag.tag_type().as_str(),
                                "content" | "name" | "tag"
                            )
                        })
                        .map(|e| e.typed_tag.as_str())
                })
                .unwrap_or_default();
            if let Some((t, l)) = tag_str.split_once(':') {
                QueryNode::TypedTag(TypedTag::new(
                    escape_glob_literal(t),
                    escape_glob_literal(l),
                ))
            } else {
                QueryNode::Or(vec![])
            }
        }
        ItemKind::Type => {
            let type_name = item
                .representative
                .tags
                .first()
                .map(|t| t.as_str())
                .or_else(|| {
                    item.tags
                        .entries
                        .iter()
                        .find(|e| {
                            matches!(
                                e.typed_tag.tag_type().as_str(),
                                "content" | "name"
                            )
                        })
                        .map(|e| e.typed_tag.as_str())
                })
                .unwrap_or_default();
            if type_name.is_empty() {
                QueryNode::Or(vec![])
            } else {
                QueryNode::TypedTag(TypedTag::new(
                    escape_glob_literal(&type_name),
                    "*",
                ))
            }
        }
        ItemKind::File | ItemKind::Note => {
            if item.id.is_stored() {
                QueryNode::TypedTag(TypedTag::new(
                    "item_id",
                    item.id.as_i64().to_string(),
                ))
            } else {
                // 揮発性アイテムは自己参照不可のため空条件に倒す
                QueryNode::Or(vec![])
            }
        }
        _ if item.representative.tags.len() > 1 => {
            let nodes = item
                .representative
                .tags
                .iter()
                .cloned()
                .map(|t| {
                    QueryNode::TypedTag(TypedTag::new(
                        escape_glob_literal(t.tag_type().as_str()),
                        escape_glob_literal(&t.as_str()),
                    ))
                })
                .collect();
            QueryNode::And(nodes)
        }
        _ if item.representative.tags.len() == 1 => {
            let tag = &item.representative.tags[0];
            match tag.tag_type().as_str() {
                "value" => QueryNode::TypedTag(tag.clone()),
                "label" => QueryNode::TypedTag(TypedTag::new(
                    "*",
                    escape_glob_literal(&tag.as_str()),
                )),
                _ => QueryNode::TypedTag(TypedTag::new(
                    escape_glob_literal(tag.tag_type().as_str()),
                    escape_glob_literal(&tag.as_str()),
                )),
            }
        }
        _ => QueryNode::Or(vec![]),
    }
}

/// 式内部の Operand に対する Eval 再帰走査と展開を行います。
pub fn expand_eval_operand(
    op: Operand,
    store: &Store,
    registry: &TagRegistry,
    sink: &mut dyn WarningSink,
) -> Result<Operand> {
    match op {
        Operand::Query(q) => {
            let expanded = expand_eval(*q, store, registry, sink)?;
            Ok(Operand::Query(Box::new(expanded)))
        }
        Operand::Calculation(calc) => {
            let left = expand_eval_operand(calc.left, store, registry, sink)?;
            let right = expand_eval_operand(calc.right, store, registry, sink)?;
            Ok(Operand::Calculation(Box::new(CalculationNode {
                left,
                op: calc.op,
                right,
            })))
        }
        Operand::Aggregation(agg) => {
            let exp_agg = match *agg {
                AggregationNode::Count(inner) => AggregationNode::Count(
                    Box::new(expand_eval(*inner, store, registry, sink)?),
                ),
                AggregationNode::Arithmetic { op, inner } => {
                    AggregationNode::Arithmetic {
                        op,
                        inner: Box::new(expand_eval(
                            *inner, store, registry, sink,
                        )?),
                    }
                }
            };
            Ok(Operand::Aggregation(Box::new(exp_agg)))
        }
        other => Ok(other),
    }
}

/// AST 内に存在するすべての `QueryNode::Eval` を事前実行し、検索結果のクエリ項へ置換します。
pub fn expand_eval(
    node: QueryNode,
    store: &Store,
    registry: &TagRegistry,
    sink: &mut dyn WarningSink,
) -> Result<QueryNode> {
    match node {
        QueryNode::Eval(inner) => {
            let expanded_inner = expand_eval(*inner, store, registry, sink)?;
            let resolver = Resolver::from_node(expanded_inner, registry, sink)?;
            let fetcher = Fetcher::new(&resolver, &store.conn);
            let items = fetcher.fetch_for_eval(0, 0)?;
            if items.is_empty() {
                return Ok(QueryNode::Or(vec![]));
            }
            let terms: Vec<QueryNode> =
                items.iter().map(item_to_query_term).collect();
            Ok(QueryNode::Or(terms))
        }
        QueryNode::And(nodes) => {
            let expanded = nodes
                .into_iter()
                .map(|n| expand_eval(n, store, registry, sink))
                .collect::<Result<Vec<_>>>()?;
            Ok(QueryNode::And(expanded))
        }
        QueryNode::Or(nodes) => {
            let expanded = nodes
                .into_iter()
                .map(|n| expand_eval(n, store, registry, sink))
                .collect::<Result<Vec<_>>>()?;
            Ok(QueryNode::Or(expanded))
        }
        QueryNode::Difference(l, r) => {
            let exp_l = expand_eval(*l, store, registry, sink)?;
            let exp_r = expand_eval(*r, store, registry, sink)?;
            Ok(QueryNode::Difference(Box::new(exp_l), Box::new(exp_r)))
        }
        QueryNode::Nest(nest) => {
            let exp_left = nest
                .left
                .map(|l| expand_eval(*l, store, registry, sink))
                .transpose()?
                .map(Box::new);
            let exp_right =
                expand_eval_operand(nest.right, store, registry, sink)?;
            Ok(QueryNode::Nest(NestNode {
                left: exp_left,
                right: exp_right,
            }))
        }
        QueryNode::Comparison(cmp) => {
            let first = expand_eval_operand(cmp.first, store, registry, sink)?;
            let mut rest = Vec::with_capacity(cmp.rest.len());
            for (op, r_op) in cmp.rest {
                rest.push((
                    op,
                    expand_eval_operand(r_op, store, registry, sink)?,
                ));
            }
            Ok(QueryNode::Comparison(ComparisonNode { first, rest }))
        }
        QueryNode::Aggregation(agg) => {
            let exp_agg = match agg {
                AggregationNode::Count(inner) => AggregationNode::Count(
                    Box::new(expand_eval(*inner, store, registry, sink)?),
                ),
                AggregationNode::Arithmetic { op, inner } => {
                    AggregationNode::Arithmetic {
                        op,
                        inner: Box::new(expand_eval(
                            *inner, store, registry, sink,
                        )?),
                    }
                }
            };
            Ok(QueryNode::Aggregation(exp_agg))
        }
        QueryNode::DateTimeRange { first, op, range } => {
            let exp_first = expand_eval_operand(first, store, registry, sink)?;
            Ok(QueryNode::DateTimeRange {
                first: exp_first,
                op,
                range,
            })
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::Item;
    use crate::types::{ItemId, ItemKind, Origin, TypedTag};

    #[test]
    fn test_translate_tag_definition_splits_colon_and_escapes() {
        let mut item = Item::new_empty(ItemId::Stored(1), ItemKind::Tag);
        item.representative =
            vec![TypedTag::new("tag", "proj*ect:core*")].into();
        let node = item_to_query_term(&item);
        assert_eq!(
            node,
            QueryNode::TypedTag(TypedTag::new("proj\\*ect", "core\\*"))
        );
    }

    #[test]
    fn test_translate_type_definition_to_wildcard() {
        let mut item = Item::new_empty(ItemId::Stored(1), ItemKind::Type);
        item.representative = vec![TypedTag::new("type", "proj*ect")].into();
        let node = item_to_query_term(&item);
        assert_eq!(node, QueryNode::TypedTag(TypedTag::new("proj\\*ect", "*")));
    }

    #[test]
    fn test_translate_stored_vs_volatile_file_item() {
        let item_stored = Item::new_empty(ItemId::Stored(42), ItemKind::File);
        assert_eq!(
            item_to_query_term(&item_stored),
            QueryNode::TypedTag(TypedTag::new("item_id", "42"))
        );

        let item_volatile =
            Item::new_empty(ItemId::Volatile(1), ItemKind::File);
        assert_eq!(item_to_query_term(&item_volatile), QueryNode::Or(vec![]));
    }

    #[test]
    fn test_translate_value_from_tags_and_representative() {
        let mut item_val =
            Item::new_empty(ItemId::Stored(1), ItemKind::Volatile);
        item_val
            .tags
            .push(TypedTag::new("value", "-1"), Origin::Builtin);
        assert_eq!(
            item_to_query_term(&item_val),
            QueryNode::TypedTag(TypedTag::new("value", "-1"))
        );

        let mut item_lbl =
            Item::new_empty(ItemId::Stored(1), ItemKind::Volatile);
        item_lbl.representative = vec![TypedTag::new("label", "foo*")].into();
        assert_eq!(
            item_to_query_term(&item_lbl),
            QueryNode::TypedTag(TypedTag::new("*", "foo\\*"))
        );
    }

    #[test]
    fn test_escape_glob_literal_escapes_all_meta_chars() {
        assert_eq!(escape_glob_literal(r"a*b?c[d]e\f"), r"a\*b\?c\[d\]e\\f");
    }
}
