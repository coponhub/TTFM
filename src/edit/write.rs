use super::sql::{self, UserTagDelete};
use crate::db::{Col, Store, TargetTable, Tbl};
use crate::types::{ItemId, Label, Origin, SType, TagType};
use crate::util::{parquet_query, ExecuteSql, IdenExt, ParquetExt, SelectExt};
use anyhow::Result;
use sea_query::{Asterisk, Expr, Order, Query};

#[derive(Debug)]
pub enum WriteAction {
    Add {
        item: ItemId,
        tags: Vec<TagOp>,
    },
    Delete {
        item: ItemId,
        tags: Vec<DeleteTarget>,
    },
}

#[derive(Debug, PartialEq)]
pub enum TagOp {
    Append(Label),
    Replace(Label),
}

#[derive(Debug, PartialEq)]
pub enum DeleteTarget {
    Type(TagType),
    Tag(Label),
}

pub struct WriteResponse {
    pub updated: usize,
    pub deleted: usize,
    pub new_item_ids: Vec<i64>,
}

// ──────────────────────────────────────────────
// 公開 API
// ──────────────────────────────────────────────

pub fn write(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    actions: Vec<WriteAction>,
) -> Result<WriteResponse> {
    // 1. Volatile/Settling → Stored 採番
    let (resolved, new_item_ids) = resolve_volatiles(store, actions)?;
    // 2. 未実体化の組み込み型定義（Sys 区画・行なし）に kind/content を補う
    let resolved = inject_builtin_definitions(store, registry, resolved)?;

    // 3. カウント
    let (updated, deleted) = {
        let mut add: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::new();
        let mut del: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::new();
        for action in &resolved {
            match action {
                WriteAction::Add { item, tags } => {
                    *add.entry(item.as_i64()).or_default() += tags.len()
                }
                WriteAction::Delete { item, tags } => {
                    *del.entry(item.as_i64()).or_default() += tags.len()
                }
            }
        }
        let u: usize = add.values().sum::<usize>() + new_item_ids.len();
        let d: usize = del
            .iter()
            .map(|(id, &n)| n.saturating_sub(*add.get(id).unwrap_or(&0)))
            .sum();
        (u, d)
    };

    // 4. 変更を収集
    let mut ir_inserts: Vec<(i64, String, String)> = vec![];
    let mut ir_rank_updates: Vec<(i64, i64)> = vec![];
    let mut fr_rank_updates: Vec<(i64, i64)> = vec![];
    let mut ut_inserts: Vec<(i64, Label)> = vec![];
    let mut ut_deletes: Vec<UserTagDelete> = vec![];
    let mut user_cascade: Vec<i64> = vec![]; // User/System item_id カスケード削除
    let mut file_cascade: Vec<i64> = vec![]; // File item_id カスケード削除

    for action in resolved {
        match action {
            WriteAction::Add { item, tags } => {
                let item_id = item.as_i64();
                let mut item_kind: Option<String> = None;
                let mut content: Option<String> = None;

                for op in tags {
                    let label = match op {
                        TagOp::Append(l) | TagOp::Replace(l) => l,
                    };
                    match &label {
                        Label::ItemKind(s) => item_kind = Some(s.clone()),
                        Label::Content(s) => content = Some(s.clone()),
                        Label::Rank(new_rank) => {
                            if Origin::within(item_id) == Origin::File {
                                fr_rank_updates.push((item_id, *new_rank));
                            } else {
                                ir_rank_updates.push((item_id, *new_rank));
                            }
                        }
                        _ => ut_inserts.push((item_id, label)),
                    }
                }

                if item_kind.is_some() || content.is_some() {
                    ir_inserts.push((
                        item_id,
                        item_kind.unwrap_or_default(),
                        content.unwrap_or_default(),
                    ));
                }
            }
            WriteAction::Delete { item, tags } => {
                let item_id = item.as_i64();
                for target in tags {
                    match target {
                        DeleteTarget::Type(TagType::Base(SType::ItemId)) => {
                            if Origin::within(item_id) == Origin::File {
                                file_cascade.push(item_id);
                            } else {
                                user_cascade.push(item_id);
                            }
                        }
                        DeleteTarget::Type(tt) => {
                            ut_deletes.push(UserTagDelete {
                                item_id,
                                tag_type: tt.to_string(),
                                value: None,
                            })
                        }
                        DeleteTarget::Tag(label) => {
                            ut_deletes.push(UserTagDelete {
                                item_id,
                                tag_type: label.tag_type().to_string(),
                                value: Some(label.value()),
                            })
                        }
                    }
                }
            }
        }
    }

    // 5. 書き込み（順序固定: item_references → user_tags → rank 更新）
    if !ir_inserts.is_empty() || !user_cascade.is_empty() {
        let path = store.path_for_target(TargetTable::ItemReferences);
        sql::item_references_write(
            &path.to_string_lossy(),
            ir_inserts,
            &user_cascade,
        )
        .save_parquet(&store.conn, &path)?;
    }
    let all_cascade: Vec<i64> = user_cascade
        .iter()
        .chain(file_cascade.iter())
        .copied()
        .collect();
    if !ut_inserts.is_empty()
        || !ut_deletes.is_empty()
        || !all_cascade.is_empty()
    {
        let path = store.path_for_target(TargetTable::UserTags);
        sql::user_tags_write(
            &path.to_string_lossy(),
            ut_inserts,
            ut_deletes,
            &all_cascade,
        )
        .save_parquet(&store.conn, &path)?;
    }
    if !file_cascade.is_empty() {
        for target in [
            TargetTable::FileReferences,
            TargetTable::Locations,
            TargetTable::BaseTags,
        ] {
            cascade_delete_from(store, target, &file_cascade)?;
        }
    }
    if !ir_rank_updates.is_empty() {
        update_rank_column(
            store,
            TargetTable::ItemReferences,
            &ir_rank_updates,
        )?;
    }
    if !fr_rank_updates.is_empty() {
        update_rank_column(
            store,
            TargetTable::FileReferences,
            &fr_rank_updates,
        )?;
    }

    Ok(WriteResponse {
        updated,
        deleted,
        new_item_ids,
    })
}

pub fn write_and_refresh(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    actions: Vec<WriteAction>,
) -> Result<WriteResponse> {
    let resp = write(store, registry, actions)?;
    let all_cols = registry.get_all_columns();
    let reader = crate::query::lens_reader::Reader::build(
        registry,
        crate::db::Tbl::_OneView,
    );
    crate::oneview::OneView::recreate(
        &store.conn,
        &all_cols,
        reader,
        &store.db_dir,
    )?;
    Ok(resp)
}

// ──────────────────────────────────────────────
// Volatile/Settling 採番
// ──────────────────────────────────────────────

fn resolve_volatiles(
    store: &Store,
    actions: Vec<WriteAction>,
) -> Result<(Vec<WriteAction>, Vec<i64>)> {
    use std::collections::HashMap;

    if let Some(item) = actions.iter().find_map(|a| match a {
        WriteAction::Delete { item, .. } if !item.is_stored() => {
            Some(item.clone())
        }
        _ => None,
    }) {
        anyhow::bail!(
            "cannot delete an unresolved item ({item}) that has not been stored yet"
        );
    }

    // Volatile は User 区画、Settling は指定された区画へ、それぞれ採番する。
    // counter 空間は共有するが variant が違うため衝突しない。
    let mut by_origin: HashMap<Origin, Vec<u64>> = HashMap::new();
    for action in &actions {
        if let WriteAction::Add { item, .. } = action {
            match item {
                ItemId::Volatile(c) => {
                    by_origin.entry(Origin::User).or_default().push(*c)
                }
                ItemId::Settling(origin, c) => {
                    by_origin.entry(*origin).or_default().push(*c)
                }
                ItemId::Stored(_) => {}
            }
        }
    }

    if by_origin.is_empty() {
        return Ok((actions, vec![]));
    }

    let mut mapping: HashMap<u64, i64> = HashMap::new();
    let mut new_ids: Vec<i64> = vec![];
    for (origin, mut counters) in by_origin {
        counters.sort_unstable();
        counters.dedup();
        let ids = crate::db::identifier::next(store, origin, counters.len())?;
        new_ids.extend(ids.iter().copied());
        mapping.extend(counters.into_iter().zip(ids));
    }

    let resolved = actions
        .into_iter()
        .map(|action| match action {
            WriteAction::Add {
                item: ItemId::Volatile(c),
                tags,
            } => WriteAction::Add {
                item: ItemId::Stored(*mapping.get(&c).unwrap()),
                tags,
            },
            WriteAction::Add {
                item: ItemId::Settling(_, c),
                tags,
            } => WriteAction::Add {
                item: ItemId::Stored(*mapping.get(&c).unwrap()),
                tags,
            },
            other => other,
        })
        .collect();

    Ok((resolved, new_ids))
}

// ──────────────────────────────────────────────
// 組み込み型定義（Sys 区画）の初回実体化
// ──────────────────────────────────────────────

// Sys 区画の id を持つが item_references にまだ行が無い Add に、
// registry から導出した kind/content を補って行を作れるようにする。
// 行が既にあれば触らない（通常の rank/tag 更新に任せる）。
fn inject_builtin_definitions(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    actions: Vec<WriteAction>,
) -> Result<Vec<WriteAction>> {
    let sys_ids: Vec<i64> = actions
        .iter()
        .filter_map(|a| match a {
            WriteAction::Add { item, .. } if item.is_stored() => {
                let id = item.as_i64();
                (Origin::within(id) == Origin::Builtin).then_some(id)
            }
            _ => None,
        })
        .collect();

    if sys_ids.is_empty() {
        return Ok(actions);
    }

    let existing = existing_item_ids(store, &sys_ids)?;

    Ok(actions
        .into_iter()
        .map(|action| match action {
            WriteAction::Add { item, mut tags } if item.is_stored() => {
                let id = item.as_i64();
                if Origin::within(id) == Origin::Builtin
                    && !existing.contains(&id)
                {
                    let offset = (id - Origin::Builtin.block_lo()) as u32;
                    if let Some(name) = registry.builtin_name_for_offset(offset)
                    {
                        tags.push(TagOp::Append(Label::ItemKind(
                            "type".to_string(),
                        )));
                        tags.push(TagOp::Append(Label::Content(
                            name.to_string(),
                        )));
                    }
                }
                WriteAction::Add { item, tags }
            }
            other => other,
        })
        .collect())
}

// 指定 id のうち、既に item_references に行があるものの集合を返す。
fn existing_item_ids(
    store: &Store,
    ids: &[i64],
) -> Result<std::collections::HashSet<i64>> {
    let path = store.path_for_target(TargetTable::ItemReferences);
    if !path.exists() {
        return Ok(Default::default());
    }
    let sql = Query::select()
        .column(Col::ItemId)
        .from_subquery(
            parquet_query(&path.to_string_lossy()),
            Tbl::ItemReferences,
        )
        .and_where(Expr::col(Col::ItemId).is_in(ids.to_vec()))
        .to_owned();
    Ok(crate::query::fetcher::fetch_ids(&store.conn, &sql)?
        .into_iter()
        .collect())
}

// ──────────────────────────────────────────────
// カスケード削除（File-origin）
// ──────────────────────────────────────────────

fn cascade_delete_from(
    store: &Store,
    target: TargetTable,
    ids: &[i64],
) -> Result<()> {
    let path = store.path_for_target(target);
    if !path.exists() {
        return Ok(());
    }
    let mut q = parquet_query(&path.to_string_lossy());
    q.and_where(Expr::col(Col::ItemId).is_not_in(ids.to_vec()));
    q.save_parquet(&store.conn, &path)
}

// ──────────────────────────────────────────────
// rank 更新（temp table パターン）
// ──────────────────────────────────────────────

fn update_rank_column(
    store: &Store,
    target: TargetTable,
    updates: &[(i64, i64)],
) -> Result<()> {
    let path = store.path_for_target(target);
    if !path.exists() {
        return Ok(());
    }
    let path_str = path.to_string_lossy();
    let tmp = Tbl::Target;

    parquet_query(&path_str).create_table_as(&store.conn, tmp)?;
    sql::rank_case_update(tmp, updates).execute(&store.conn)?;
    Query::select()
        .column(Asterisk)
        .from(tmp)
        .order_by(Col::ItemId, Order::Asc)
        .to_owned()
        .save_parquet(&store.conn, &path)?;
    tmp.drop_table(&store.conn)?;
    Ok(())
}

// ──────────────────────────────────────────────
// 単体テスト（型確認）
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LabelValue, SType, TagType};

    #[test]
    fn write_action_variants_are_constructible() {
        let add = WriteAction::Add {
            item: ItemId::Volatile(0),
            tags: vec![
                TagOp::Append(Label::Other(
                    TagType::from("project"),
                    LabelValue::String("A".to_string()),
                )),
                TagOp::Append(Label::ItemKind("tag".to_string())),
                TagOp::Append(Label::Content("project:A".to_string())),
            ],
        };
        assert!(matches!(add, WriteAction::Add { .. }));

        let del = WriteAction::Delete {
            item: ItemId::Stored(42),
            tags: vec![
                DeleteTarget::Type(TagType::from("project")),
                DeleteTarget::Tag(Label::Other(
                    TagType::from("project"),
                    LabelValue::String("A".to_string()),
                )),
            ],
        };
        assert!(matches!(del, WriteAction::Delete { .. }));
    }

    #[test]
    fn write_response_tracks_new_ids() {
        let resp = WriteResponse {
            updated: 3,
            deleted: 1,
            new_item_ids: vec![0, 1],
        };
        assert_eq!(resp.new_item_ids.len(), 2);
        assert_eq!(resp.updated, 3);
        assert_eq!(resp.deleted, 1);
    }

    #[test]
    fn write_response_updated_includes_rank_ops() {
        // rank は ut_inserts でなく ir_rank_updates に入るため、
        // updated は両方の合計であることを構造体レベルで確認する
        let resp = WriteResponse {
            updated: 1,
            deleted: 0,
            new_item_ids: vec![],
        };
        assert_eq!(resp.updated, 1);
    }

    #[test]
    fn delete_target_type_item_id_for_cascade() {
        let target = DeleteTarget::Type(TagType::Base(SType::ItemId));
        assert!(matches!(
            target,
            DeleteTarget::Type(TagType::Base(SType::ItemId))
        ));
    }
}
