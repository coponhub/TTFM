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

use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use ttfm::{
    cli::progress::MultiStageProgressView,
    db::Store,
    indexing::{IndexProgress, Indexer},
    tag::TagRegistry,
};

#[test]
fn test_indexer_emits_all_progress_stages_in_strict_order() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "hello").unwrap();
    std::fs::write(root.join("b.txt"), "world").unwrap();

    let db_dir = dir.path().join("db");
    let store = Store::open(&db_dir).unwrap();
    let registry = TagRegistry::with_standard();
    Indexer::new(&store, &registry).initialize_tables().unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);

    let count = Indexer::new(&store, &registry)
        .run(
            &[&root],
            Some(&move |p: IndexProgress| {
                events_clone.lock().unwrap().push(p);
            }),
            false,
        )
        .unwrap();

    assert_eq!(count, 3);
    let captured = events.lock().unwrap().clone();

    let scan_idx = captured
        .iter()
        .position(|e| matches!(e, IndexProgress::Scanning { .. }))
        .expect("Scanning event missing");
    let diff_idx = captured
        .iter()
        .position(|e| matches!(e, IndexProgress::Diffing))
        .expect("Diffing event missing");
    let ext_idx = captured
        .iter()
        .position(|e| matches!(e, IndexProgress::Extracting { .. }))
        .expect("Extracting event missing");
    let merge_idx = captured
        .iter()
        .position(|e| matches!(e, IndexProgress::Merging))
        .expect("Merging event missing");

    assert!(scan_idx < diff_idx);
    assert!(diff_idx < ext_idx);
    assert!(ext_idx < merge_idx);
}

#[test]
fn test_indexer_emits_initial_extracting_event_even_when_empty() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "hello").unwrap();

    let db_dir = dir.path().join("db");
    let store = Store::open(&db_dir).unwrap();
    let registry = TagRegistry::with_standard();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None, false)
        .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);

    Indexer::new(&store, &registry)
        .run(
            &[&root],
            Some(&move |p: IndexProgress| {
                events_clone.lock().unwrap().push(p);
            }),
            false,
        )
        .unwrap();

    let captured = events.lock().unwrap().clone();
    assert!(captured.iter().any(|e| matches!(
        e,
        IndexProgress::Extracting {
            current: 0,
            total: 0
        }
    )));
}

#[test]
fn test_interactive_index_command_passes_progress_view() {
    use ttfm::cli::interactive::command::{dispatch_command, Command};
    use ttfm::cli::interactive::state::State;
    use ttfm::config::Config;
    use ttfm::edit::WriteOptions;

    let dir = TempDir::new().unwrap();
    let root = dir.path().join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("c.txt"), "test").unwrap();

    let db_dir = dir.path().join("db");
    let store = Store::open(&db_dir).unwrap();
    let registry = TagRegistry::with_standard();
    Indexer::new(&store, &registry).initialize_tables().unwrap();

    let mut state = State::new();
    let last_path = Arc::new(Mutex::new(".".to_string()));
    let mut stdin = std::io::empty();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    dispatch_command(
        Command::Index(vec![root.clone()]),
        &mut state,
        &store,
        &registry,
        &Config::default(),
        WriteOptions::default(),
        &last_path,
        &mut stdin,
        &mut stdout,
        &mut stderr,
    )
    .unwrap();

    let output_str = String::from_utf8(stdout).unwrap();
    assert!(output_str.contains("Indexed 2 files."));

    let view = MultiStageProgressView::hidden();
    let n = Indexer::new(&store, &registry)
        .run(&[&root], Some(&|p| view.handle_progress(p)), false)
        .unwrap();
    assert_eq!(n, 2);
}
