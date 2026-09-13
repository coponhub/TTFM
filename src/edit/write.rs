use super::sql::{self, UserTagDelete};
use crate::db::{Col, Store, TargetTable, Tbl};
use crate::types::{BiticalType, ItemId, Origin, SType, TagType, TypedTag};
use crate::util::{parquet_query, ParquetExt};
use anyhow::Result;
use sea_query::{Expr, PostgresQueryBuilder, Query, UnionType};

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug, PartialEq)]
pub enum TagOp {
    Append(TypedTag),
    Replace(TypedTag),
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeleteTarget {
    Type(TagType),
    Tag(TypedTag),
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
    cast_migration: Option<(TagType, BiticalType)>,
) -> Result<WriteResponse> {
    let mut modified_types = extract_action_tag_types(&actions);
    if let Some((cast_type, _)) = &cast_migration {
        modified_types.insert(cast_type.as_str().to_string());
    }

    let cascade_ids = extract_cascade_item_ids(&actions);
    if !cascade_ids.is_empty() {
        let cascade_types = collect_types_for_items(
            &store.conn,
            store,
            registry,
            &cascade_ids,
        )?;
        modified_types.extend(cascade_types);
    }
    modified_types.retain(|t| !registry.is_excluded_from_tags(t));

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
    let mut ut_inserts: Vec<(i64, TypedTag)> = vec![];
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
                    let tag = match op {
                        TagOp::Append(t) | TagOp::Replace(t) => t,
                    };
                    match tag.tag_type() {
                        TagType::Base(SType::ItemKind) => {
                            item_kind = Some(tag.as_str())
                        }
                        TagType::Base(SType::Content) => {
                            content = Some(tag.as_str())
                        }
                        _ => ut_inserts.push((item_id, tag)),
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
                        DeleteTarget::Tag(tag) => {
                            ut_deletes.push(UserTagDelete {
                                item_id,
                                tag_type: tag.tag_type().to_string(),
                                value: Some(tag.value()),
                            })
                        }
                    }
                }
            }
        }
    }

    let casts: Vec<(TagType, BiticalType)> =
        cast_migration.into_iter().collect();

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
        || !casts.is_empty()
    {
        let path = store.path_for_target(TargetTable::UserTags);
        sql::user_tags_write(
            &path.to_string_lossy(),
            ut_inserts,
            ut_deletes,
            &all_cascade,
            &casts,
        )
        .save_parquet(&store.conn, &path)?;
    }
    if !file_cascade.is_empty() {
        for target in [
            TargetTable::FileReferences,
            TargetTable::Locations,
            TargetTable::TagsByLocation,
            TargetTable::BaseTags,
            TargetTable::RemovedFiles,
        ] {
            cascade_delete_from(store, target, &file_cascade)?;
        }
    }

    if !modified_types.is_empty() {
        sync_tags_partitions(store, registry, &modified_types)?;
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
    cast_migration: Option<(TagType, BiticalType)>,
) -> Result<WriteResponse> {
    let resp = write(store, registry, actions, cast_migration)?;
    registry.load_type_configs(store)?;
    let all_cols = registry.get_all_columns();
    let reader = crate::query::lens_reader::Reader::build(
        registry,
        crate::db::Tbl::_OneView,
    );
    crate::oneview::OneView::recreate(
        &store.conn,
        registry,
        &all_cols,
        reader,
        &store.db_dir,
    )?;
    if resp.updated > 0 || resp.deleted > 0 {
        crate::search::clear_cache(&store.db_dir);
    }
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
                        tags.push(TagOp::Append(TypedTag::new(
                            SType::ItemKind,
                            "type",
                        )));
                        tags.push(TagOp::Append(TypedTag::new(
                            SType::Content,
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
// 単体テスト（型確認）
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::TagRegistry;
    use crate::types::{SType, TagType};

    #[test]
    fn write_action_variants_are_constructible() {
        let add = WriteAction::Add {
            item: ItemId::Volatile(0),
            tags: vec![
                TagOp::Append(TypedTag::new("project", "A")),
                TagOp::Append(TypedTag::new(SType::ItemKind, "tag")),
                TagOp::Append(TypedTag::new(SType::Content, "project:A")),
            ],
        };
        assert!(matches!(add, WriteAction::Add { .. }));

        let del = WriteAction::Delete {
            item: ItemId::Stored(42),
            tags: vec![
                DeleteTarget::Type(TagType::from("project")),
                DeleteTarget::Tag(TypedTag::new("project", "A")),
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
    fn delete_target_type_item_id_for_cascade() {
        let target = DeleteTarget::Type(TagType::Base(SType::ItemId));
        assert!(matches!(
            target,
            DeleteTarget::Type(TagType::Base(SType::ItemId))
        ));
    }

    #[test]
    fn rank_tag_is_stored_as_a_user_tags_row() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::tag::TagRegistry::with_standard();
        let store = Store::open(dir.path().join("db")).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();

        write(
            &store,
            &registry,
            vec![WriteAction::Add {
                item: ItemId::Volatile(0),
                tags: vec![TagOp::Append(TypedTag::new(SType::Rank, 9i64))],
            }],
            None,
        )
        .unwrap();

        let path = store.path_for_target(TargetTable::UserTags);
        let count: i64 = store
            .conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}') \
                     WHERE type = 'rank' AND label_int = 9",
                    path.to_string_lossy()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_collect_types_for_items() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::tag::TagRegistry::with_standard();
        let store = Store::open(dir.path().join("db")).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();

        write(
            &store,
            &registry,
            vec![WriteAction::Add {
                item: ItemId::Volatile(0),
                tags: vec![TagOp::Append(TypedTag::new("category", "docs"))],
            }],
            None,
        )
        .unwrap();

        let types = collect_types_for_items(
            &store.conn,
            &store,
            &registry,
            &[ItemId::from(0)],
        )
        .unwrap();
        assert!(types.contains(&"category".to_string()));
    }

    #[test]
    fn test_write_updates_tags_partition() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::tag::TagRegistry::with_standard();
        let store = Store::open(dir.path().join("db")).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();

        write(
            &store,
            &registry,
            vec![WriteAction::Add {
                item: ItemId::Volatile(0),
                tags: vec![TagOp::Append(TypedTag::new("category", "docs"))],
            }],
            None,
        )
        .unwrap();

        let part_dir = store.tags_dir().join("type=category");
        assert!(part_dir.exists());
        assert!(part_dir.join("data.parquet").exists());
    }

    #[test]
    fn test_extract_action_tag_types_and_cascade_ids() {
        let actions = vec![
            WriteAction::Add {
                item: ItemId::from(1),
                tags: vec![TagOp::Append(TypedTag::new("project", "alpha"))],
            },
            WriteAction::Delete {
                item: ItemId::from(1),
                tags: vec![
                    DeleteTarget::Tag(TypedTag::new("status", "done")),
                    DeleteTarget::Type(TagType::Custom("priority".to_string())),
                    DeleteTarget::Type(TagType::Base(SType::ItemId)),
                ],
            },
        ];
        let types = extract_action_tag_types(&actions);
        assert!(types.contains("project"));
        assert!(types.contains("status"));
        assert!(types.contains("priority"));
        assert!(!types.contains("item_id"));

        let cascade_ids = extract_cascade_item_ids(&actions);
        assert_eq!(cascade_ids, vec![ItemId::from(1)]);
    }

    #[test]
    fn test_write_filters_excluded_tags() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        crate::indexing::Indexer::new(&store, &TagRegistry::with_standard())
            .initialize_tables()
            .unwrap();

        let registry = TagRegistry::with_standard();
        let actions = vec![WriteAction::Add {
            item: ItemId::from(1),
            tags: vec![TagOp::Append(TypedTag::new("mtime", "2026-01-01"))],
        }];

        write(&store, &registry, actions, None).unwrap();
        assert!(!store.tags_dir().join("type=mtime").exists());
    }

    #[test]
    fn test_sync_tags_partitions_cleanup_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        // create a dummy corrupted partition dir
        let part_dir = store.tags_dir().join("type=broken");
        std::fs::create_dir_all(&part_dir).unwrap();
        assert!(part_dir.exists());

        let mut modified = rustc_hash::FxHashSet::default();
        modified.insert("broken".to_string());
        let reg = crate::tag::TagRegistry::with_standard();
        let res = sync_tags_partitions(&store, &reg, &modified);
        assert!(res.is_err());
        assert!(!part_dir.exists());
    }
}

pub(crate) fn collect_types_for_items(
    conn: &duckdb::Connection,
    store: &Store,
    registry: &crate::tag::TagRegistry,
    item_ids: &[ItemId],
) -> Result<Vec<String>> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = item_ids.iter().map(|id| id.as_i64()).collect();

    let mut queries = Vec::new();

    let loc_path = store.path_for_target(TargetTable::TagsByLocation);
    if loc_path.exists() {
        let mut loc_q = Query::select();
        loc_q
            .column(Col::Type)
            .from_subquery(
                crate::util::parquet_query(&loc_path.to_string_lossy()),
                Tbl::TagsByLocation,
            )
            .and_where(Expr::col(Col::ItemId).is_in(ids.clone()));
        queries.push(loc_q);
    }

    let user_path = store.path_for_target(TargetTable::UserTags);
    if user_path.exists() {
        let mut user_q = Query::select();
        user_q
            .column(Col::Type)
            .from_subquery(
                crate::util::parquet_query(&user_path.to_string_lossy()),
                Tbl::UserTags,
            )
            .and_where(Expr::col(Col::ItemId).is_in(ids.clone()));
        queries.push(user_q);
    }

    let base_path = store.path_for_target(TargetTable::BaseTags);
    if base_path.exists() {
        let mut base_q = Query::select();
        base_q
            .column(Col::Type)
            .from_subquery(
                crate::util::parquet_query(&base_path.to_string_lossy()),
                Tbl::BaseTags,
            )
            .and_where(Expr::col(Col::ItemId).is_in(ids));
        let excluded = registry.get_excluded_from_tags();
        if !excluded.is_empty() {
            base_q.and_where(Expr::col(Col::Type).is_not_in(excluded));
        }
        queries.push(base_q);
    }

    if queries.is_empty() {
        return Ok(Vec::new());
    }

    let mut combined_q = queries.remove(0);
    for q in queries {
        combined_q.union(UnionType::All, q);
    }

    let mut q = Query::select();
    q.distinct()
        .column(Col::Type)
        .from_subquery(combined_q, Tbl::ItemTags);

    let sql = q.to_string(PostgresQueryBuilder);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect::<duckdb::Result<Vec<_>>>().map_err(Into::into)
}

fn extract_action_tag_types(
    actions: &[WriteAction],
) -> rustc_hash::FxHashSet<String> {
    let mut types = rustc_hash::FxHashSet::default();
    for action in actions {
        match action {
            WriteAction::Add { tags, .. } => {
                for tag_op in tags {
                    let tag = match tag_op {
                        TagOp::Append(t) | TagOp::Replace(t) => t,
                    };
                    types.insert(tag.tag_type().as_str().to_string());
                }
            }
            WriteAction::Delete { tags, .. } => {
                for target in tags {
                    match target {
                        DeleteTarget::Tag(t) => {
                            types.insert(t.tag_type().as_str().to_string());
                        }
                        DeleteTarget::Type(tt) => {
                            if tt.as_str() != "item_id" {
                                types.insert(tt.as_str().to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    types
}

fn extract_cascade_item_ids(actions: &[WriteAction]) -> Vec<ItemId> {
    let mut ids = Vec::new();
    for action in actions {
        if let WriteAction::Delete { item, tags } = action {
            for target in tags {
                if matches!(
                    target,
                    DeleteTarget::Type(TagType::Base(SType::ItemId))
                ) {
                    ids.push(*item);
                }
            }
        }
    }
    ids
}

fn sync_tags_partitions(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    modified_types: &rustc_hash::FxHashSet<String>,
) -> Result<()> {
    let types_vec: Vec<&str> =
        modified_types.iter().map(|s| s.as_str()).collect();
    if let Err(e) = crate::db::tags::update_tags_partitions(
        store,
        &store.conn,
        registry,
        &types_vec,
    ) {
        for tag_type in &types_vec {
            let part_dir = store.tags_dir().join(format!("type={tag_type}"));
            if part_dir.exists() {
                if let Err(err) = std::fs::remove_dir_all(&part_dir) {
                    eprintln!(
                        "Failed to remove invalid partition directory {}: {err}",
                        part_dir.display()
                    );
                }
            }
        }
        return Err(e);
    }
    Ok(())
}
