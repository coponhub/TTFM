use super::{
    parse::{EditQuery, EditQueryLeaf},
    write::{DeleteTarget, TagOp, WriteAction},
    EditStrategy, QueryType,
};
use crate::query::ast::{
    BasicOp, ComparisonNode, ComparisonOp, Operand, QueryNode,
};
use crate::response::Item;
use crate::tag::TagRegistry;
use crate::types::{Bitical, ItemId, Label, SType, TagType, TypedTag};
use crate::util::DotOk;
use anyhow::{bail, Result};

// ──────────────────────────────────────────────
// 内部型
// ──────────────────────────────────────────────

// Tag/Untag と EditStrategy を組み合わせた解決済み編集指示。
// into_actions がすべての分岐を1段フラットマッチで担う。
enum Directive {
    Tag(TypedTag, EditStrategy),
    DeleteType(TagType),
    DeleteTag(TypedTag),
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
            Directive::Tag(tag, EditStrategy::Append) => vec![WriteAction::Add {
                item: id.clone(),
                tags: vec![TagOp::Append(tag)],
            }],
            Directive::Tag(tag, EditStrategy::Replace) => vec![
                WriteAction::Delete {
                    item: id.clone(),
                    tags: vec![DeleteTarget::Type(tag.tag_type())],
                },
                WriteAction::Add {
                    item: id.clone(),
                    tags: vec![TagOp::Replace(tag)],
                },
            ],
            Directive::Tag(tag, EditStrategy::ModifyInjection) => bail!(
                "tag type '{}' cannot be set via EditQuery (ModifyInjection)",
                tag.tag_type()
            ),
            Directive::Tag(tag, EditStrategy::Relocate | EditStrategy::SetFileAttr) => bail!(
                "tag type '{}' requires fs_operate, not modify (plan contract violation)",
                tag.tag_type()
            ),
            Directive::Tag(tag, EditStrategy::RemoveOnly) => bail!(
                "tag type '{}' cannot be added, only removed (RemoveOnly)",
                tag.tag_type()
            ),
            Directive::DeleteType(tag_type) => vec![WriteAction::Delete {
                item: id.clone(),
                tags: vec![DeleteTarget::Type(tag_type)],
            }],
            Directive::DeleteTag(tag) => item.tags.entries.iter().any(|e| e.typed_tag == tag)
                .then(|| WriteAction::Delete {
                    item: id.clone(),
                    tags: vec![DeleteTarget::Tag(tag)],
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
        value: Option<Label>,
        registry: &TagRegistry,
    ) -> Result<Directive> {
        match (self, value) {
            (QueryType::Tag, None) => bail!(
                "Projection '{}:' is not allowed in EditQuery (Tag direction)",
                tag_type.as_str()
            ),
            (QueryType::Tag, Some(v)) => {
                let strategy = get_strategy(&tag_type, registry)?;
                let tag = TypedTag::retag(tag_type, &v);
                let tag = interpret_tag_value(tag, registry)?;
                Ok(Directive::Tag(tag, strategy))
            }
            (QueryType::Untag, None) => {
                check_untag_allowed(&tag_type, registry)?;
                Ok(Directive::DeleteType(tag_type))
            }
            (QueryType::Untag, Some(v)) => {
                check_untag_allowed(&tag_type, registry)?;
                Ok(Directive::DeleteTag(TypedTag::retag(tag_type, &v)))
            }
        }
    }
}

// ──────────────────────────────────────────────
// ヘルパー
// ──────────────────────────────────────────────

fn build_leaf_label(l: &EditQueryLeaf) -> Result<Label> {
    let s = l.value();
    if l.quoted {
        return Label::other(Bitical::String(s)).to_ok();
    }
    use crate::query::format::OperandFormat;
    match Bitical::parse(&s) {
        Some(Ok(b)) => {
            crate::query::format::attach_formatted_node(Label::other(b)).to_ok()
        }
        Some(Err(e)) => bail!("invalid value {s:?}: {e}"),
        None => bail!("invalid value {s:?}"),
    }
}

fn interpret_tag_value(
    tag: TypedTag,
    registry: &TagRegistry,
) -> Result<TypedTag> {
    let Some(f) = registry.get(tag.tag_type().as_str()) else {
        return tag.to_ok();
    };
    let predicate = f.query().interpret(
        &Operand::TypeRef(tag.tag_type()),
        ComparisonOp::Label(BasicOp::Eq),
        &tag.label,
    )?;
    let not_single = || {
        crate::query::error::tag_value_not_a_single_value(
            tag.tag_type().as_str(),
            &tag.label.as_str(),
        )
    };
    let QueryNode::Comparison(ComparisonNode { rest, .. }) = predicate else {
        return Err(not_single());
    };
    let [(ComparisonOp::Label(BasicOp::Eq), Operand::Literal(value))] =
        rest.as_slice()
    else {
        return Err(not_single());
    };
    TypedTag::retag(tag.tag_type(), value).to_ok()
}

fn get_strategy(
    tag_type: &TagType,
    registry: &TagRegistry,
) -> Result<EditStrategy> {
    match registry.get(tag_type.as_str()) {
        Some(f) => match f.edit() {
            Some(e) => Ok(e.strategy()),
            None => bail!(
                "tag type '{}' is registered but not editable (Forbidden)",
                tag_type
            ),
        },
        None => Ok(EditStrategy::Append),
    }
}

pub struct ResolvedNode {
    pub tag_type: TagType,
    pub label: Option<Label>,
    pub strategy: EditStrategy,
}

pub fn resolve_nodes(
    query: Option<&EditQuery>,
    query_type: QueryType,
    registry: &TagRegistry,
) -> Result<Vec<ResolvedNode>> {
    query
        .map(|q| q.nodes.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|n| {
            let tag_type = TagType::from(n.tag_type.value().as_str());
            if tag_type.as_str().is_empty() {
                bail!("empty tag type in EditQuery");
            }
            let label = n.label.as_ref().map(build_leaf_label).transpose()?;
            if matches!(query_type, QueryType::Untag) {
                check_untag_allowed(&tag_type, registry)?;
            }
            let strategy = get_strategy(&tag_type, registry)?;
            Ok(ResolvedNode {
                tag_type,
                label,
                strategy,
            })
        })
        .collect()
}

fn check_untag_allowed(
    tag_type: &TagType,
    registry: &TagRegistry,
) -> Result<()> {
    let Some(f) = registry.get(tag_type.as_str()) else {
        return Ok(());
    };
    let Some(e) = f.edit() else {
        bail!(
            "tag type '{tag_type}' is registered but not editable (Forbidden)"
        );
    };
    if e.can_untag() {
        return Ok(());
    }
    match e.strategy() {
        EditStrategy::Relocate => bail!(
            "tag type '{tag_type}' is only part of a location; \
             remove 'path:' to remove the location itself"
        ),
        _ => bail!("tag type '{tag_type}' has no removal to perform"),
    }
}

// ──────────────────────────────────────────────
// 公開 API
// ──────────────────────────────────────────────

// name が無い場合に representative または item_kind から name タグを out に補完する。
// existing は既存タグ（directives 適用済み）、out は注入先（registry ラベルが既にある）。
fn inject_name(item: &Item, existing: &[TypedTag], out: &mut Vec<TypedTag>) {
    if existing
        .iter()
        .chain(out.iter())
        .any(|t| t.tag_type() == TagType::Base(SType::Name))
    {
        return;
    }
    let name = if let Some(repr) = item.representative.tags.first() {
        repr.value().as_display_name()
    } else {
        out.iter()
            .find(|t| t.tag_type() == TagType::Base(SType::ItemKind))
            .map(|t| t.as_str())
            .unwrap_or_default()
    };
    if !name.is_empty() {
        out.push(TypedTag::new(SType::Name, name));
    }
}

// Volatile 登録時に item から注入するタグ群。
// ModifyInjection 戦略のタグ（content / item_kind）と name 補完をまとめて返す。
fn injection_labels(
    item: &Item,
    current_tags: &[TypedTag],
    registry: &TagRegistry,
) -> Vec<TypedTag> {
    let mut tags: Vec<TypedTag> = registry
        .iter_arcs()
        .filter_map(|f| {
            let e = f.edit()?;
            matches!(e.strategy(), EditStrategy::ModifyInjection)
                .then(|| e.inject(item).map(|l| TypedTag::retag(f.name(), &l)))
                .flatten()
        })
        .collect();
    inject_name(item, current_tags, &mut tags);
    tags
}

// WriteAction（DB の Delete/Add encoding）を原子的なタグ操作へ平坦化したもの。
// 平坦化後の適用（apply）は完全にフラットな1段 match で済む。
enum TagDelta {
    Add(TypedTag),
    DropType(TagType),
    DropTag(TypedTag),
}

impl TagDelta {
    // WriteAction 1件を原子 delta 列へ平坦化する。
    fn flatten(action: WriteAction) -> Vec<TagDelta> {
        match action {
            WriteAction::Add { tags, .. } => tags
                .into_iter()
                .map(|op| match op {
                    TagOp::Append(t) | TagOp::Replace(t) => TagDelta::Add(t),
                })
                .collect(),
            WriteAction::Delete { tags, .. } => tags
                .into_iter()
                .map(|t| match t {
                    DeleteTarget::Type(tt) => TagDelta::DropType(tt),
                    DeleteTarget::Tag(t) => TagDelta::DropTag(t),
                })
                .collect(),
        }
    }

    // 作業集合へ適用する（フラットな1段 match）。
    fn apply(self, tags: &mut Vec<TypedTag>) {
        match self {
            TagDelta::Add(t) => tags.push(t),
            TagDelta::DropType(tt) => tags.retain(|t| t.tag_type() != tt),
            TagDelta::DropTag(t) => tags.retain(|x| *x != t),
        }
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
    let mut tags: Vec<TypedTag> = item
        .tags
        .entries
        .iter()
        .map(|e| e.typed_tag.clone())
        .collect();
    actions
        .into_iter()
        .flat_map(TagDelta::flatten)
        .for_each(|d| d.apply(&mut tags));
    let injections = injection_labels(item, &tags, registry);
    tags.extend(injections);
    vec![WriteAction::Add {
        item: item.id.clone(),
        tags: tags.into_iter().map(TagOp::Append).collect(),
    }]
}

pub fn modify(
    item: &Item,
    nodes: &[ResolvedNode],
    query_type: QueryType,
    registry: &TagRegistry,
) -> Result<Vec<WriteAction>> {
    let directives = nodes
        .iter()
        .map(|n| {
            query_type.to_directive(
                n.tag_type.clone(),
                n.label.clone(),
                registry,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let actions: Vec<WriteAction> = directives
        .into_iter()
        .map(|d| d.into_actions(&item.id, item))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    if !item.id.is_stored() {
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
    use crate::types::{Bitical, ItemId, ItemKind, SType, TagType, Tags};

    fn make_item(item_id: i64, typed_tags: Vec<TypedTag>) -> Item {
        use crate::types::{Intrinsic, Origin, Rank};
        let mut tags = Tags::new();
        for tag in typed_tags {
            tags.push(tag, Origin::User);
        }
        Item {
            id: ItemId::Stored(item_id),
            item_kind: ItemKind::File,
            representative: vec![].into(),
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags,
            item_count: None,
        }
    }

    fn registry() -> TagRegistry {
        TagRegistry::with_standard()
    }

    fn eq(q: &str, qt: QueryType) -> EditQuery {
        crate::edit::parse::parse_edit_query(q, qt, &registry()).unwrap()
    }

    fn modify_query(
        item: &Item,
        query: Option<&EditQuery>,
        query_type: QueryType,
        registry: &TagRegistry,
    ) -> Result<Vec<WriteAction>> {
        let nodes = resolve_nodes(query, query_type, registry)?;
        modify(item, &nodes, query_type, registry)
    }

    fn leaf(q: &str) -> EditQueryLeaf {
        eq(&format!("t:{q}"), QueryType::Tag)
            .nodes
            .into_iter()
            .next()
            .unwrap()
            .label
            .unwrap()
    }

    fn make_volatile_item(stype: SType, repr_value: &str) -> Item {
        use crate::types::{Bitical, Intrinsic, Rank};
        let repr_label = TypedTag::new(
            TagType::Base(stype),
            Bitical::String(repr_value.to_string()),
        );
        Item {
            id: ItemId::Volatile(0),
            item_kind: ItemKind::Volatile,
            representative: vec![repr_label].into(),
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            item_count: None,
        }
    }

    // ── build_leaf_label テスト ───────────────────

    #[test]
    fn integer_value_is_typed_as_integer() {
        let label = build_leaf_label(&leaf("5")).unwrap();
        assert_eq!(label, Label::other(Bitical::Integer(5)));
    }

    #[test]
    fn double_value_is_typed_as_double() {
        let label = build_leaf_label(&leaf("42.1")).unwrap();
        assert_eq!(label, Label::other(Bitical::Double(42.1)));
    }

    #[test]
    fn boolean_value_is_typed_as_boolean() {
        let label = build_leaf_label(&leaf("true")).unwrap();
        assert_eq!(label, Label::other(Bitical::Boolean(true)));
    }

    #[test]
    fn quoted_value_stays_string() {
        let label = build_leaf_label(&leaf("\"42\"")).unwrap();
        assert_eq!(label, Label::other(Bitical::String("42".into())));
    }

    #[test]
    fn braced_value_renders_as_literal() {
        let label = build_leaf_label(&leaf("{1}")).unwrap();
        assert_eq!(label, Label::other(Bitical::String("{1}".into())));
    }

    // ── modify テスト ─────────────────────────

    #[test]
    fn modify_append_custom_type() {
        let item = make_item(1, vec![]);
        let actions = modify_query(
            &item,
            Some(&eq("project:A", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Add { tags, .. } if matches!(&tags[0], TagOp::Append(_)))
        );
    }

    #[test]
    fn modify_tag_multiple_tokens() {
        let item = make_item(1, vec![]);
        let actions = modify_query(
            &item,
            Some(&eq("project:A status:done", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn modify_replace_rank_generates_delete_then_add() {
        let item = make_item(1, vec![]);
        let actions = modify_query(
            &item,
            Some(&eq("rank:5", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 2);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Type(TagType::Base(SType::Rank))))
        );
        assert!(
            matches!(&actions[1], WriteAction::Add { tags, .. } if matches!(&tags[0], TagOp::Replace(t) if t.tag_type() == TagType::Base(SType::Rank) && t.label.as_i64() == 5))
        );
    }

    #[test]
    fn interpret_tag_value_applies_tagfn_interpret() {
        let label =
            TypedTag::new(TagType::from("size"), Bitical::String("1k".into()));
        let interpreted = interpret_tag_value(label, &registry()).unwrap();
        assert_eq!(
            interpreted,
            TypedTag::new(SType::Size, 1024),
            "TagFn (SizeFn::interpret) must interpret unit-suffixed values, \
             independent of whether the type is currently editable"
        );
    }

    #[test]
    fn modify_tag_relocate_is_error() {
        let item = make_item(1, vec![]);
        assert!(modify_query(
            &item,
            Some(&eq("filename:foo.txt", QueryType::Tag)),
            QueryType::Tag,
            &registry()
        )
        .is_err());
    }

    // ── EDIT.md §2 Forbidden (バグ C) ─────────────────

    #[test]
    fn modify_tag_registered_without_edit_is_forbidden() {
        assert!(
            crate::edit::parse::parse_edit_query(
                "hash:xxx",
                QueryType::Tag,
                &registry()
            )
            .is_err(),
            "registered type without edit() must be Forbidden, not Append"
        );
    }

    #[test]
    fn modify_tag_type_value_is_forbidden() {
        assert!(
            crate::edit::parse::parse_edit_query(
                "type:foo",
                QueryType::Tag,
                &registry()
            )
            .is_err(),
            "type: is a meta type and must be Forbidden as an EditQuery value"
        );
    }

    #[test]
    fn modify_tag_item_id_is_forbidden() {
        let item = make_item(1, vec![]);
        assert!(
            modify_query(&item, Some(&eq("item_id:5", QueryType::Tag)), QueryType::Tag, &registry())
                .is_err(),
            "item_id: must reject Tag direction (RemoveOnly: append not allowed)"
        );
    }

    #[test]
    fn modify_untag_registered_without_edit_is_forbidden() {
        assert!(
            crate::edit::parse::parse_edit_query(
                "hash:xxx",
                QueryType::Untag,
                &registry()
            )
            .is_err(),
            "Forbidden must also block Untag direction"
        );
    }

    #[test]
    fn modify_untag_item_id_is_allowed() {
        let item = make_item(1, vec![]);
        let actions = modify_query(
            &item,
            Some(&eq("item_id:", QueryType::Untag)),
            QueryType::Untag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Type(TagType::Base(SType::ItemId)))),
            "untag item_id: must be allowed (RemoveOnly) and produce item deletion"
        );
    }

    #[test]
    fn modify_tag_stem_is_relocate_not_forbidden() {
        let item = make_item(1, vec![]);
        let err = modify_query(
            &item,
            Some(&eq("stem:foo", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("fs_operate"),
            "stem must be classified as Relocate, not Forbidden: {err}"
        );
    }

    #[test]
    fn modify_tag_modify_injection_is_error() {
        let item = make_item(1, vec![]);
        assert!(modify_query(
            &item,
            Some(&eq("item_kind:note", QueryType::Tag)),
            QueryType::Tag,
            &registry()
        )
        .is_err());
    }

    #[test]
    fn modify_tag_projection_is_error() {
        assert!(
            crate::edit::parse::parse_edit_query(
                "project:",
                QueryType::Tag,
                &registry()
            )
            .is_err(),
            "Projection form must be rejected at parse time in Tag direction"
        );
    }

    #[test]
    fn modify_untag_existing_label() {
        let tag = TypedTag::new("project", Bitical::String("A".into()));
        let item = make_item(1, vec![tag.clone()]);
        let actions = modify_query(
            &item,
            Some(&eq("project:A", QueryType::Untag)),
            QueryType::Untag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Tag(_)))
        );
    }

    #[test]
    fn modify_untag_nonexistent_label_is_noop() {
        let item = make_item(1, vec![]);
        let actions = modify_query(
            &item,
            Some(&eq("project:Z", QueryType::Untag)),
            QueryType::Untag,
            &registry(),
        )
        .unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn modify_untag_projection() {
        let item = make_item(1, vec![]);
        let actions = modify_query(
            &item,
            Some(&eq("project:", QueryType::Untag)),
            QueryType::Untag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], WriteAction::Delete { tags, .. } if matches!(&tags[0], DeleteTarget::Type(_)))
        );
    }

    #[test]
    fn modify_untag_multiple_gives_separate_deletes() {
        let tags = vec![
            TypedTag::new("project", Bitical::String("A".into())),
            TypedTag::new("status", Bitical::String("done".into())),
        ];
        let item = make_item(1, tags);
        let actions = modify_query(
            &item,
            Some(&eq("project:A status:done", QueryType::Untag)),
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
        let WriteAction::Add { tags, .. } = &actions[0] else {
            panic!("expected Add")
        };
        tags
    }
    fn has_append(tags: &[TagOp], pred: impl Fn(&TypedTag) -> bool) -> bool {
        tags.iter()
            .any(|t| matches!(t, TagOp::Append(l) if pred(l)))
    }

    #[test]
    fn modify_settling_item_also_folds_into_single_add() {
        use crate::types::Origin;
        let mut item = make_volatile_item(SType::Type, "project");
        item.id = ItemId::Volatile(0).settle(Origin::User);
        let actions = modify_query(
            &item,
            Some(&eq("rank:5", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 1, "Settling item must fold into one Add");
        match &actions[0] {
            WriteAction::Add { item: id, .. } => {
                assert_eq!(*id, item.id, "Add must target the same Settling id")
            }
            other => panic!("expected Add, got {:?}", other),
        }
    }

    #[test]
    fn modify_volatile_injects_name_from_representative() {
        let item = make_volatile_item(SType::TypedTag, "project:A");
        let actions =
            modify_query(&item, None, QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(
            has_append(tags, |l| l.tag_type() == TagType::Base(SType::Name)
                && l.as_str() == "project:A"),
            "name must be injected from representative when no name tag exists"
        );
    }

    #[test]
    fn modify_volatile_existing_name_not_duplicated() {
        use crate::types::Origin;
        let mut item = make_volatile_item(SType::TypedTag, "project:A");
        item.tags
            .push(TypedTag::new(SType::Name, "custom name"), Origin::User);
        let actions =
            modify_query(&item, None, QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        let names: Vec<_> = tags
            .iter()
            .filter(|t| matches!(t, TagOp::Append(l) if l.tag_type() == TagType::Base(SType::Name)))
            .collect();
        assert_eq!(names.len(), 1, "must not duplicate name tag");
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Name)
            && l.as_str() == "custom name"));
    }

    #[test]
    fn modify_volatile_no_representative_falls_back_to_item_kind() {
        use crate::types::{Intrinsic, Rank};
        let item = Item {
            id: ItemId::Volatile(0),
            item_kind: ItemKind::Volatile,
            representative: vec![].into(),
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            item_count: None,
        };
        let actions = modify_query(
            &item,
            Some(&eq("project:X", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        let tags = add_tags(&actions);
        assert!(
            has_append(tags, |l| l.tag_type() == TagType::Base(SType::Name)
                && l.as_str() == "note"),
            "name must fall back to item_kind when representative is empty"
        );
    }

    #[test]
    fn modify_volatile_tag_def_replace_single_add() {
        let item = make_volatile_item(SType::TypedTag, "project:A");
        let actions = modify_query(
            &item,
            Some(&eq("rank:5", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Rank)
            && l.label.as_i64() == 5));
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Content)
            && l.as_str() == "project:A"));
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::ItemKind)
            && l.as_str() == "tag"));
    }

    // item.tags に同 type が既存なら Replace は EditQuery 側のみ採用（重複しない）
    #[test]
    fn modify_volatile_replace_dedups_against_item_tags() {
        use crate::types::Origin;
        let mut item = make_volatile_item(SType::TypedTag, "project:A");
        item.tags.push(TypedTag::new(SType::Rank, 1), Origin::User);
        let actions = modify_query(
            &item,
            Some(&eq("rank:5", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        let tags = add_tags(&actions);
        let ranks: Vec<_> = tags
            .iter()
            .filter(|t| matches!(t, TagOp::Append(l) if l.tag_type() == TagType::Base(SType::Rank)))
            .collect();
        assert_eq!(
            ranks.len(),
            1,
            "old rank dropped, only EditQuery rank remains"
        );
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Rank)
            && l.label.as_i64() == 5));
    }

    #[test]
    fn modify_volatile_tag_def_append_single_add() {
        let item = make_volatile_item(SType::TypedTag, "project:A");
        let actions = modify_query(
            &item,
            Some(&eq("project:X", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Content)
            && l.as_str() == "project:A"));
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::ItemKind)
            && l.as_str() == "tag"));
    }

    #[test]
    fn modify_volatile_type_def_single_add() {
        let item = make_volatile_item(SType::Type, "project");
        let actions = modify_query(
            &item,
            Some(&eq("project:X", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Content)
            && l.as_str() == "project"));
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::ItemKind)
            && l.as_str() == "type"));
    }

    #[test]
    fn modify_volatile_other_projection_single_add_note() {
        let item = make_volatile_item(SType::Parentdir, "/home/aki/projects");
        let actions = modify_query(
            &item,
            Some(&eq("project:X", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Content)
            && l.as_str() == "/home/aki/projects"));
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::ItemKind)
            && l.as_str() == "note"));
    }

    #[test]
    fn modify_stored_item_no_registration_action() {
        let item = make_item(1, vec![]);
        let actions = modify_query(
            &item,
            Some(&eq("project:X", QueryType::Tag)),
            QueryType::Tag,
            &registry(),
        )
        .unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn modify_volatile_no_edit_query_registration_only() {
        let item = make_volatile_item(SType::TypedTag, "project:A");
        let actions =
            modify_query(&item, None, QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Content)
            && l.as_str() == "project:A"));
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::ItemKind)
            && l.as_str() == "tag"));
    }

    #[test]
    fn modify_stored_no_edit_query_is_noop() {
        let item = make_item(1, vec![]);
        let actions =
            modify_query(&item, None, QueryType::Tag, &registry()).unwrap();
        assert!(actions.is_empty());
    }

    // 複合 representative（複数要素）の Volatile item: item_kind=note, content=全要素連結。
    #[test]
    fn modify_volatile_multi_repr_is_note_with_joined_content() {
        use crate::types::{Bitical, Intrinsic, Rank};
        let repr = vec![
            TypedTag::new(
                TagType::Base(SType::TypedTag),
                Bitical::String("project:A".to_string()),
            ),
            TypedTag::new(
                TagType::Base(SType::TypedTag),
                Bitical::String("status:done".to_string()),
            ),
        ];
        let item = Item {
            id: ItemId::Volatile(0),
            item_kind: ItemKind::Volatile,
            representative: repr.into(),
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            item_count: None,
        };
        let actions =
            modify_query(&item, None, QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::ItemKind)
            && l.as_str() == "note"));
        assert!(has_append(tags, |l| l.tag_type()
            == TagType::Base(SType::Content)
            && l.as_str() == "project:A &: status:done"));
    }

    // bitical_type: 移行後、type: タグはもうリネームされず素通りする。
    #[test]
    fn modify_type_tag_no_longer_renamed() {
        use crate::types::Origin;
        let mut item = make_volatile_item(SType::TypedTag, "project:A");
        item.tags.push(
            TypedTag::new(SType::Type, Bitical::String("integer".to_string())),
            Origin::Builtin,
        );
        let actions =
            modify_query(&item, None, QueryType::Tag, &registry()).unwrap();
        let tags = add_tags(&actions);
        assert!(
            has_append(tags, |l| l.tag_type() == TagType::Base(SType::Type)
                && matches!(
                    l.value(),
                    Bitical::String(s) if s == "integer"
                )),
            "type: tag must pass through unchanged"
        );
    }

    #[test]
    fn test_check_untag_allowed_rejections() {
        let reg = registry();
        assert!(check_untag_allowed(&TagType::Base(SType::Path), &reg).is_ok());
        assert!(check_untag_allowed(&TagType::Base(SType::Name), &reg).is_ok());
        assert!(
            check_untag_allowed(&TagType::Base(SType::ItemId), &reg).is_ok()
        );
        assert!(check_untag_allowed(
            &TagType::Custom("custom_tag".into()),
            &reg
        )
        .is_ok());

        assert!(check_untag_allowed(&TagType::Base(SType::Stem), &reg).is_err());
        assert!(
            check_untag_allowed(&TagType::Base(SType::Filename), &reg).is_err()
        );
        assert!(check_untag_allowed(&TagType::Base(SType::Extension), &reg)
            .is_err());
        assert!(check_untag_allowed(&TagType::Base(SType::Parentdir), &reg)
            .is_err());
        assert!(
            check_untag_allowed(&TagType::Base(SType::Mtime), &reg).is_err()
        );
        assert!(check_untag_allowed(&TagType::Base(SType::Size), &reg).is_err());
    }
}
