use super::{
    write::{DeleteTarget, TagOp, WriteAction},
    EditStrategy, QueryType,
};
use crate::response::Item;
use crate::tag::TagRegistry;
use crate::types::{ItemId, Label, LabelValue, TagType};
use crate::util::DotOk;
use anyhow::{bail, Result};

// ──────────────────────────────────────────────
// 内部型
// ──────────────────────────────────────────────

// Tag/Untag と EditStrategy を組み合わせた解決済み編集指示。
// into_actions がすべての分岐を1段フラットマッチで担う。
enum Directive {
    Tag(Label, EditStrategy),
    DeleteType(TagType),
    DeleteTag(Label),
}

impl Directive {
    // Tag/Untag × strategy の全分岐をここ1箇所で処理する。
    // DeleteTag は item.tags に存在する場合のみ Delete を生成（存在しなければ no-op）。
    fn into_actions(
        self,
        id: &ItemId,
        item: &Item,
    ) -> Result<Vec<WriteAction>> {
        match self {
            Directive::Tag(label, EditStrategy::Append) => vec![WriteAction::Add {
                item: id.clone(),
                tags: vec![TagOp::Append(label)],
            }],
            Directive::Tag(label, EditStrategy::Replace) => vec![
                WriteAction::Delete {
                    item: id.clone(),
                    tags: vec![DeleteTarget::Type(label.tag_type())],
                },
                WriteAction::Add {
                    item: id.clone(),
                    tags: vec![TagOp::Replace(label)],
                },
            ],
            Directive::Tag(label, EditStrategy::ModifyInjection) => bail!(
                "tag type '{}' cannot be set via EditQuery (ModifyInjection)",
                label.tag_type()
            ),
            Directive::Tag(label, EditStrategy::Relocate | EditStrategy::SetFileAttr) => bail!(
                "tag type '{}' requires fs_operate, not modify (plan contract violation)",
                label.tag_type()
            ),
            Directive::DeleteType(tag_type) => vec![WriteAction::Delete {
                item: id.clone(),
                tags: vec![DeleteTarget::Type(tag_type)],
            }],
            Directive::DeleteTag(label) => item.tags.entries.iter().any(|e| e.label == label)
                .then(|| WriteAction::Delete {
                    item: id.clone(),
                    tags: vec![DeleteTarget::Tag(label)],
                })
                .into_iter()
                .collect(),
        }
        .to_ok()
    }
}

impl QueryType {
    // (query_type, tag_type, value) → Directive
    fn to_directive(
        &self,
        tag_type: TagType,
        value: Option<LabelValue>,
        registry: &TagRegistry,
    ) -> Result<Directive> {
        match (self, value) {
            (QueryType::Tag, None) => bail!(
                "Projection '{}:' is not allowed in EditQuery (Tag direction)",
                tag_type.as_str()
            ),
            (QueryType::Tag, Some(v)) => {
                let label = Label::resolve(tag_type, v);
                let strategy = get_strategy(&label, registry);
                Ok(Directive::Tag(label, strategy))
            }
            (QueryType::Untag, None) => Ok(Directive::DeleteType(tag_type)),
            (QueryType::Untag, Some(v)) => {
                Ok(Directive::DeleteTag(Label::resolve(tag_type, v)))
            }
        }
    }
}

// ──────────────────────────────────────────────
// ヘルパー
// ──────────────────────────────────────────────

fn parse_value(s: &str) -> LabelValue {
    if let Ok(i) = s.parse::<i64>() {
        LabelValue::Integer(i)
    } else {
        LabelValue::String(s.to_string())
    }
}

fn tokenize(query: &str) -> Result<Vec<(TagType, Option<LabelValue>)>> {
    let raw: Vec<&str> = query
        .split(|c: char| c == '|' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if raw.is_empty() {
        bail!("empty query");
    }

    raw.iter()
        .map(|tok| {
            if tok.contains('&') {
                bail!("'&' is not allowed in EditQuery/TagQuery: {tok:?}");
            }
            let (type_str, value_str) =
                tok.split_once(':').ok_or_else(|| {
                    anyhow::anyhow!("invalid token (no colon): {tok:?}")
                })?;
            let value = (!value_str.is_empty()).then(|| parse_value(value_str));
            Ok((TagType::from(type_str), value))
        })
        .collect()
}

// registry から label の EditStrategy を取り出す純粋ヘルパー。
// 未登録のカスタム型はデフォルト Append。
fn get_strategy(label: &Label, registry: &TagRegistry) -> EditStrategy {
    registry
        .get(label.tag_type().as_str())
        .and_then(|f| f.edit())
        .map(|e| e.strategy())
        .unwrap_or(EditStrategy::Append)
}

// ──────────────────────────────────────────────
// 公開 API
// ──────────────────────────────────────────────

pub fn modify(
    item: &Item,
    query: &str,
    query_type: QueryType,
    registry: &TagRegistry,
) -> Result<Vec<WriteAction>> {
    tokenize(query)?
        .into_iter()
        .map(|(tag_type, value)| {
            query_type.to_directive(tag_type, value, registry)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|d| d.into_actions(&item.id, item))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .to_ok()
}

// ──────────────────────────────────────────────
// テスト
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::Item;
    use crate::tag::TagRegistry;
    use crate::types::{
        ItemId, ItemKind, Label, LabelValue, SType, TagType, Tags,
    };

    fn make_item(item_id: i64, labels: Vec<Label>) -> Item {
        use crate::types::{Intrinsic, Origin, Rank};
        let mut tags = Tags::new();
        for label in labels {
            tags.push(label, Origin::User);
        }
        Item {
            id: ItemId::Stored(item_id),
            item_kind: ItemKind::File,
            representative: vec![],
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags,
            item_count: None,
        }
    }

    fn registry() -> TagRegistry {
        TagRegistry::with_standard()
    }

    // ── tokenize テスト ───────────────────────

    #[test]
    fn tokenize_space_and_pipe_are_same() {
        let space = tokenize("project:A status:done").unwrap();
        let pipe = tokenize("project:A | status:done").unwrap();
        assert_eq!(space, pipe);
        assert_eq!(space.len(), 2);
    }

    #[test]
    fn tokenize_types_integer_value() {
        let tokens = tokenize("rank:5").unwrap();
        assert_eq!(
            tokens,
            vec![(TagType::from("rank"), Some(LabelValue::Integer(5)))]
        );
    }

    #[test]
    fn tokenize_string_value() {
        let tokens = tokenize("project:A").unwrap();
        assert_eq!(
            tokens,
            vec![(
                TagType::from("project"),
                Some(LabelValue::String("A".into()))
            )]
        );
    }

    #[test]
    fn tokenize_projection_has_no_value() {
        let tokens = tokenize("project:").unwrap();
        assert_eq!(tokens, vec![(TagType::from("project"), None)]);
    }

    #[test]
    fn tokenize_ampersand_is_error() {
        assert!(tokenize("project:A & status:done").is_err());
    }

    #[test]
    fn tokenize_no_colon_is_error() {
        assert!(tokenize("project").is_err());
    }

    #[test]
    fn tokenize_empty_is_error() {
        assert!(tokenize("").is_err());
    }

    // ── modify テスト ─────────────────────────

    #[test]
    fn modify_append_custom_type() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, "project:A", QueryType::Tag, &registry()).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Add { tags, .. } if matches!(&tags[0], TagOp::Append(_)))
        );
    }

    #[test]
    fn modify_tag_multiple_tokens() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, "project:A status:done", QueryType::Tag, &registry())
                .unwrap();
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn modify_replace_rank_generates_delete_then_add() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, "rank:5", QueryType::Tag, &registry()).unwrap();
        assert_eq!(actions.len(), 2);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Type(TagType::Base(SType::Rank))))
        );
        assert!(
            matches!(&actions[1], WriteAction::Add { tags, .. } if matches!(&tags[0], TagOp::Replace(Label::Rank(5))))
        );
    }

    #[test]
    fn modify_tag_relocate_is_error() {
        let item = make_item(1, vec![]);
        assert!(
            modify(&item, "filename:foo.txt", QueryType::Tag, &registry())
                .is_err()
        );
    }

    #[test]
    fn modify_tag_modify_injection_is_error() {
        let item = make_item(1, vec![]);
        assert!(modify(&item, "item_kind:note", QueryType::Tag, &registry())
            .is_err());
    }

    #[test]
    fn modify_tag_projection_is_error() {
        let item = make_item(1, vec![]);
        assert!(modify(&item, "project:", QueryType::Tag, &registry()).is_err());
    }

    #[test]
    fn modify_untag_existing_label() {
        let label = Label::Other(
            TagType::from("project"),
            LabelValue::String("A".into()),
        );
        let item = make_item(1, vec![label.clone()]);
        let actions =
            modify(&item, "project:A", QueryType::Untag, &registry()).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Tag(_)))
        );
    }

    #[test]
    fn modify_untag_nonexistent_label_is_noop() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, "project:Z", QueryType::Untag, &registry()).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn modify_untag_projection() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, "project:", QueryType::Untag, &registry()).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Type(_)))
        );
    }

    #[test]
    fn modify_untag_multiple_gives_separate_deletes() {
        let labels = vec![
            Label::Other(
                TagType::from("project"),
                LabelValue::String("A".into()),
            ),
            Label::Other(
                TagType::from("status"),
                LabelValue::String("done".into()),
            ),
        ];
        let item = make_item(1, labels);
        let actions = modify(
            &item,
            "project:A status:done",
            QueryType::Untag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 2);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Tag(_)))
        );
        assert!(
            matches!(&actions[1], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Tag(_)))
        );
    }
}
