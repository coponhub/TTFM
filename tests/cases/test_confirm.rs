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

use std::io::Cursor;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use ttfm::{
    config::{ConfirmMode, ConflictPolicy, HardlinkPolicy, SkipScope},
    db::Store,
    edit::{
        confirm::{
            ConfirmPrompt, ConfirmSummary, ConflictChoice, HardlinkChoice,
            ReplaceChoice,
        },
        edit, edit_with_io, edit_with_prompt, QueryType, WriteOptions,
    },
    indexing::Indexer,
    response::Item,
    tag::TagRegistry,
    types::{ItemId, Label, TagType},
    SearchOptions,
};

fn setup(files: &[&str]) -> (Store, TagRegistry, TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    for name in files {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, *name).unwrap();
    }
    let registry = TagRegistry::with_standard();
    let store = Store::open(base.join("db")).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None::<&fn(usize)>, false)
        .unwrap();
    (store, registry, dir, root)
}

fn find(store: &Store, registry: &TagRegistry, q: &str) -> Vec<Item> {
    ttfm::search::search_nowarn(store, registry, q, SearchOptions::default())
        .unwrap()
        .results
}

fn tag_item(
    store: &Store,
    registry: &TagRegistry,
    search_q: &str,
    edit_q: &str,
) {
    edit(
        store,
        registry,
        search_q,
        Some(edit_q),
        QueryType::Tag,
        None,
        WriteOptions::noconfirm(),
        &mut Vec::new(),
    )
    .unwrap();
}

struct MockPrompt {
    answer: bool,
    conflict_answer: ConflictChoice,
    hardlink_answer: HardlinkChoice,
    replace_answer: Option<ReplaceChoice>,
    mkdir_answer: bool,
    recorded_summary: Option<ConfirmSummary>,
}

impl ConfirmPrompt for MockPrompt {
    fn ask_confirmation(
        &mut self,
        summary: &ConfirmSummary,
    ) -> anyhow::Result<bool> {
        self.recorded_summary = Some(summary.clone());
        Ok(self.answer)
    }

    fn ask_replace_resolution(
        &mut self,
        _item: &ItemId,
        _tag_type: &TagType,
        candidates: &[Label],
    ) -> anyhow::Result<ReplaceChoice> {
        Ok(self
            .replace_answer
            .clone()
            .unwrap_or_else(|| ReplaceChoice {
                label: candidates[0].clone(),
                for_all: false,
            }))
    }

    fn ask_conflict_resolution(
        &mut self,
        _item: &ItemId,
        _target: &Path,
    ) -> anyhow::Result<ConflictChoice> {
        Ok(self.conflict_answer)
    }

    fn ask_hardlink_resolution(
        &mut self,
        _item: &ItemId,
        _candidate_moves: &[ttfm::edit::FsMove],
    ) -> anyhow::Result<HardlinkChoice> {
        Ok(self.hardlink_answer.clone())
    }

    fn ask_mkdir_confirmation(&mut self, _dir: &Path) -> anyhow::Result<bool> {
        Ok(self.mkdir_answer)
    }
}

#[test]
fn prompt_yes_applies_changes_with_structured_summary() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let opts = WriteOptions::interactive().on_confirm(ConfirmMode::Always);
    let mut prompt = MockPrompt {
        answer: true,
        conflict_answer: ConflictChoice::Abort,
        hardlink_answer: HardlinkChoice::Abort,
        replace_answer: None,
        mkdir_answer: true,
        recorded_summary: None,
    };
    let res = edit_with_prompt(
        &store,
        &registry,
        "filename:a.txt",
        Some("filename:renamed.txt"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
        &mut prompt,
    )
    .unwrap();
    assert_eq!(res.fs_ops, 1);
    assert!(root.join("renamed.txt").exists());
    assert!(!root.join("a.txt").exists());
    assert_eq!(
        prompt.recorded_summary,
        Some(ConfirmSummary {
            matched_items: 1,
            fs_ops: 1,
            tag_ops: 0,
            deleted_items: 0,
            is_registration: false,
            skipped_types: Vec::new(),
            cast_info: None,
        })
    );
}

#[test]
fn prompt_interactive_conflict_serial_resolves() {
    let (store, registry, _d, root) = setup(&["a.txt", "b.txt"]);
    let opts = WriteOptions::interactive().on_confirm(ConfirmMode::Always);
    let mut prompt = MockPrompt {
        answer: true,
        conflict_answer: ConflictChoice::Serial,
        hardlink_answer: HardlinkChoice::Abort,
        replace_answer: None,
        mkdir_answer: true,
        recorded_summary: None,
    };
    let res = edit_with_prompt(
        &store,
        &registry,
        "filename:a.txt",
        Some("filename:b.txt"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
        &mut prompt,
    )
    .unwrap();
    assert_eq!(res.fs_ops, 1);
    assert!(root.join("b.txt").exists());
    assert!(root.join("b_1.txt").exists());
}

#[test]
fn prompt_interactive_replace_resolution_with_cache() {
    let (store, registry, _d, _root) = setup(&["a.txt"]);
    tag_item(&store, &registry, "filename:a.txt", "project:alpha");
    tag_item(&store, &registry, "filename:a.txt", "project:beta");

    let opts = WriteOptions::interactive().on_confirm(ConfirmMode::Always);
    let mut prompt = MockPrompt {
        answer: true,
        conflict_answer: ConflictChoice::Abort,
        hardlink_answer: HardlinkChoice::Abort,
        replace_answer: Some(ReplaceChoice {
            label: Label::from("beta"),
            for_all: true,
        }),
        mkdir_answer: true,
        recorded_summary: None,
    };
    let res = edit_with_prompt(
        &store,
        &registry,
        "project:*",
        Some("name:{1}"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
        &mut prompt,
    )
    .unwrap();
    assert!(res.updated > 0);
    assert!(!find(&store, &registry, "name:beta").is_empty());
}

#[test]
fn prompt_interactive_hardlink_path_selection() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::hard_link(root.join("a.txt"), sub.join("a.txt")).unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None::<&fn(usize)>, false)
        .unwrap();

    let opts = WriteOptions::interactive().on_confirm(ConfirmMode::Always);
    let mut prompt = MockPrompt {
        answer: true,
        conflict_answer: ConflictChoice::Abort,
        hardlink_answer: HardlinkChoice::Selected(vec![0]),
        replace_answer: None,
        mkdir_answer: true,
        recorded_summary: None,
    };
    let res = edit_with_prompt(
        &store,
        &registry,
        "filename:a.txt",
        Some("filename:renamed.txt"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
        &mut prompt,
    )
    .unwrap();
    assert_eq!(res.fs_ops, 1);
    assert!(root.join("renamed.txt").exists());
    assert!(sub.join("a.txt").exists());
}

#[test]
fn test_hardlink_interactive_candidate_move_index_alignment() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::hard_link(root.join("a.txt"), sub.join("a.txt")).unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None::<&fn(usize)>, false)
        .unwrap();

    let dest = root.join("dest");
    let opts = WriteOptions::interactive().on_confirm(ConfirmMode::Always);
    let mut prompt = MockPrompt {
        answer: true,
        conflict_answer: ConflictChoice::Abort,
        hardlink_answer: HardlinkChoice::Selected(vec![0]),
        replace_answer: None,
        mkdir_answer: true,
        recorded_summary: None,
    };
    let res = edit_with_prompt(
        &store,
        &registry,
        "filename:a.txt",
        Some(&format!("parentdir:{}", dest.display())),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
        &mut prompt,
    )
    .unwrap();
    assert_eq!(res.fs_ops, 1);
}

#[test]
fn prompt_no_cancels_operation() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let opts = WriteOptions::interactive().on_confirm(ConfirmMode::Always);
    let mut input = Cursor::new(b"n\n");
    let mut output = Vec::new();
    let res = edit_with_io(
        &store,
        &registry,
        "filename:a.txt",
        Some("filename:renamed.txt"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
        &mut input,
        &mut output,
    )
    .unwrap();
    assert_eq!(res.fs_ops, 0);
    assert!(root.join("a.txt").exists());
    assert!(!root.join("renamed.txt").exists());
}

#[test]
fn conflict_abort_aborts_and_reports_error_when_target_exists() {
    let (store, registry, _d, root) = setup(&["a.txt", "b.txt"]);
    let opts = WriteOptions::noconfirm().on_conflict(ConflictPolicy::Abort);
    let res = edit(
        &store,
        &registry,
        "filename:a.txt",
        Some("filename:b.txt"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
    );
    assert!(res.is_err());
    assert!(root.join("a.txt").exists());
    assert!(root.join("b.txt").exists());
}

#[test]
fn conflict_skip_skips_conflicting_item_and_syncs_db_actions() {
    let (store, registry, _d, root) = setup(&["a.txt", "c.txt"]);
    let opts = WriteOptions::noconfirm()
        .on_conflict(ConflictPolicy::Skip)
        .skip_scope(SkipScope::Item);
    let res = edit(
        &store,
        &registry,
        "filename:a.txt | filename:c.txt",
        Some("filename:b.txt project:active"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
    )
    .unwrap();
    assert!(res.has_skipped);
    assert_eq!(res.fs_ops, 1);
    assert!(root.join("a.txt").exists());
    assert!(
        find(&store, &registry, "filename:a.txt & project:active").is_empty()
    );
    assert!(
        !find(&store, &registry, "filename:b.txt & project:active").is_empty()
    );
}

#[test]
fn conflict_serial_renames_with_numeric_suffix() {
    let (store, registry, _d, root) = setup(&["a.txt", "b.txt"]);
    let opts = WriteOptions::noconfirm().on_conflict(ConflictPolicy::Serial);
    let res = edit(
        &store,
        &registry,
        "filename:a.txt",
        Some("filename:b.txt"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(res.fs_ops, 1);
    assert!(root.join("b.txt").exists());
    assert!(root.join("b_1.txt").exists());
}

#[test]
fn conflict_first_picks_first_candidate_for_replace() {
    let (store, registry, _d, _root) = setup(&["a.txt"]);
    tag_item(&store, &registry, "filename:a.txt", "project:alpha");
    tag_item(&store, &registry, "filename:a.txt", "project:beta");

    let opts = WriteOptions::noconfirm().on_conflict(ConflictPolicy::First);
    let res = edit(
        &store,
        &registry,
        "project:*",
        Some("name:{1}"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
    )
    .unwrap();
    assert!(res.updated > 0);
    assert!(!find(&store, &registry, "name:alpha").is_empty());
}

#[test]
fn hardlink_abort_aborts_and_lists_paths() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    std::fs::hard_link(root.join("a.txt"), root.join("a_link.txt")).unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None::<&fn(usize)>, false)
        .unwrap();

    let opts = WriteOptions::noconfirm().on_hardlink(HardlinkPolicy::Abort);
    let res = edit(
        &store,
        &registry,
        "filename:a.txt",
        Some("filename:c.txt"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
    );
    assert!(res.is_err());
    let err_str = res.unwrap_err().to_string();
    assert!(err_str.contains("a.txt"));
    assert!(err_str.contains("a_link.txt"));
}

#[test]
fn hardlink_all_moves_all_links() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::hard_link(root.join("a.txt"), sub.join("a.txt")).unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None::<&fn(usize)>, false)
        .unwrap();

    let opts = WriteOptions::noconfirm().on_hardlink(HardlinkPolicy::All);
    let res = edit(
        &store,
        &registry,
        "filename:a.txt",
        Some("filename:renamed.txt"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(res.fs_ops, 2);
    assert!(root.join("renamed.txt").exists());
    assert!(sub.join("renamed.txt").exists());
}

#[test]
fn conflict_skip_preserves_existing_replace_tag() {
    let (store, registry, _d, _root) = setup(&["a.txt"]);
    tag_item(&store, &registry, "filename:a.txt", "name:original");
    tag_item(&store, &registry, "filename:a.txt", "project:alpha");
    tag_item(&store, &registry, "filename:a.txt", "project:beta");

    let opts = WriteOptions::noconfirm().on_conflict(ConflictPolicy::Skip);
    let _res = edit(
        &store,
        &registry,
        "project:*",
        Some("name:{1}"),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
    )
    .unwrap();
    assert!(!find(&store, &registry, "name:original").is_empty());
}

#[test]
fn hardlink_all_moves_all_links_to_new_directory() {
    let (store, registry, _d, root) = setup(&["a.txt"]);
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::hard_link(root.join("a.txt"), sub.join("a_sub.txt")).unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None::<&fn(usize)>, false)
        .unwrap();

    let new_dir = root.join("new_parent/nested");
    let opts = WriteOptions::noconfirm().on_hardlink(HardlinkPolicy::All);
    let res = edit(
        &store,
        &registry,
        "filename:a.txt",
        Some(&format!("parentdir:{}", new_dir.to_str().unwrap())),
        QueryType::Tag,
        None,
        opts,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(res.fs_ops, 2);
    assert!(new_dir.join("a.txt").exists());
    assert!(new_dir.join("a_sub.txt").exists());
}
