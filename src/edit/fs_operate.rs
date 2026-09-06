use crate::edit::modify::ResolvedNode;
use crate::edit::QueryType;
use crate::response::Item;
use crate::tag::{EditStrategy, FileAttr, TagRegistry};
use crate::types::{ItemId, Label, PathComponents, SType, TagType};
use anyhow::{anyhow, bail, Result};
use std::fmt::{self, Formatter};
use std::path::{Path, PathBuf};

pub fn path_components(
    tag_type: &TagType,
    registry: &TagRegistry,
) -> &'static [SType] {
    registry
        .get(tag_type.as_str())
        .and_then(|f| f.edit())
        .map(|e| e.path_components())
        .unwrap_or(&[])
}

pub fn check_location_tag(
    tag_types: &[TagType],
    registry: &TagRegistry,
) -> Result<()> {
    let mut seen: Vec<(SType, TagType)> = vec![];
    for tt in tag_types {
        for c in path_components(tt, registry) {
            if let Some((_, prev)) = seen.iter().find(|(s, _)| s == c) {
                bail!("'{tt}' and '{prev}' both set the path component '{c}'");
            }
            seen.push((*c, tt.clone()));
        }
    }
    Ok(())
}

pub fn is_fs_strategy(s: EditStrategy) -> bool {
    matches!(s, EditStrategy::Relocate | EditStrategy::SetFileAttr)
}

pub fn file_attr_of(
    tag_type: &TagType,
    label: &Label,
    registry: &TagRegistry,
) -> Result<FileAttr> {
    registry
        .get(tag_type.as_str())
        .and_then(|f| f.edit())
        .map(|e| e.file_attr(label))
        .unwrap_or_else(|| {
            bail!("tag type '{tag_type}' sets no file attribute")
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsMove {
    pub item: ItemId,
    pub from: PathBuf,
    pub to: PathBuf,
    pub crossed: bool,
}

pub struct FsAttr {
    pub item: ItemId,
    pub from: PathBuf,
    pub attr: FileAttr,
}

#[derive(Default)]
pub struct FsPlan {
    pub moves: Vec<FsMove>,
    pub attrs: Vec<FsAttr>,
    pub mkdirs: Vec<PathBuf>,
    pub issues: Vec<FsIssue>,
}

impl FsPlan {
    pub fn warn_unsupported(
        &mut self,
        sink: &mut dyn crate::query::error::WarningSink,
    ) -> bool {
        let mut skipped = false;
        self.issues.retain(|i| match i {
            FsIssue::UntagPathUnsupported(_)
            | FsIssue::CreateUnsupported(_) => {
                sink.warn(crate::query::error::Warning(format!(
                    "{i} (not performed)"
                )));
                skipped = true;
                false
            }
            _ => true,
        });
        skipped
    }
}

pub struct Moved {
    pub item: ItemId,
    pub to: PathBuf,
    pub crossed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrSet {
    pub item: ItemId,
    pub path: PathBuf,
}

#[derive(Default)]
pub struct FsOutcome {
    pub moved: Vec<Moved>,
    pub attrs_set: Vec<AttrSet>,
}

impl FsOutcome {
    pub fn target_of(&self, item: &ItemId) -> Option<&PathBuf> {
        self.moved.iter().find(|m| &m.item == item).map(|m| &m.to)
    }

    pub fn count(&self) -> usize {
        self.moved.len() + self.attrs_set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.attrs_set.is_empty()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FsIssue {
    SourceMissing(ItemId, PathBuf),
    ChainedMove(ItemId, PathBuf),
    TargetInsideSource(ItemId, PathBuf, PathBuf),
    TargetNotWritable(ItemId, PathBuf),
    MultipleLocations(ItemId, Vec<PathBuf>, Vec<FsMove>),
    NoLocation(ItemId),
    CreateUnsupported(ItemId),
    UntagPathUnsupported(ItemId),
    NotEnoughSpace(PathBuf, u64),
}

impl fmt::Display for FsIssue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceMissing(i, p) => write!(f, "{i}: source {p:?} is gone"),
            Self::ChainedMove(i, p) => {
                write!(f, "{i}: {p:?} is another item's source in this edit")
            }
            Self::TargetInsideSource(i, s, p) => {
                write!(f, "{i}: {p:?} is inside {s:?}")
            }
            Self::TargetNotWritable(i, p) => {
                write!(f, "{i}: cannot write {p:?}")
            }
            Self::MultipleLocations(i, paths, _) => {
                let list: Vec<String> =
                    paths.iter().map(|p| format!("{p:?}")).collect();
                write!(f, "{i}: has several paths: {}", list.join(", "))
            }
            Self::NoLocation(i) => write!(f, "{i}: has no file to act on"),
            Self::CreateUnsupported(i) => {
                write!(f, "{i}: creating a file is not implemented yet")
            }
            Self::UntagPathUnsupported(i) => {
                write!(f, "{i}: deleting a file is not implemented yet")
            }
            Self::NotEnoughSpace(d, n) => write!(f, "{d:?}: needs {n} bytes"),
        }
    }
}

fn types_of(nodes: &[ResolvedNode]) -> Vec<TagType> {
    nodes.iter().map(|n| n.tag_type.clone()).collect()
}

fn label_of(node: &ResolvedNode) -> Result<Label> {
    node.label
        .clone()
        .ok_or_else(|| anyhow!("tag type '{}' requires a value", node.tag_type))
}

fn apply_node(parts: &mut PathComponents, comps: &[SType], value: &str) {
    match comps {
        [SType::Parentdir, SType::Stem, SType::Extension] => {
            *parts = PathComponents::decompose(Path::new(value))
        }
        [SType::Stem, SType::Extension] => parts.set_filename(value),
        [SType::Parentdir] => parts.parentdir = value.to_string(),
        [SType::Stem] => parts.stem = value.to_string(),
        [SType::Extension] => parts.extension = Some(value.to_string()),
        _ => {}
    }
}

fn location_paths(item: &Item) -> Vec<PathBuf> {
    item.tags
        .get_values(&TagType::Base(SType::Path))
        .iter()
        .map(|v| PathBuf::from(v.label.as_str()))
        .collect()
}

fn writes_path(nodes: &[ResolvedNode]) -> bool {
    nodes.iter().any(|n| n.strategy == EditStrategy::Relocate)
}

fn existing_parent(p: &Path) -> &Path {
    let mut cur = p;
    while !cur.exists() {
        match cur.parent() {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    cur
}

fn device_of(path: &Path) -> Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::metadata(path)?.dev())
    }
    #[cfg(not(unix))]
    {
        match file_id::get_file_id(path)? {
            file_id::FileId::Inode { device_id, .. } => Ok(device_id),
            file_id::FileId::LowRes {
                volume_serial_number,
                ..
            } => Ok(volume_serial_number as u64),
            file_id::FileId::HighRes {
                volume_serial_number,
                ..
            } => Ok(volume_serial_number),
        }
    }
}

pub fn plan_fs(
    registry: &TagRegistry,
    inputs: Vec<(Item, Vec<ResolvedNode>)>,
    query_type: QueryType,
) -> Result<FsPlan> {
    let mut plan = FsPlan::default();
    for (item, nodes) in inputs {
        if nodes.is_empty() {
            continue;
        }
        check_location_tag(&types_of(&nodes), registry)?;
        if query_type == QueryType::Untag {
            plan.issues
                .push(FsIssue::UntagPathUnsupported(item.id.clone()));
            continue;
        }
        let paths = location_paths(&item);
        if paths.len() > 1 && writes_path(&nodes) {
            let mut candidate_moves = Vec::new();
            for from in &paths {
                let mut parts = PathComponents::decompose(from);
                for n in &nodes {
                    if n.strategy == EditStrategy::Relocate {
                        let comps = path_components(&n.tag_type, registry);
                        let label = label_of(n)?;
                        apply_node(&mut parts, comps, &label.as_str());
                    }
                }
                let to = parts.join();
                if !from.exists() {
                    plan.issues.push(FsIssue::SourceMissing(
                        item.id.clone(),
                        from.clone(),
                    ));
                    continue;
                }
                if to == *from {
                    continue;
                }
                let parent = to.parent().unwrap_or(Path::new("")).to_path_buf();
                if parent.exists() && !writable(&parent) {
                    plan.issues
                        .push(FsIssue::TargetNotWritable(item.id.clone(), to));
                    continue;
                }
                let target_dev_dir = existing_parent(&parent);
                let crossed = device_of(from)? != device_of(target_dev_dir)?;
                candidate_moves.push(FsMove {
                    item: item.id.clone(),
                    from: from.clone(),
                    to,
                    crossed,
                });
            }
            plan.issues.push(FsIssue::MultipleLocations(
                item.id.clone(),
                paths,
                candidate_moves,
            ));
            continue;
        }
        let from = match paths.as_slice() {
            [] if writes_path(&nodes) => {
                plan.issues
                    .push(FsIssue::CreateUnsupported(item.id.clone()));
                continue;
            }
            [] => {
                plan.issues.push(FsIssue::NoLocation(item.id.clone()));
                continue;
            }
            [from, ..] => from.clone(),
        };
        let mut parts = PathComponents::decompose(&from);
        for n in &nodes {
            match n.strategy {
                EditStrategy::Relocate => {
                    let comps = path_components(&n.tag_type, registry);
                    let label = label_of(n)?;
                    apply_node(&mut parts, comps, &label.as_str());
                }
                EditStrategy::SetFileAttr => plan.attrs.push(FsAttr {
                    item: item.id.clone(),
                    from: from.clone(),
                    attr: file_attr_of(&n.tag_type, &label_of(n)?, registry)?,
                }),
                _ => {}
            }
        }
        verify(&mut plan, item.id.clone(), from, parts.join())?;
    }
    classify_targets(&mut plan);
    check_free_space(&mut plan)?;
    Ok(plan)
}

fn verify(
    plan: &mut FsPlan,
    item: ItemId,
    from: PathBuf,
    to: PathBuf,
) -> Result<()> {
    if !from.exists() {
        plan.issues.push(FsIssue::SourceMissing(item, from));
        return Ok(());
    }
    if to == from {
        return Ok(());
    }
    let parent = to.parent().unwrap_or(Path::new("")).to_path_buf();
    if parent.exists() && !writable(&parent) {
        plan.issues.push(FsIssue::TargetNotWritable(item, to));
        return Ok(());
    }
    if !parent.exists() {
        plan.mkdirs.push(parent.clone());
    }
    let target_dev_dir = existing_parent(&parent);
    let crossed = device_of(&from)? != device_of(target_dev_dir)?;
    plan.moves.push(FsMove {
        item,
        from,
        to,
        crossed,
    });
    Ok(())
}

fn classify_one(moves: &[FsMove], m: &FsMove) -> Option<FsIssue> {
    let (item, to) = (m.item.clone(), m.to.clone());
    if m.to.starts_with(&m.from) {
        return Some(FsIssue::TargetInsideSource(item, m.from.clone(), to));
    }
    if moves.iter().any(|o| o.from == m.to) {
        return Some(FsIssue::ChainedMove(item, to));
    }
    None
}

fn classify_targets(plan: &mut FsPlan) {
    let verdicts: Vec<Option<FsIssue>> = plan
        .moves
        .iter()
        .map(|m| classify_one(&plan.moves, m))
        .collect();
    let mut keep = verdicts.iter().map(|v| v.is_none());
    plan.moves.retain(|_| keep.next().unwrap_or(false));
    plan.issues.extend(verdicts.into_iter().flatten());
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".ttfm-write-probe-{}", std::process::id()));
    let ok = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    std::fs::remove_file(&probe).ok();
    ok
}

fn bytes_per_target_dir(moves: &[FsMove]) -> Result<Vec<(PathBuf, u64)>> {
    let mut totals: Vec<(PathBuf, u64)> = vec![];
    for m in moves {
        if !m.crossed {
            continue;
        }
        let dir = existing_parent(&m.to).to_path_buf();
        let size = std::fs::metadata(&m.from)?.len();
        match totals.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, n)) => *n += size,
            None => totals.push((dir, size)),
        }
    }
    Ok(totals)
}

fn check_free_space(plan: &mut FsPlan) -> Result<()> {
    for (dir, needed) in bytes_per_target_dir(&plan.moves)? {
        if fs4::available_space(&dir)? < needed {
            plan.issues.push(FsIssue::NotEnoughSpace(dir, needed));
        }
    }
    Ok(())
}

fn copy_and_delete(from: &Path, to: &Path) -> Result<()> {
    if from.is_file() {
        let meta = std::fs::metadata(from)?;
        std::fs::copy(from, to)?;
        filetime::set_file_mtime(
            to,
            filetime::FileTime::from_last_modification_time(&meta),
        )?;
        std::fs::remove_file(from)?;
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in walkdir::WalkDir::new(from).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(from)?;
        let target = to.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            let meta = std::fs::metadata(entry.path())?;
            std::fs::copy(entry.path(), &target)?;
            filetime::set_file_mtime(
                &target,
                filetime::FileTime::from_last_modification_time(&meta),
            )?;
        }
    }
    std::fs::remove_dir_all(from)?;
    Ok(())
}

fn set_attr(path: &Path, attr: &FileAttr) -> Result<()> {
    match attr {
        FileAttr::Mtime(ts) => {
            let ft = filetime::FileTime::from_unix_time(ts.0, 0);
            filetime::set_file_mtime(path, ft)?;
            Ok(())
        }
    }
}

pub(crate) fn update_column(
    store: &crate::db::Store,
    target: crate::db::TargetTable,
    col: crate::db::Col,
    bitical_type: crate::types::BiticalType,
    updates: &[(i64, crate::types::Bitical)],
) -> Result<()> {
    use crate::db::Tbl;
    use crate::util::{ExecuteSql, IdenExt, ParquetExt, SelectExt};
    use sea_query::{Asterisk, Order, Query};

    let path = store.path_for_target(target);
    let tmp = Tbl::Target;
    crate::util::parquet_query(&path.to_string_lossy())
        .create_table_as(&store.conn, tmp)?;
    crate::edit::sql::column_case_update(tmp, col, bitical_type, updates)
        .execute(&store.conn)?;
    Query::select()
        .column(Asterisk)
        .from(tmp)
        .order_by(crate::db::Col::ItemId, Order::Asc)
        .to_owned()
        .save_parquet(&store.conn, &path)?;
    tmp.drop_table(&store.conn)?;
    Ok(())
}

fn rebind(store: &crate::db::Store, outcome: &FsOutcome) -> Result<()> {
    let updates: Vec<(i64, crate::types::Bitical)> = outcome
        .moved
        .iter()
        .filter(|m| m.crossed)
        .map(|m| {
            Ok((
                m.item.as_i64(),
                crate::types::Bitical::Uuid(crate::get_file_ref(&m.to)?),
            ))
        })
        .collect::<Result<_>>()?;
    if updates.is_empty() {
        return Ok(());
    }
    update_column(
        store,
        crate::db::TargetTable::FileReferences,
        crate::db::Col::FileId,
        crate::types::BiticalType::Uuid,
        &updates,
    )
}

impl FsOutcome {
    fn touched_dirs(&self, plan: &FsPlan) -> Vec<PathBuf> {
        self.moved
            .iter()
            .map(|m| m.to.clone())
            .chain(plan.moves.iter().map(|m| m.from.clone()))
            .chain(self.attrs_set.iter().map(|a| a.path.clone()))
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .fold(Vec::new(), |mut dirs, d| {
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
                dirs
            })
    }
}

fn reindex(
    store: &crate::db::Store,
    registry: &TagRegistry,
    plan: &FsPlan,
    outcome: &FsOutcome,
) -> Result<()> {
    if outcome.is_empty() {
        return Ok(());
    }
    rebind(store, outcome)?;
    crate::indexing::indexer::Indexer::new(store, registry).run(
        &outcome.touched_dirs(plan),
        None::<&fn(usize)>,
        false,
    )?;
    Ok(())
}

fn abort(
    store: &crate::db::Store,
    registry: &TagRegistry,
    plan: &FsPlan,
    outcome: FsOutcome,
    failed: &Path,
    cause: anyhow::Error,
) -> Result<FsOutcome> {
    reindex(store, registry, plan, &outcome)?;
    let done: Vec<String> = outcome
        .moved
        .iter()
        .map(|m| format!("{:?}", m.to))
        .collect();
    bail!(
        "failed on {failed:?}: {cause}\ncompleted before the failure:\n{}",
        done.join("\n")
    )
}

pub fn apply(
    store: &crate::db::Store,
    registry: &TagRegistry,
    plan: FsPlan,
) -> Result<FsOutcome> {
    for dir in &plan.mkdirs {
        std::fs::create_dir_all(dir)?;
    }
    let mut outcome = FsOutcome::default();
    for m in &plan.moves {
        let res = if m.crossed {
            copy_and_delete(&m.from, &m.to)
        } else {
            std::fs::rename(&m.from, &m.to).map_err(Into::into)
        };
        match res {
            Ok(()) => outcome.moved.push(Moved {
                item: m.item.clone(),
                to: m.to.clone(),
                crossed: m.crossed,
            }),
            Err(e) => {
                return abort(store, registry, &plan, outcome, &m.from, e)
            }
        }
    }
    for a in &plan.attrs {
        let path = outcome.target_of(&a.item).unwrap_or(&a.from).clone();
        match set_attr(&path, &a.attr) {
            Ok(()) => outcome.attrs_set.push(AttrSet {
                item: a.item.clone(),
                path,
            }),
            Err(e) => return abort(store, registry, &plan, outcome, &path, e),
        }
    }
    reindex(store, registry, &plan, &outcome)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_location_tag_duplicates() {
        let reg = TagRegistry::with_standard();

        // 互いに重複しない組み合わせは OK
        assert!(check_location_tag(
            &[TagType::Base(SType::Parentdir), TagType::Base(SType::Stem)],
            &reg
        )
        .is_ok());

        assert!(check_location_tag(
            &[TagType::Base(SType::Stem), TagType::Base(SType::Extension)],
            &reg
        )
        .is_ok());

        // 重複する組み合わせはエラー
        assert!(check_location_tag(
            &[TagType::Base(SType::Path), TagType::Base(SType::Stem)],
            &reg
        )
        .is_err());

        assert!(check_location_tag(
            &[TagType::Base(SType::Filename), TagType::Base(SType::Stem)],
            &reg
        )
        .is_err());

        assert!(check_location_tag(
            &[
                TagType::Base(SType::Filename),
                TagType::Base(SType::Extension)
            ],
            &reg
        )
        .is_err());

        assert!(check_location_tag(
            &[TagType::Base(SType::Stem), TagType::Base(SType::Stem)],
            &reg
        )
        .is_err());
    }

    #[test]
    fn test_is_fs_strategy() {
        assert!(is_fs_strategy(EditStrategy::Relocate));
        assert!(is_fs_strategy(EditStrategy::SetFileAttr));
        assert!(!is_fs_strategy(EditStrategy::Append));
        assert!(!is_fs_strategy(EditStrategy::Replace));
        assert!(!is_fs_strategy(EditStrategy::RemoveOnly));
        assert!(!is_fs_strategy(EditStrategy::ModifyInjection));
    }

    #[test]
    fn test_file_attr_of() {
        let reg = TagRegistry::with_standard();
        let attr = file_attr_of(
            &TagType::Base(SType::Mtime),
            &Label::from("2020-01-01"),
            &reg,
        )
        .unwrap();
        assert!(matches!(attr, FileAttr::Mtime(_)));

        assert!(file_attr_of(
            &TagType::Base(SType::Path),
            &Label::from("foo.txt"),
            &reg,
        )
        .is_err());
    }

    #[test]
    fn test_fs_issue_display() {
        let id = ItemId::Stored(42);
        let p1 = PathBuf::from("/a/b.txt");
        let p2 = PathBuf::from("/a/c.txt");
        let issue = FsIssue::MultipleLocations(id, vec![p1, p2], Vec::new());
        let s = issue.to_string();
        assert!(s.contains("42"));
        assert!(s.contains("/a/b.txt"));
        assert!(s.contains("/a/c.txt"));
    }

    fn open_test_store(dir: &Path, registry: &TagRegistry) -> crate::db::Store {
        let store = crate::db::Store::open(dir).unwrap();
        crate::indexing::indexer::Indexer::new(&store, registry)
            .initialize_tables()
            .unwrap();
        store
    }

    #[test]
    fn test_plan_fs_basic_rename() {
        use crate::types::{Bitical, Intrinsic, Origin, Rank, Tags, TypedTag};
        let reg = TagRegistry::with_standard();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello").unwrap();

        let mut tags = Tags::new();
        tags.push(
            TypedTag::new(
                TagType::Base(SType::Path),
                Bitical::String(p.to_string_lossy().to_string()),
            ),
            Origin::File,
        );
        let item = Item {
            id: ItemId::Stored(1),
            item_kind: crate::ItemKind::File,
            representative: vec![].into(),
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags,
            item_count: None,
        };

        let nodes = vec![ResolvedNode {
            tag_type: TagType::Base(SType::Stem),
            label: Some(Label::from("b")),
            strategy: EditStrategy::Relocate,
        }];

        let plan = plan_fs(&reg, vec![(item, nodes)], QueryType::Tag).unwrap();
        assert_eq!(plan.moves.len(), 1);
        assert_eq!(plan.moves[0].from, p);
        assert_eq!(plan.moves[0].to, dir.path().join("b.txt"));
        assert!(plan.issues.is_empty());

        let store = open_test_store(dir.path(), &reg);
        let outcome = apply(&store, &reg, plan).unwrap();
        assert_eq!(outcome.count(), 1);
        assert!(!p.exists());
        assert!(dir.path().join("b.txt").exists());
    }

    #[test]
    fn test_apply_crossed_copy_and_delete() {
        let reg = TagRegistry::with_standard();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("file.txt");
        let to = dir.path().join("sub").join("file.txt");
        std::fs::write(&p, "content").unwrap();

        let plan = FsPlan {
            moves: vec![FsMove {
                item: ItemId::Stored(1),
                from: p.clone(),
                to: to.clone(),
                crossed: true,
            }],
            attrs: vec![],
            mkdirs: vec![dir.path().join("sub")],
            issues: vec![],
        };

        let store = open_test_store(dir.path(), &reg);
        let outcome = apply(&store, &reg, plan).unwrap();
        assert_eq!(outcome.count(), 1);
        assert_eq!(outcome.moved[0].crossed, true);
        assert!(!p.exists());
        assert!(to.exists());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "content");
    }

    #[test]
    fn test_apply_sets_mtime() {
        let reg = TagRegistry::with_standard();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("file.txt");
        std::fs::write(&p, "content").unwrap();

        let plan = FsPlan {
            moves: vec![],
            attrs: vec![FsAttr {
                item: ItemId::Stored(1),
                from: p.clone(),
                attr: FileAttr::Mtime(crate::types::FileTimestamp(1577836800)),
            }],
            mkdirs: vec![],
            issues: vec![],
        };

        let store = open_test_store(dir.path(), &reg);
        let outcome = apply(&store, &reg, plan).unwrap();
        assert_eq!(outcome.count(), 1);
        assert_eq!(
            outcome.attrs_set,
            vec![AttrSet {
                item: ItemId::Stored(1),
                path: p.clone(),
            }]
        );

        let meta = std::fs::metadata(&p).unwrap();
        let ft = filetime::FileTime::from_last_modification_time(&meta);
        assert_eq!(ft.unix_seconds(), 1577836800);
    }
}
