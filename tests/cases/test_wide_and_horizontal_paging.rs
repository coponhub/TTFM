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

use clap::Parser;
use tempfile::TempDir;
use ttfm::{
    cli::args::{Cli, Commands},
    cli::format::{print_results_with_options, FormatOptions},
    cli::interactive::command::{dispatch_command, parse_command},
    cli::interactive::state::State,
    config::Config,
    db::Store,
    edit::WriteOptions,
    indexing::Indexer,
    tag::TagRegistry,
    SearchOptions,
};

use super::test_terminal_width_formatting::TEST_MUTEX;

struct EnvVarGuard<'a>(&'a str);
impl<'a> Drop for EnvVarGuard<'a> {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

fn setup_wide_fixture() -> (Store, TagRegistry, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("test_sample.txt"), "hello world").unwrap();

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
fn test_cli_wide_argument_parsing() {
    let cli = Cli::try_parse_from(["ttfm", "search", "extension:rs", "--wide"])
        .unwrap();
    match cli.command {
        Some(Commands::Search { wide, .. }) => assert!(wide),
        _ => panic!("Expected Search command with wide=true"),
    }
    let cli_short =
        Cli::try_parse_from(["ttfm", "search", "extension:rs", "-w"]).unwrap();
    match cli_short.command {
        Some(Commands::Search { wide, .. }) => assert!(wide),
        _ => panic!("Expected Search command with wide=true"),
    }
}

#[test]
fn test_search_wide_option_prints_all_columns_without_truncation() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("COLUMNS", "60");
    let _env_guard = EnvVarGuard("COLUMNS");
    let (store, registry, _dir) = setup_wide_fixture();
    let response = ttfm::search::search_nowarn(
        &store,
        &registry,
        "extension:txt",
        SearchOptions::default(),
    )
    .unwrap();

    let mut out_normal = Vec::new();
    print_results_with_options(
        &store,
        &registry,
        &response,
        "extension:txt",
        100,
        &mut out_normal,
        FormatOptions {
            is_interactive: false,
            wide: false,
            col_offset: 0,
            ..Default::default()
        },
    );

    let mut out_wide = Vec::new();
    print_results_with_options(
        &store,
        &registry,
        &response,
        "extension:txt",
        100,
        &mut out_wide,
        FormatOptions {
            is_interactive: false,
            wide: true,
            col_offset: 0,
            ..Default::default()
        },
    );

    let text_normal = String::from_utf8(out_normal).unwrap();
    let text_wide = String::from_utf8(out_wide).unwrap();
    assert!(text_normal.contains("..."));
    assert!(!text_wide.contains("..."));
}

#[test]
fn test_interactive_horizontal_paging_next_and_prev_columns() {
    let _guard = TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("COLUMNS", "60");
    let _env_guard = EnvVarGuard("COLUMNS");
    let (store, registry, _dir) = setup_wide_fixture();
    let mut state = State::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();
    let last_path = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

    let cmd_s = parse_command("s extension:txt", false).unwrap();
    let mut out1 = Vec::new();
    let mut err1 = Vec::new();
    dispatch_command(
        cmd_s,
        &mut state,
        &store,
        &registry,
        &config,
        write_opts.clone(),
        &last_path,
        &mut std::io::Cursor::new(b""),
        &mut out1,
        &mut err1,
    )
    .unwrap();

    let clean1 = console::strip_ansi_codes(&String::from_utf8(out1).unwrap())
        .to_string();
    assert!(clean1.contains("item_id"));
    assert!(clean1.contains("..."));

    let cmd_next_col = parse_command(">", true).unwrap();
    let mut out2 = Vec::new();
    let mut err2 = Vec::new();
    dispatch_command(
        cmd_next_col,
        &mut state,
        &store,
        &registry,
        &config,
        write_opts.clone(),
        &last_path,
        &mut std::io::Cursor::new(b""),
        &mut out2,
        &mut err2,
    )
    .unwrap();

    let clean2 = console::strip_ansi_codes(&String::from_utf8(out2).unwrap())
        .to_string();
    assert!(clean2.contains("item_id"));
    assert!(clean2.contains("item_id  ..."));

    let cmd_prev_col = parse_command("<", true).unwrap();
    let mut out3 = Vec::new();
    let mut err3 = Vec::new();
    dispatch_command(
        cmd_prev_col,
        &mut state,
        &store,
        &registry,
        &config,
        write_opts,
        &last_path,
        &mut std::io::Cursor::new(b""),
        &mut out3,
        &mut err3,
    )
    .unwrap();

    let clean3 = console::strip_ansi_codes(&String::from_utf8(out3).unwrap())
        .to_string();
    assert!(clean3.contains("item_id"));
    assert!(!clean3.contains("item_id  ..."));
}
