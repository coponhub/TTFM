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

use std::sync::Mutex;
use tempfile::TempDir;
use ttfm::{
    cli::format::print_results, db::Store, indexing::Indexer, tag::TagRegistry,
    SearchOptions,
};

static TEST_MUTEX: Mutex<()> = Mutex::new(());

fn setup_fixture() -> (Store, TagRegistry, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("ascii_long_filename_for_test.txt"), "content")
        .unwrap();
    std::fs::write(root.join("日本語ファイル名_全角テスト.txt"), "content")
        .unwrap();

    let db_dir = base.join("db");
    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run_single(&root, None::<&fn(usize)>, false)
        .unwrap();
    (store, registry, dir)
}

#[test]
fn test_search_results_ascii_drops_columns_without_cell_truncation() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("COLUMNS", "60");
    let (store, registry, _dir) = setup_fixture();
    let response = ttfm::search::search_nowarn(
        &store,
        &registry,
        "extension:txt",
        SearchOptions::default(),
    )
    .unwrap();
    let mut out = Vec::new();
    print_results(
        &store,
        &registry,
        &response,
        "extension:txt",
        100,
        &mut out,
        false,
    );
    std::env::remove_var("COLUMNS");

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("item_id"));
    for line in text.lines().filter(|l| {
        !l.is_empty() && !l.contains("displayed") && !l.contains("available")
    }) {
        assert!(
            console::measure_text_width(line) <= 60 - 4,
            "Line width {} exceeds 56 for line: {:?}",
            console::measure_text_width(line),
            line
        );
        assert!(
            !line.contains("2026-09..."),
            "mtime should not be partially truncated inside cell, got: {}",
            line
        );
    }
}

#[test]
fn test_search_results_unicode_never_exceed_terminal_width() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("COLUMNS", "60");
    let (store, registry, _dir) = setup_fixture();
    let response = ttfm::search::search_nowarn(
        &store,
        &registry,
        "name:*全角*",
        SearchOptions::default(),
    )
    .unwrap();
    let mut out = Vec::new();
    print_results(
        &store,
        &registry,
        &response,
        "name:*全角*",
        100,
        &mut out,
        false,
    );
    std::env::remove_var("COLUMNS");

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("item_id"));
    for line in text.lines().filter(|l| {
        !l.is_empty() && !l.contains("displayed") && !l.contains("available")
    }) {
        assert!(
            console::measure_text_width(line) <= 60 - 4,
            "Unicode line width {} exceeds 56 for line: {:?}",
            console::measure_text_width(line),
            line
        );
    }
}
