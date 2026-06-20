use super::sql::{self, UserTagDelete};
use crate::db::{Col, Store, Tbl, TargetTable};
use crate::types::{ItemId, Label, Origin, SType, TagType};
use crate::util::{parquet_query, ExecuteSql, IdenExt, ParquetExt, SelectExt};
use anyhow::Result;
use sea_query::{Asterisk, Expr, Order, Query};

pub enum WriteAction {
    Add    { item: ItemId, tags: Vec<TagOp> },
    Delete { item: ItemId, tags: Vec<DeleteTarget> },
}

pub enum TagOp {
    Append(Label),
    Replace(Label),
}

pub enum DeleteTarget {
    Type(TagType),
    Tag(Label),
}

pub struct WriteResponse {
    pub added: usize,
    pub deleted: usize,
    pub new_item_ids: Vec<i64>,
}

// ──────────────────────────────────────────────
// 公開 API
// ──────────────────────────────────────────────

pub fn write(store: &Store, actions: Vec<WriteAction>) -> Result<WriteResponse> {
    // 1. Volatile → Stored 採番
    let (resolved, new_item_ids) = resolve_volatiles(store, actions)?;

    // 2. 変更を収集
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
                        Label::Content(s)  => content = Some(s.clone()),
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
                    ir_inserts.push((item_id, item_kind.unwrap_or_default(), content.unwrap_or_default()));
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
                        DeleteTarget::Type(tt) => ut_deletes.push(UserTagDelete {
                            item_id,
                            tag_type: tt.to_string(),
                            value: None,
                        }),
                        DeleteTarget::Tag(label) => ut_deletes.push(UserTagDelete {
                            item_id,
                            tag_type: label.tag_type().to_string(),
                            value: Some(label.value()),
                        }),
                    }
                }
            }
        }
    }

    let added = ut_inserts.len();
    let deleted = ut_deletes.len() + user_cascade.len() + file_cascade.len();

    // 3. 書き込み（順序固定: item_references → user_tags → rank 更新）
    if !ir_inserts.is_empty() || !user_cascade.is_empty() {
        let path = store.path_for_target(TargetTable::ItemReferences);
        sql::item_references_write(&path.to_string_lossy(), ir_inserts, &user_cascade)
            .save_parquet(&store.conn, &path)?;
    }
    let all_cascade: Vec<i64> = user_cascade.iter().chain(file_cascade.iter()).copied().collect();
    if !ut_inserts.is_empty() || !ut_deletes.is_empty() || !all_cascade.is_empty() {
        let path = store.path_for_target(TargetTable::UserTags);
        sql::user_tags_write(&path.to_string_lossy(), ut_inserts, ut_deletes, &all_cascade)
            .save_parquet(&store.conn, &path)?;
    }
    if !file_cascade.is_empty() {
        for target in [TargetTable::FileReferences, TargetTable::Locations, TargetTable::BaseTags] {
            cascade_delete_from(store, target, &file_cascade)?;
        }
    }
    if !ir_rank_updates.is_empty() {
        update_rank_column(store, TargetTable::ItemReferences, &ir_rank_updates)?;
    }
    if !fr_rank_updates.is_empty() {
        update_rank_column(store, TargetTable::FileReferences, &fr_rank_updates)?;
    }

    Ok(WriteResponse { added, deleted, new_item_ids })
}

pub fn write_and_refresh(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    actions: Vec<WriteAction>,
) -> Result<WriteResponse> {
    let resp = write(store, actions)?;
    let all_cols = registry.get_all_columns();
    crate::oneview::OneView::recreate(&store.conn, &all_cols, &store.db_dir)?;
    Ok(resp)
}

// ──────────────────────────────────────────────
// Volatile 採番
// ──────────────────────────────────────────────

fn resolve_volatiles(
    store: &Store,
    actions: Vec<WriteAction>,
) -> Result<(Vec<WriteAction>, Vec<i64>)> {
    use std::collections::HashMap;

    if let Some(WriteAction::Delete { item: ItemId::Volatile(c), .. }) =
        actions.iter().find(|a| matches!(a, WriteAction::Delete { item: ItemId::Volatile(_), .. }))
    {
        anyhow::bail!("cannot delete a Volatile item (counter={c}) that has not been stored yet");
    }

    let mut counters: Vec<u64> = actions.iter().filter_map(|a| match a {
        WriteAction::Add { item: ItemId::Volatile(c), .. } => Some(*c),
        _ => None,
    }).collect();
    counters.sort_unstable();
    counters.dedup();

    if counters.is_empty() {
        return Ok((actions, vec![]));
    }

    let new_ids = crate::db::identifier::next(store, Origin::User, counters.len())?;
    let mapping: HashMap<u64, i64> = counters.into_iter().zip(new_ids.iter().copied()).collect();

    let resolved = actions.into_iter().map(|action| match action {
        WriteAction::Add { item: ItemId::Volatile(c), tags } => WriteAction::Add {
            item: ItemId::Stored(*mapping.get(&c).unwrap()),
            tags,
        },
        other => other,
    }).collect();

    Ok((resolved, new_ids))
}

// ──────────────────────────────────────────────
// カスケード削除（File-origin）
// ──────────────────────────────────────────────

fn cascade_delete_from(store: &Store, target: TargetTable, ids: &[i64]) -> Result<()> {
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

fn update_rank_column(store: &Store, target: TargetTable, updates: &[(i64, i64)]) -> Result<()> {
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
            added: 3,
            deleted: 1,
            new_item_ids: vec![0, 1],
        };
        assert_eq!(resp.new_item_ids.len(), 2);
        assert_eq!(resp.added, 3);
        assert_eq!(resp.deleted, 1);
    }

    #[test]
    fn delete_target_type_item_id_for_cascade() {
        let target = DeleteTarget::Type(TagType::Base(SType::ItemId));
        assert!(matches!(target, DeleteTarget::Type(TagType::Base(SType::ItemId))));
    }
}
