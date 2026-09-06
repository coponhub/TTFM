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
use std::path::PathBuf;
use tempfile::TempDir;
use ttfm::{
    cli::interactive::run_interactive_stream, config::Config, db::Store,
    edit::WriteOptions, indexing::Indexer, tag::TagRegistry,
};

fn setup_env(files: &[&str]) -> (Store, TagRegistry, TempDir, PathBuf) {
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

#[test]
fn test_interactive_init_menu_and_quit() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"m\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("s : Search"));
    assert!(out_str.contains("i : Index directories"));
    assert!(out_str.contains("`s extension:rs`"));
}

#[test]
fn test_interactive_search_and_next_paging() {
    let files: Vec<String> =
        (0..45).map(|i| format!("file_{i:02}.rs")).collect();
    let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let (store, registry, _dir, _root) = setup_env(&file_refs);
    let input = Cursor::new(b"s extension:rs\nn\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(
        out_str.contains("results displayed") || out_str.contains("s(earch)")
    );
    assert!(out_str.contains("file_"));
}

#[test]
fn test_interactive_tag_quoted_and_auto_requery_refresh() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input =
        Cursor::new(b"s extension:txt\nt \"extension:txt\" project:alpha\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Updated tags"));
    assert!(out_str.contains("sample.txt"));
}

#[test]
fn test_interactive_tag_requires_quotes_and_error_message() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(
        b"s extension:txt\nt unquoted project:alpha\nt \"extension:txt\"\nq\n",
    );
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let err_str = String::from_utf8(err_out).unwrap();
    assert!(err_str.contains("double quotes"));
    assert!(err_str.contains("Missing required argument"));
}

#[test]
fn test_interactive_untag_rejects_empty_quotes() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(b"s extension:txt\nu \"extension:txt\" \"\"\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let err_str = String::from_utf8(err_out).unwrap();
    assert!(err_str.contains("Missing required argument"));
}

#[test]
fn test_interactive_rejects_empty_search_query() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(b"s \"\"\nt \"\" project:alpha\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let err_str = String::from_utf8(err_out).unwrap();
    assert!(
        err_str.contains("Empty query")
            || err_str.contains("Missing required argument")
            || err_str.contains("syntax error")
            || err_str.contains("Empty search query")
    );
}

#[test]
fn test_interactive_clear_index_with_prompt() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"clear\ny\ns *:*\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::interactive();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    let err_str = String::from_utf8(err_out).unwrap();
    assert!(out_str.contains("Clear indexed files? [y/N]: "));
    assert!(out_str.contains("File indexes cleared successfully."));
    assert!(err_str.contains("Index not found"));
}

#[test]
fn test_interactive_clear_index_with_prompt_cancelled() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"clear\nn\ns *:*\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::interactive();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Clear indexed files? [y/N]: "));
    assert!(out_str.contains("Cancelled."));
    assert!(out_str.contains("a.txt"));
}

#[test]
fn test_interactive_tag_quoted_second_argument() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(
        b"s extension:txt\nt \"extension:txt\" \"project:alpha\"\nq\n",
    );
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Updated tags"));
    assert!(out_str.contains("sample.txt"));
}

#[test]
fn test_interactive_tag_with_confirm_prompt() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(
        b"s extension:txt\nt \"extension:txt\" \"project:alpha\"\ny\nq\n",
    );
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let mut write_opts = WriteOptions::interactive();
    write_opts.confirm = ttfm::config::ConfirmMode::Always;

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Apply these changes? [y/N]: "));
    assert!(out_str.contains("Updated tags"));
}

#[test]
fn test_interactive_quit_hierarchical_navigation() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(b"s extension:txt\nq\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Welcome to ttfm interactive mode"));
    assert!(out_str.contains("q(uit to main menu)"));
    assert!(out_str.contains("clear : Clear indexed files (or `clear all`)"));
}

#[test]
fn test_interactive_clear_with_confirm_never_skips_prompt() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"clear\ns *:*\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let mut write_opts = WriteOptions::default();
    write_opts.confirm = ttfm::config::ConfirmMode::Never;

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("File indexes cleared successfully."));
    assert!(!out_str.contains("[y/N]"));
}

#[test]
fn test_interactive_search_internal_quotes_preserved() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(b"s \"extension:txt\"\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

    run_interactive_stream(
        &store,
        &registry,
        &config,
        write_opts,
        None,
        input,
        &mut output,
        &mut err_out,
        true,
    )
    .unwrap();

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("sample.txt"));
}

#[test]
fn test_interactive_tab_no_matches_transient_hint() {
    use reedline::ReedlineEvent;
    use std::sync::{Arc, Mutex};
    use ttfm::cli::interactive::{
        autocomplete::{InteractiveCompleter, InteractiveHinter},
        runner::InteractiveEditMode,
        state::State,
    };

    let state = Arc::new(Mutex::new(State::new()));
    let last_path = Arc::new(Mutex::new(".".to_string()));
    let current_line = Arc::new(Mutex::new("s nonexistent_query".to_string()));
    let hinter = Arc::new(Mutex::new(InteractiveHinter::with_current_line(
        state.clone(),
        last_path,
        current_line.clone(),
    )));
    let completer =
        Arc::new(Mutex::new(InteractiveCompleter::new(state.clone())));

    let mut edit_mode = InteractiveEditMode::with_hinter_and_completer(
        reedline::Emacs::new(
            ttfm::cli::interactive::runner::build_keybindings(),
        ),
        current_line.clone(),
        state.clone(),
        hinter.clone(),
        completer,
    );

    // Tab キーのイベント (UntilFound([HistoryHintComplete, Menu("completion_menu"), MenuNext]))
    let tab_event = ReedlineEvent::UntilFound(vec![
        ReedlineEvent::HistoryHintComplete,
        ReedlineEvent::Menu("completion_menu".to_string()),
        ReedlineEvent::MenuNext,
    ]);

    let event = edit_mode.handle_reedline_event(tab_event);

    // メニューを開かず (ReedlineEvent::Menu ではない)、再描画 (Repaint) を返し、
    // hinter から減光の " (no matches)" が得られること
    assert_eq!(event, ReedlineEvent::Repaint);
    let hint_str = hinter
        .lock()
        .unwrap()
        .handle_line("s nonexistent_query", 19);
    assert!(
        hint_str.contains("(no matches)"),
        "hint should contain (no matches), got: {hint_str:?}"
    );

    // 矢印キー (Left) を押すと一時メッセージがクリアされ、カーソル移動 (Left) が返ること
    let left_event = ReedlineEvent::UntilFound(vec![
        ReedlineEvent::MenuLeft,
        ReedlineEvent::Left,
    ]);
    let left_res = edit_mode.handle_reedline_event(left_event);
    assert_eq!(left_res, ReedlineEvent::Left);
    let hint_after_left = hinter
        .lock()
        .unwrap()
        .handle_line("s nonexistent_query", 18);
    assert!(
        !hint_after_left.contains("(no matches)"),
        "hint should be cleared after arrow key, got: {hint_after_left:?}"
    );
}
