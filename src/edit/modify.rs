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

// Volatile 登録時に item から注入する ModifyInjection 戦略のラベル（content / item_kind）。
fn injection_labels(item: &Item, registry: &TagRegistry) -> Vec<Label> {
    registry
        .iter_arcs()
        .filter_map(|f| {
            let e = f.edit()?;
            matches!(e.strategy(), EditStrategy::ModifyInjection)
                .then(|| e.inject(item))
                .flatten()
        })
        .collect()
}

// WriteAction（DB の Delete/Add encoding）を原子的なタグ操作へ平坦化したもの。
// 平坦化後の適用（apply）は完全にフラットな1段 match で済む。
enum TagDelta {
    Add(Label),
    DropType(TagType),
    DropTag(Label),
}

impl TagDelta {
    // WriteAction 1件を原子 delta 列へ平坦化する。
    fn flatten(action: WriteAction) -> Vec<TagDelta> {
        match action {
            WriteAction::Add { tags, .. } => tags
                .into_iter()
                .map(|op| match op {
                    TagOp::Append(l) | TagOp::Replace(l) => TagDelta::Add(l),
                })
                .collect(),
            WriteAction::Delete { tags, .. } => tags
                .into_iter()
                .map(|t| match t {
                    DeleteTarget::Type(tt) => TagDelta::DropType(tt),
                    DeleteTarget::Tag(l) => TagDelta::DropTag(l),
                })
                .collect(),
        }
    }

    // 作業集合へ適用する（フラットな1段 match）。
    fn apply(self, tags: &mut Vec<Label>) {
        match self {
            TagDelta::Add(l) => tags.push(l),
            TagDelta::DropType(t) => tags.retain(|l| l.tag_type() != t),
            TagDelta::DropTag(l) => tags.retain(|x| *x != l),
        }
    }
}

// scalar 結果の物理型タグ type:"integer" 等を value_type: へ正規化する。
// type: は定義参照関数と衝突するため、永続化前にリネームする。
fn rename_volatile_type(label: Label) -> Label {
    use crate::types::SType;
    if label.tag_type() == TagType::Base(SType::Type) {
        Label::Other(TagType::Custom("value_type".to_string()), label.value())
    } else {
        label
    }
}

// Volatile アイテムへの編集: directive の delta（into_actions 由来）を原子 delta へ平坦化し、
// item.tags を種に順次適用、注入を加えて単一 Add にする。
// 意味（Append/Replace/Untag）は into_actions が唯一源。
fn fold_volatile(
    item: &Item,
    actions: Vec<WriteAction>,
    registry: &TagRegistry,
) -> Vec<WriteAction> {
    let mut tags: Vec<Label> = item.tags.entries.iter()
        .map(|e| rename_volatile_type(e.label.clone()))
        .collect();
    actions
        .into_iter()
        .flat_map(TagDelta::flatten)
        .for_each(|d| d.apply(&mut tags));
    tags.extend(injection_labels(item, registry));
    vec![WriteAction::Add {
        item: item.id.clone(),
        tags: tags.into_iter().map(TagOp::Append).collect(),
    }]
}

pub fn modify(
    item: &Item,
    query: Option<&str>,
    query_type: QueryType,
    registry: &TagRegistry,
) -> Result<Vec<WriteAction>> {
    let directives = match query {
        Some(q) => tokenize(q)?
            .into_iter()
            .map(|(tag_type, value)| query_type.to_directive(tag_type, value, registry))
            .collect::<Result<Vec<_>>>()?,
        None => vec![],
    };
    let actions: Vec<WriteAction> = directives
        .into_iter()
        .map(|d| d.into_actions(&item.id, item))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    if item.id.is_volatile() {
        fold_volatile(item, actions, registry).to_ok()
    } else {
        actions.to_ok()
    }
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

    fn make_volatile_item(stype: SType, repr_value: &str) -> Item {
        use crate::types::{Intrinsic, LabelValue, Rank};
        let repr_label = Label::resolve(TagType::Base(stype), LabelValue::String(repr_value.to_string()));
        Item {
            id: ItemId::Volatile(0),
            item_kind: ItemKind::Volatile,
            representative: vec![repr_label],
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            item_count: None,
        }
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
            modify(&item, Some("project:A"), QueryType::Tag, &registry()).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Add { tags, .. } if matches!(&tags[0], TagOp::Append(_)))
        );
    }

    #[test]
    fn modify_tag_multiple_tokens() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, Some("project:A status:done"), QueryType::Tag, &registry())
                .unwrap();
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn modify_replace_rank_generates_delete_then_add() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, Some("rank:5"), QueryType::Tag, &registry()).unwrap();
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
            modify(&item, Some("filename:foo.txt"), QueryType::Tag, &registry())
                .is_err()
        );
    }

    #[test]
    fn modify_tag_modify_injection_is_error() {
        let item = make_item(1, vec![]);
        assert!(modify(&item, Some("item_kind:note"), QueryType::Tag, &registry())
            .is_err());
    }

    #[test]
    fn modify_tag_projection_is_error() {
        let item = make_item(1, vec![]);
        assert!(modify(&item, Some("project:"), QueryType::Tag, &registry()).is_err());
    }

    #[test]
    fn modify_untag_existing_label() {
        let label = Label::Other(
            TagType::from("project"),
            LabelValue::String("A".into()),
        );
        let item = make_item(1, vec![label.clone()]);
        let actions =
            modify(&item, Some("project:A"), QueryType::Untag, &registry()).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Tag(_)))
        );
    }

    #[test]
    fn modify_untag_nonexistent_label_is_noop() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, Some("project:Z"), QueryType::Untag, &registry()).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn modify_untag_projection() {
        let item = make_item(1, vec![]);
        let actions =
            modify(&item, Some("project:"), QueryType::Untag, &registry()).unwrap();
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
            Some("project:A status:done"),
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

    // ── Volatile item 登録注入テスト ─────────────────────────

    // Volatile は常に単一 Add（item.tags に into_actions の delta を畳み込んだ結果＋注入）。
    // DB Delete は出さない。
    fn add_tags(actions: &[WriteAction]) -> &[TagOp] {
        assert_eq!(actions.len(), 1, "Volatile must yield exactly one Add");
        let WriteAction::Add { tags, .. } = &actions[0] else { panic!("expected Add") };
        tags
    }
    fn has_append(tags: &[TagOp], pred: impl Fn(&Label) -> bool) -> bool {
        tags.iter().any(|t| matches!(t, TagOp::Append(l) if pred(l)))
    }

    #[test]
    fn modify_volatile_tag_def_replace_single_add() {
        let item = make_volatile_item(SType::TypedTag, "project:A");
        let actions = modify(&item, Some("rank:5"), QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| matches!(l, Label::Rank(5))));
        assert!(has_append(tags, |l| matches!(l, Label::Content(s) if s == "project:A")));
        assert!(has_append(tags, |l| matches!(l, Label::ItemKind(s) if s == "tag")));
    }

    // item.tags に同 type が既存なら Replace は EditQuery 側のみ採用（重複しない）
    #[test]
    fn modify_volatile_replace_dedups_against_item_tags() {
        use crate::types::Origin;
        let mut item = make_volatile_item(SType::TypedTag, "project:A");
        item.tags.push(Label::Rank(1), Origin::User);
        let actions = modify(&item, Some("rank:5"), QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        let ranks: Vec<_> = tags
            .iter()
            .filter(|t| matches!(t, TagOp::Append(Label::Rank(_))))
            .collect();
        assert_eq!(ranks.len(), 1, "old rank dropped, only EditQuery rank remains");
        assert!(has_append(tags, |l| matches!(l, Label::Rank(5))));
    }

    #[test]
    fn modify_volatile_tag_def_append_single_add() {
        let item = make_volatile_item(SType::TypedTag, "project:A");
        let actions = modify(&item, Some("project:X"), QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| matches!(l, Label::Content(s) if s == "project:A")));
        assert!(has_append(tags, |l| matches!(l, Label::ItemKind(s) if s == "tag")));
    }

    #[test]
    fn modify_volatile_type_def_single_add() {
        let item = make_volatile_item(SType::Type, "project");
        let actions = modify(&item, Some("project:X"), QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| matches!(l, Label::Content(s) if s == "project")));
        assert!(has_append(tags, |l| matches!(l, Label::ItemKind(s) if s == "type")));
    }

    #[test]
    fn modify_volatile_other_projection_single_add_note() {
        let item = make_volatile_item(SType::Parentdir, "/home/aki/projects");
        let actions = modify(&item, Some("project:X"), QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| matches!(l, Label::Content(s) if s == "/home/aki/projects")));
        assert!(has_append(tags, |l| matches!(l, Label::ItemKind(s) if s == "note")));
    }

    #[test]
    fn modify_stored_item_no_registration_action() {
        let item = make_item(1, vec![]);
        let actions = modify(&item, Some("project:X"), QueryType::Tag, &registry()).unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn modify_volatile_no_edit_query_registration_only() {
        let item = make_volatile_item(SType::TypedTag, "project:A");
        let actions = modify(&item, None, QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| matches!(l, Label::Content(s) if s == "project:A")));
        assert!(has_append(tags, |l| matches!(l, Label::ItemKind(s) if s == "tag")));
    }

    #[test]
    fn modify_stored_no_edit_query_is_noop() {
        let item = make_item(1, vec![]);
        let actions = modify(&item, None, QueryType::Tag, &registry()).unwrap();
        assert!(actions.is_empty());
    }

    // 複合 representative（複数要素）の Volatile item: item_kind=note, content=全要素連結。
    #[test]
    fn modify_volatile_multi_repr_is_note_with_joined_content() {
        use crate::types::{Intrinsic, LabelValue, Rank};
        let repr = vec![
            Label::resolve(TagType::Base(SType::TypedTag), LabelValue::String("project:A".to_string())),
            Label::resolve(TagType::Base(SType::TypedTag), LabelValue::String("status:done".to_string())),
        ];
        let item = Item {
            id: ItemId::Volatile(0),
            item_kind: ItemKind::Volatile,
            representative: repr,
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            item_count: None,
        };
        let actions = modify(&item, None, QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| matches!(l, Label::ItemKind(s) if s == "note")));
        assert!(has_append(tags, |l| matches!(l, Label::Content(s) if s == "project:A &: status:done")));
    }

    // scalar result の物理型タグ type:"integer" は value_type:"integer" にリネームされる。
    #[test]
    fn modify_volatile_type_tag_renamed_to_value_type() {
        use crate::types::Origin;
        let mut item = make_volatile_item(SType::TypedTag, "project:A");
        item.tags.push(
            Label::Other(TagType::Base(SType::Type), LabelValue::String("integer".to_string())),
            Origin::System,
        );
        let actions = modify(&item, None, QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(
            !has_append(tags, |l| l.tag_type() == TagType::Base(SType::Type)),
            "type: tag must not appear after rename"
        );
        assert!(
            has_append(tags, |l| l.tag_type() == TagType::Custom("value_type".to_string())),
            "value_type: tag must appear"
        );
    }
}
