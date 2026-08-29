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

use super::{
    fs_operate::{FsIssue, FsMove, FsPlan},
    parse::EditQuery,
    write::{DeleteTarget, TagOp, WriteAction},
    WriteOptions,
};
use crate::config::{ConflictPolicy, HardlinkPolicy, SkipScope};
use crate::response::Item;
use crate::types::{ItemId, Label, TagType, TypedTag};
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceChoice {
    pub label: Label,
    pub for_all: bool,
}

fn resolve_replace_conflicts(
    actions: Vec<WriteAction>,
    policy_override: Option<ConflictPolicy>,
    interactive: bool,
    prompt: &mut dyn ConfirmPrompt,
    skipped: &mut HashSet<ItemId>,
) -> Result<Vec<WriteAction>> {
    // 1. (ItemId, TagType) ごとの全 Replace ラベルを収集
    let mut item_replace_groups: HashMap<(ItemId, TagType), Vec<Label>> =
        HashMap::new();
    for action in &actions {
        if let WriteAction::Add { item, tags } = action {
            for op in tags {
                if let TagOp::Replace(tag) = op {
                    item_replace_groups
                        .entry((item.clone(), tag.tag_type()))
                        .or_default()
                        .push(tag.label.clone());
                }
            }
        }
    }

    // 2. 競合解決を行い、(ItemId, TagType) ごとの確定ラベルまたはスキップを決定
    let mut chosen_map: HashMap<(ItemId, TagType), Option<Label>> =
        HashMap::new();
    let mut cache: HashMap<(TagType, Vec<String>), Label> = HashMap::new();

    let mut sorted_keys: Vec<(ItemId, TagType)> =
        item_replace_groups.keys().cloned().collect();
    sorted_keys.sort_by(|(i1, t1), (i2, t2)| {
        i1.cmp(i2).then_with(|| t1.as_str().cmp(t2.as_str()))
    });

    for (item, tag_type) in sorted_keys {
        let labels = &item_replace_groups[&(item.clone(), tag_type.clone())];
        let mut distinct: Vec<Label> = Vec::new();
        for l in labels {
            if !distinct.contains(l) {
                distinct.push(l.clone());
            }
        }
        distinct.sort_by(|a, b| a.as_str().cmp(&b.as_str()));

        if distinct.len() == 1 {
            chosen_map.insert((item, tag_type), Some(distinct[0].clone()));
        } else {
            let distinct_strings: Vec<String> =
                distinct.iter().map(|l| l.as_str().to_string()).collect();
            let chosen = if let Some(cached) =
                cache.get(&(tag_type.clone(), distinct_strings.clone()))
            {
                Some(cached.clone())
            } else {
                match policy_override {
                    Some(ConflictPolicy::First) => {
                        let c = distinct[0].clone();
                        cache.insert(
                            (tag_type.clone(), distinct_strings),
                            c.clone(),
                        );
                        Some(c)
                    }
                    Some(ConflictPolicy::Abort) => {
                        bail!(
                            "ambiguous Replace for tag type '{tag_type}' on item {item}: multiple distinct values from capture expansion"
                        );
                    }
                    Some(ConflictPolicy::Skip) => None,
                    Some(ConflictPolicy::Serial) => {
                        bail!(
                            "serial policy is not supported for Replace tags"
                        );
                    }
                    None if interactive => {
                        let choice = prompt.ask_replace_resolution(
                            &item, &tag_type, &distinct,
                        )?;
                        if choice.for_all {
                            cache.insert(
                                (tag_type.clone(), distinct_strings),
                                choice.label.clone(),
                            );
                        }
                        Some(choice.label)
                    }
                    None => {
                        bail!(
                            "ambiguous Replace for tag type '{tag_type}' on item {item}: multiple distinct values from capture expansion"
                        );
                    }
                }
            };
            chosen_map.insert((item, tag_type), chosen);
        }
    }

    let mut skipped_replace_types: HashSet<(ItemId, TagType)> = HashSet::new();
    for ((item, tag_type), chosen) in &chosen_map {
        if chosen.is_none() {
            skipped_replace_types.insert((item.clone(), tag_type.clone()));
            skipped.insert(item.clone());
        }
    }

    // 3. actions を再構築（各 (ItemId, TagType) につき確定した Replace を1回だけ出力）
    let mut resolved_actions = Vec::new();
    let mut emitted_replace: HashSet<(ItemId, TagType)> = HashSet::new();

    for action in actions {
        match action {
            WriteAction::Add { item, tags } => {
                let mut new_tags = Vec::new();
                for op in tags {
                    match op {
                        TagOp::Append(tag) => new_tags.push(TagOp::Append(tag)),
                        TagOp::Replace(tag) => {
                            let key = (item.clone(), tag.tag_type());
                            if !emitted_replace.contains(&key) {
                                if let Some(Some(chosen_label)) =
                                    chosen_map.get(&key)
                                {
                                    new_tags.push(TagOp::Replace(
                                        TypedTag::retag(
                                            tag.tag_type(),
                                            chosen_label,
                                        ),
                                    ));
                                    emitted_replace.insert(key);
                                }
                            }
                        }
                    }
                }
                if !new_tags.is_empty() {
                    resolved_actions.push(WriteAction::Add {
                        item,
                        tags: new_tags,
                    });
                }
            }
            WriteAction::Delete { item, tags } => {
                let new_tags: Vec<DeleteTarget> = tags
                    .into_iter()
                    .filter(|t| match t {
                        DeleteTarget::Type(tt) => !skipped_replace_types
                            .contains(&(item.clone(), tt.clone())),
                        _ => true,
                    })
                    .collect();
                if !new_tags.is_empty() {
                    resolved_actions.push(WriteAction::Delete {
                        item,
                        tags: new_tags,
                    });
                }
            }
        }
    }

    Ok(resolved_actions)
}

fn next_serial_path(target: &Path, reserved: &HashSet<PathBuf>) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new(""));
    let stem = target.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = target.extension().and_then(|s| s.to_str());

    let mut n = 1;
    loop {
        let name = match ext {
            Some(e) => format!("{}_{}.{}", stem, n, e),
            None => format!("{}_{}", stem, n),
        };
        let cand = parent.join(name);
        if !cand.exists() && !reserved.contains(&cand) {
            return cand;
        }
        n += 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictChoice {
    Abort,
    Skip,
    Serial,
    SkipAll,
    SerialAll,
}

fn resolve_conflicts(
    moves: Vec<FsMove>,
    policy_override: Option<ConflictPolicy>,
    interactive: bool,
    prompt: &mut dyn ConfirmPrompt,
    skipped_items: &mut HashSet<ItemId>,
) -> Result<Vec<FsMove>> {
    let mut resolved = Vec::new();
    let mut reserved = HashSet::new();
    let mut for_all_choice: Option<ConflictChoice> = None;

    for m in moves {
        let exists = m.to.exists();
        let dup = reserved.contains(&m.to);
        if exists || dup {
            let choice = match policy_override {
                Some(ConflictPolicy::Abort) => ConflictChoice::Abort,
                Some(ConflictPolicy::Skip) => ConflictChoice::Skip,
                Some(ConflictPolicy::Serial) => ConflictChoice::Serial,
                Some(ConflictPolicy::First) => ConflictChoice::Abort,
                None => match for_all_choice {
                    Some(c) => c,
                    None if interactive => {
                        let c =
                            prompt.ask_conflict_resolution(&m.item, &m.to)?;
                        if matches!(
                            c,
                            ConflictChoice::SkipAll | ConflictChoice::SerialAll
                        ) {
                            for_all_choice = Some(c);
                        }
                        c
                    }
                    None => ConflictChoice::Abort,
                },
            };

            match choice {
                ConflictChoice::Abort => {
                    bail!("target already exists: {}", m.to.display());
                }
                ConflictChoice::Skip | ConflictChoice::SkipAll => {
                    skipped_items.insert(m.item);
                }
                ConflictChoice::Serial | ConflictChoice::SerialAll => {
                    let new_to = next_serial_path(&m.to, &reserved);
                    reserved.insert(new_to.clone());
                    resolved.push(FsMove {
                        item: m.item,
                        from: m.from,
                        to: new_to,
                        crossed: m.crossed,
                    });
                }
            }
        } else {
            reserved.insert(m.to.clone());
            resolved.push(m);
        }
    }
    Ok(resolved)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardlinkChoice {
    Abort,
    Skip,
    All,
    Selected(Vec<usize>),
    SkipAll,
    AllForSubsequent,
}

fn register_missing_parents(moves: &[FsMove], mkdirs: &mut Vec<PathBuf>) {
    let mut known: HashSet<PathBuf> = mkdirs.iter().cloned().collect();
    let new_dirs = moves
        .iter()
        .filter_map(|m| m.to.parent())
        .filter(|p| !p.exists())
        .map(Path::to_path_buf)
        .filter(|p| known.insert(p.clone()))
        .collect::<Vec<_>>();
    mkdirs.extend(new_dirs);
}

fn resolve_hardlinks(
    mut moves: Vec<FsMove>,
    issues: &[FsIssue],
    policy_override: Option<HardlinkPolicy>,
    interactive: bool,
    prompt: &mut dyn ConfirmPrompt,
    skipped_items: &mut HashSet<ItemId>,
    mkdirs: &mut Vec<PathBuf>,
) -> Result<Vec<FsMove>> {
    let mut for_all_choice: Option<HardlinkChoice> = None;

    for issue in issues {
        if let FsIssue::MultipleLocations(item, paths, candidate_moves) = issue
        {
            let choice = match policy_override {
                Some(HardlinkPolicy::Abort) => HardlinkChoice::Abort,
                Some(HardlinkPolicy::Skip) => HardlinkChoice::Skip,
                Some(HardlinkPolicy::All) => HardlinkChoice::All,
                None => match for_all_choice.clone() {
                    Some(c) => c,
                    None if interactive => {
                        let c = prompt.ask_hardlink_resolution(item, paths)?;
                        if matches!(
                            c,
                            HardlinkChoice::SkipAll
                                | HardlinkChoice::AllForSubsequent
                        ) {
                            for_all_choice = Some(c.clone());
                        }
                        c
                    }
                    None => HardlinkChoice::Abort,
                },
            };

            match choice {
                HardlinkChoice::Abort => {
                    let path_list = paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "item {} has multiple hardlinks: {}",
                        item,
                        path_list
                    );
                }
                HardlinkChoice::Skip | HardlinkChoice::SkipAll => {
                    skipped_items.insert(item.clone());
                }
                HardlinkChoice::All | HardlinkChoice::AllForSubsequent => {
                    moves.extend(candidate_moves.clone());
                }
                HardlinkChoice::Selected(indices) => {
                    for idx in indices {
                        if let Some(m) = candidate_moves.get(idx) {
                            moves.push(m.clone());
                        }
                    }
                }
            }
        }
    }
    register_missing_parents(&moves, mkdirs);
    Ok(moves)
}

fn sync_write_actions(
    actions: Vec<WriteAction>,
    skipped: &HashSet<ItemId>,
    scope: SkipScope,
) -> Vec<WriteAction> {
    match scope {
        SkipScope::Item => actions
            .into_iter()
            .filter(|a| match a {
                WriteAction::Add { item, .. } => !skipped.contains(item),
                WriteAction::Delete { item, .. } => !skipped.contains(item),
            })
            .collect(),
        SkipScope::FsOnly => actions,
    }
}

fn check_fatal_issues(issues: &[FsIssue]) -> Result<()> {
    for issue in issues {
        match issue {
            FsIssue::SourceMissing(..)
            | FsIssue::TargetNotWritable(..)
            | FsIssue::TargetInsideSource(..)
            | FsIssue::NotEnoughSpace(..)
            | FsIssue::ChainedMove(..)
            | FsIssue::NoLocation(..) => {
                bail!("{issue}");
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastSummary {
    pub tag_type: TagType,
    pub from_type: crate::types::BiticalType,
    pub to_type: crate::types::BiticalType,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmSummary {
    pub matched_items: usize,
    pub fs_ops: usize,
    pub tag_ops: usize,
    pub deleted_items: usize,
    pub is_registration: bool,
    pub skipped_types: Vec<TagType>,
    pub cast_info: Option<CastSummary>,
}

impl std::fmt::Display for ConfirmSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(c) = &self.cast_info {
            if c.count > 0 {
                writeln!(
                    f,
                    "Converting {} item(s) of type '{}' from {} to {}",
                    c.count, c.tag_type, c.from_type, c.to_type
                )?;
            }
        }
        if self.is_registration {
            write!(f, "Registering {} items", self.matched_items)?;
        } else {
            write!(
                f,
                "Matched: {} items ({} file operations, {} tag modifications)",
                self.matched_items, self.fs_ops, self.tag_ops
            )?;
        }
        if self.deleted_items > 0 {
            write!(
                f,
                " [WARNING: {} items will be deleted]",
                self.deleted_items
            )?;
        }
        if !self.skipped_types.is_empty() {
            let types = self
                .skipped_types
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            write!(f, " (Skipped non-transferable types: {})", types)?;
        }
        Ok(())
    }
}

pub trait ConfirmPrompt {
    fn ask_confirmation(&mut self, summary: &ConfirmSummary) -> Result<bool>;
    fn ask_replace_resolution(
        &mut self,
        item: &ItemId,
        tag_type: &TagType,
        candidates: &[Label],
    ) -> Result<ReplaceChoice>;
    fn ask_conflict_resolution(
        &mut self,
        item: &ItemId,
        target: &Path,
    ) -> Result<ConflictChoice>;
    fn ask_hardlink_resolution(
        &mut self,
        item: &ItemId,
        paths: &[PathBuf],
    ) -> Result<HardlinkChoice>;
    fn ask_mkdir_confirmation(&mut self, dir: &Path) -> Result<bool>;
}

pub struct IoConfirmPrompt<'a, R: BufRead, W: Write> {
    pub input: &'a mut R,
    pub output: &'a mut W,
}

impl<'a, R: BufRead, W: Write> ConfirmPrompt for IoConfirmPrompt<'a, R, W> {
    fn ask_confirmation(&mut self, summary: &ConfirmSummary) -> Result<bool> {
        writeln!(self.output, "{summary}")?;
        write!(self.output, "Apply these changes? [y/N]: ")?;
        self.output.flush()?;
        let mut buf = String::new();
        self.input.read_line(&mut buf)?;
        Ok(buf.trim().eq_ignore_ascii_case("y"))
    }

    fn ask_replace_resolution(
        &mut self,
        item: &ItemId,
        tag_type: &TagType,
        candidates: &[Label],
    ) -> Result<ReplaceChoice> {
        writeln!(
            self.output,
            "Multiple candidate values for tag '{tag_type}' on item {item}:"
        )?;
        let len = candidates.len();
        for (i, c) in candidates.iter().enumerate() {
            writeln!(self.output, "  [{}] {}", i + 1, c.as_str())?;
        }
        for (i, c) in candidates.iter().enumerate() {
            writeln!(
                self.output,
                "  [{}] {} (all subsequent)",
                len + i + 1,
                c.as_str()
            )?;
        }
        write!(self.output, "Choice [1-{}]: ", len * 2)?;
        self.output.flush()?;
        let mut buf = String::new();
        self.input.read_line(&mut buf)?;
        let idx: usize = buf.trim().parse().unwrap_or(1);
        let idx_0 = idx.saturating_sub(1);
        let for_all = idx_0 >= len;
        let cand_idx = if for_all { idx_0 - len } else { idx_0 };
        let label = candidates.get(cand_idx).unwrap_or(&candidates[0]).clone();
        Ok(ReplaceChoice { label, for_all })
    }

    fn ask_conflict_resolution(
        &mut self,
        item: &ItemId,
        target: &Path,
    ) -> Result<ConflictChoice> {
        writeln!(
            self.output,
            "Target already exists for item {item}: {}",
            target.display()
        )?;
        writeln!(
            self.output,
            "Resolve conflict: [1] abort (default), [2] skip, [3] serial, [4] skip all, [5] serial all"
        )?;
        write!(self.output, "Choice [1-5]: ")?;
        self.output.flush()?;
        let mut buf = String::new();
        self.input.read_line(&mut buf)?;
        match buf.trim() {
            "2" => Ok(ConflictChoice::Skip),
            "3" => Ok(ConflictChoice::Serial),
            "4" => Ok(ConflictChoice::SkipAll),
            "5" => Ok(ConflictChoice::SerialAll),
            _ => Ok(ConflictChoice::Abort),
        }
    }

    fn ask_hardlink_resolution(
        &mut self,
        item: &ItemId,
        paths: &[PathBuf],
    ) -> Result<HardlinkChoice> {
        writeln!(self.output, "Multiple hardlinks detected for item {item}:")?;
        let n = paths.len();
        for (i, p) in paths.iter().enumerate() {
            writeln!(
                self.output,
                "  [{}] move \"{}\" only",
                i + 1,
                p.display()
            )?;
        }
        writeln!(self.output, "  [{}] move all paths", n + 1)?;
        writeln!(
            self.output,
            "  [{}] move all for all subsequent hardlinks",
            n + 2
        )?;
        writeln!(self.output, "  [{}] skip this item", n + 3)?;
        writeln!(
            self.output,
            "  [{}] skip for all subsequent hardlinks",
            n + 4
        )?;
        writeln!(
            self.output,
            "  [{}] abort (cancel operation, default)",
            n + 5
        )?;
        writeln!(
            self.output,
            "  [Or enter comma-separated numbers, e.g. 1,2]"
        )?;
        write!(self.output, "Choice [1-{}]: ", n + 5)?;
        self.output.flush()?;
        let mut buf = String::new();
        self.input.read_line(&mut buf)?;
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            return Ok(HardlinkChoice::Abort);
        }
        if trimmed.contains(',') {
            let indices: Vec<usize> = trimmed
                .split(',')
                .filter_map(|s| s.trim().parse::<usize>().ok())
                .filter(|&idx| idx >= 1 && idx <= n)
                .map(|idx| idx - 1)
                .collect();
            return if indices.is_empty() {
                Ok(HardlinkChoice::Abort)
            } else {
                Ok(HardlinkChoice::Selected(indices))
            };
        }
        if let Ok(idx) = trimmed.parse::<usize>() {
            if idx >= 1 && idx <= n {
                return Ok(HardlinkChoice::Selected(vec![idx - 1]));
            }
            if idx == n + 1 {
                return Ok(HardlinkChoice::All);
            }
            if idx == n + 2 {
                return Ok(HardlinkChoice::AllForSubsequent);
            }
            if idx == n + 3 {
                return Ok(HardlinkChoice::Skip);
            }
            if idx == n + 4 {
                return Ok(HardlinkChoice::SkipAll);
            }
            if idx == n + 5 {
                return Ok(HardlinkChoice::Abort);
            }
        }
        Ok(HardlinkChoice::Abort)
    }

    fn ask_mkdir_confirmation(&mut self, dir: &Path) -> Result<bool> {
        write!(
            self.output,
            "Create missing directory '{}'? [y/N]: ",
            dir.display()
        )?;
        self.output.flush()?;
        let mut buf = String::new();
        self.input.read_line(&mut buf)?;
        Ok(buf.trim().eq_ignore_ascii_case("y"))
    }
}

#[allow(dead_code)]
pub fn confirm(
    fs_plan: FsPlan,
    actions: Vec<WriteAction>,
    item_edits: &[(Item, Option<EditQuery>)],
    options: &WriteOptions,
) -> Result<Option<(FsPlan, Vec<WriteAction>)>> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stderr();
    confirm_with_io(
        fs_plan,
        actions,
        item_edits,
        options,
        &mut input,
        &mut output,
    )
}

pub fn confirm_with_io<R: BufRead, W: Write>(
    fs_plan: FsPlan,
    actions: Vec<WriteAction>,
    item_edits: &[(Item, Option<EditQuery>)],
    options: &WriteOptions,
    input: &mut R,
    output: &mut W,
) -> Result<Option<(FsPlan, Vec<WriteAction>)>> {
    let mut prompt = IoConfirmPrompt { input, output };
    confirm_with_prompt(
        fs_plan,
        actions,
        item_edits,
        options,
        None,
        &mut prompt,
    )
}

pub fn confirm_with_prompt(
    mut fs_plan: FsPlan,
    actions: Vec<WriteAction>,
    item_edits: &[(Item, Option<EditQuery>)],
    options: &WriteOptions,
    cast_info: Option<CastSummary>,
    prompt: &mut dyn ConfirmPrompt,
) -> Result<Option<(FsPlan, Vec<WriteAction>)>> {
    check_fatal_issues(&fs_plan.issues)?;

    let is_interactive = options.is_interactive();

    let mut skipped = HashSet::new();
    let actions = resolve_replace_conflicts(
        actions,
        options.on_conflict,
        is_interactive,
        prompt,
        &mut skipped,
    )?;

    let moves = resolve_hardlinks(
        fs_plan.moves,
        &fs_plan.issues,
        options.on_hardlink,
        is_interactive,
        prompt,
        &mut skipped,
        &mut fs_plan.mkdirs,
    )?;
    let moves = resolve_conflicts(
        moves,
        options.on_conflict,
        is_interactive,
        prompt,
        &mut skipped,
    )?;

    if is_interactive {
        let mut asked = HashSet::new();
        for mkdir in &fs_plan.mkdirs {
            if asked.insert(mkdir.clone())
                && !prompt.ask_mkdir_confirmation(mkdir)?
            {
                return Ok(None);
            }
        }
    }

    fs_plan.moves = moves;
    let actions = sync_write_actions(actions, &skipped, options.skip_scope);

    let needs_prompt = is_interactive;

    if needs_prompt {
        let deleted_items = actions
            .iter()
            .filter(|a| match a {
                WriteAction::Delete { tags, .. } => tags.iter().any(|t| {
                    matches!(t, crate::edit::write::DeleteTarget::Type(tt) if tt.as_str() == "item_id")
                }),
                _ => false,
            })
            .count();
        let summary = ConfirmSummary {
            matched_items: item_edits.len(),
            fs_ops: fs_plan.moves.len() + fs_plan.attrs.len(),
            tag_ops: actions.len(),
            deleted_items,
            is_registration: item_edits.iter().all(|(_, q)| q.is_none()),
            skipped_types: Vec::new(),
            cast_info,
        };
        if !prompt.ask_confirmation(&summary)? {
            return Ok(None);
        }
    }

    Ok(Some((fs_plan, actions)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfirmMode;
    use tempfile::tempdir;

    #[test]
    fn test_next_serial_path() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("foo.txt");
        let reserved = HashSet::new();
        assert_eq!(
            next_serial_path(&p, &reserved),
            dir.path().join("foo_1.txt")
        );

        std::fs::write(dir.path().join("foo_1.txt"), "x").unwrap();
        assert_eq!(
            next_serial_path(&p, &reserved),
            dir.path().join("foo_2.txt")
        );
    }

    struct DummyPrompt;
    impl ConfirmPrompt for DummyPrompt {
        fn ask_confirmation(
            &mut self,
            _summary: &ConfirmSummary,
        ) -> Result<bool> {
            Ok(true)
        }
        fn ask_replace_resolution(
            &mut self,
            _item: &ItemId,
            _tag_type: &TagType,
            candidates: &[Label],
        ) -> Result<ReplaceChoice> {
            Ok(ReplaceChoice {
                label: candidates[0].clone(),
                for_all: false,
            })
        }
        fn ask_conflict_resolution(
            &mut self,
            _item: &ItemId,
            _target: &Path,
        ) -> Result<ConflictChoice> {
            Ok(ConflictChoice::Abort)
        }
        fn ask_hardlink_resolution(
            &mut self,
            _item: &ItemId,
            _paths: &[PathBuf],
        ) -> Result<HardlinkChoice> {
            Ok(HardlinkChoice::Abort)
        }
        fn ask_mkdir_confirmation(&mut self, _dir: &Path) -> Result<bool> {
            Ok(true)
        }
    }

    #[test]
    fn test_resolve_conflicts_serial() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("b.txt");
        std::fs::write(&to, "exists").unwrap();

        let moves = vec![FsMove {
            item: ItemId::Stored(1),
            from: from.clone(),
            to: to.clone(),
            crossed: false,
        }];
        let mut skipped = HashSet::new();
        let mut prompt = DummyPrompt;
        let res = resolve_conflicts(
            moves,
            Some(ConflictPolicy::Serial),
            false,
            &mut prompt,
            &mut skipped,
        )
        .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].to, dir.path().join("b_1.txt"));
        assert!(skipped.is_empty());
    }

    #[test]
    fn test_resolve_conflicts_skip() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("b.txt");
        std::fs::write(&to, "exists").unwrap();

        let moves = vec![FsMove {
            item: ItemId::Stored(1),
            from,
            to,
            crossed: false,
        }];
        let mut skipped = HashSet::new();
        let mut prompt = DummyPrompt;
        let res = resolve_conflicts(
            moves,
            Some(ConflictPolicy::Skip),
            false,
            &mut prompt,
            &mut skipped,
        )
        .unwrap();
        assert_eq!(res.len(), 0);
        assert!(skipped.contains(&ItemId::Stored(1)));
    }

    #[test]
    fn test_resolve_conflicts_abort() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("b.txt");
        std::fs::write(&to, "exists").unwrap();

        let moves = vec![FsMove {
            item: ItemId::Stored(1),
            from,
            to,
            crossed: false,
        }];
        let mut skipped = HashSet::new();
        let mut prompt = DummyPrompt;
        let res = resolve_conflicts(
            moves,
            Some(ConflictPolicy::Abort),
            false,
            &mut prompt,
            &mut skipped,
        );
        assert!(res.is_err());
    }

    #[test]
    fn test_resolve_hardlinks_all_and_skip() {
        let p1 = PathBuf::from("/a/1.txt");
        let p2 = PathBuf::from("/a/2.txt");
        let cand = vec![
            FsMove {
                item: ItemId::Stored(1),
                from: p1.clone(),
                to: PathBuf::from("/b/1.txt"),
                crossed: false,
            },
            FsMove {
                item: ItemId::Stored(1),
                from: p2.clone(),
                to: PathBuf::from("/b/2.txt"),
                crossed: false,
            },
        ];
        let issues = vec![FsIssue::MultipleLocations(
            ItemId::Stored(1),
            vec![p1, p2],
            cand,
        )];

        let mut skipped = HashSet::new();
        let mut mkdirs = Vec::new();
        let mut prompt = DummyPrompt;
        let moves = resolve_hardlinks(
            Vec::new(),
            &issues,
            Some(HardlinkPolicy::All),
            false,
            &mut prompt,
            &mut skipped,
            &mut mkdirs,
        )
        .unwrap();
        assert_eq!(moves.len(), 2);
        assert!(skipped.is_empty());

        let mut skipped_skip = HashSet::new();
        let mut mkdirs_skip = Vec::new();
        let moves_skip = resolve_hardlinks(
            Vec::new(),
            &issues,
            Some(HardlinkPolicy::Skip),
            false,
            &mut prompt,
            &mut skipped_skip,
            &mut mkdirs_skip,
        )
        .unwrap();
        assert_eq!(moves_skip.len(), 0);
        assert!(skipped_skip.contains(&ItemId::Stored(1)));
    }

    #[test]
    fn test_sync_write_actions_item_vs_fsonly() {
        let mut skipped = HashSet::new();
        skipped.insert(ItemId::Stored(1));

        let actions = vec![
            WriteAction::Add {
                item: ItemId::Stored(1),
                tags: Vec::new(),
            },
            WriteAction::Add {
                item: ItemId::Stored(2),
                tags: Vec::new(),
            },
        ];

        let item_synced =
            sync_write_actions(actions.clone(), &skipped, SkipScope::Item);
        assert_eq!(item_synced.len(), 1);

        let fs_synced =
            sync_write_actions(actions, &skipped, SkipScope::FsOnly);
        assert_eq!(fs_synced.len(), 2);
    }

    #[test]
    fn test_confirm_with_io_prompt_yes_and_no() {
        let opts = WriteOptions::interactive().on_confirm(ConfirmMode::Always);

        // yes
        let mut input_yes = std::io::Cursor::new(b"y\n");
        let mut output_yes = Vec::new();
        let res_yes = confirm_with_io(
            FsPlan::default(),
            Vec::new(),
            &[],
            &opts,
            &mut input_yes,
            &mut output_yes,
        )
        .unwrap();
        assert!(res_yes.is_some());

        // no
        let mut input_no = std::io::Cursor::new(b"n\n");
        let mut output_no = Vec::new();
        let res_no = confirm_with_io(
            FsPlan::default(),
            Vec::new(),
            &[],
            &opts,
            &mut input_no,
            &mut output_no,
        )
        .unwrap();
        assert!(res_no.is_none());
    }

    #[test]
    fn test_confirm_summary_display() {
        let summary = ConfirmSummary {
            matched_items: 3,
            fs_ops: 2,
            tag_ops: 5,
            deleted_items: 0,
            is_registration: false,
            skipped_types: Vec::new(),
            cast_info: None,
        };
        assert_eq!(
            summary.to_string(),
            "Matched: 3 items (2 file operations, 5 tag modifications)"
        );

        let cast_summary = ConfirmSummary {
            matched_items: 1,
            fs_ops: 0,
            tag_ops: 1,
            deleted_items: 0,
            is_registration: false,
            skipped_types: Vec::new(),
            cast_info: Some(CastSummary {
                tag_type: TagType::from("score"),
                from_type: crate::types::BiticalType::String,
                to_type: crate::types::BiticalType::Integer,
                count: 10,
            }),
        };
        assert!(cast_summary.to_string().contains(
            "Converting 10 item(s) of type 'score' from string to integer"
        ));
    }

    struct CountingPrompt {
        pub mkdir_count: usize,
    }
    impl ConfirmPrompt for CountingPrompt {
        fn ask_confirmation(&mut self, _: &ConfirmSummary) -> Result<bool> {
            Ok(true)
        }
        fn ask_replace_resolution(
            &mut self,
            _: &ItemId,
            _: &TagType,
            c: &[Label],
        ) -> Result<ReplaceChoice> {
            Ok(ReplaceChoice {
                label: c[0].clone(),
                for_all: false,
            })
        }
        fn ask_conflict_resolution(
            &mut self,
            _: &ItemId,
            _: &Path,
        ) -> Result<ConflictChoice> {
            Ok(ConflictChoice::Abort)
        }
        fn ask_hardlink_resolution(
            &mut self,
            _: &ItemId,
            _: &[PathBuf],
        ) -> Result<HardlinkChoice> {
            Ok(HardlinkChoice::Abort)
        }
        fn ask_mkdir_confirmation(&mut self, _: &Path) -> Result<bool> {
            self.mkdir_count += 1;
            Ok(true)
        }
    }

    #[test]
    fn test_mkdir_confirmation_deduplicates_prompts() {
        let opts = WriteOptions::interactive().on_confirm(ConfirmMode::Always);
        let same_dir = PathBuf::from("/tmp/some_new_dir");
        let fs_plan = FsPlan {
            moves: vec![],
            attrs: vec![],
            mkdirs: vec![same_dir.clone(), same_dir.clone(), same_dir.clone()],
            issues: vec![],
        };
        let mut prompt = CountingPrompt { mkdir_count: 0 };
        let res =
            confirm_with_prompt(fs_plan, vec![], &[], &opts, None, &mut prompt)
                .unwrap();
        assert!(res.is_some());
        assert_eq!(prompt.mkdir_count, 1);
    }
}
