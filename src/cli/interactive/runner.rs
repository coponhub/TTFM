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

use crate::cli::interactive::autocomplete::{
    InteractiveCompleter, InteractiveHinter,
};
use crate::cli::interactive::command::{
    dispatch_command, parse_command, print_menu,
};
use crate::cli::interactive::state::State;
use crate::config::Config;
use crate::db::Store;
use crate::edit::WriteOptions;
use crate::tag::TagRegistry;
use reedline::{
    default_emacs_keybindings, ColumnarMenu, Completer, EditCommand, EditMode,
    Emacs, FileBackedHistory, Hinter, KeyCode, KeyModifiers, MenuBuilder,
    PromptEditMode, Reedline, ReedlineEvent, ReedlineMenu, ReedlineRawEvent,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

pub struct SharedHinter(pub Arc<Mutex<InteractiveHinter>>);

impl reedline::Hinter for SharedHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn reedline::History,
        use_ansi_coloring: bool,
        cwd: &str,
    ) -> String {
        if let Ok(mut h) = self.0.lock() {
            h.handle(line, pos, history, use_ansi_coloring, cwd)
        } else {
            String::new()
        }
    }

    fn complete_hint(&self) -> String {
        if let Ok(h) = self.0.lock() {
            h.complete_hint()
        } else {
            String::new()
        }
    }

    fn next_hint_token(&self) -> String {
        if let Ok(h) = self.0.lock() {
            h.next_hint_token()
        } else {
            String::new()
        }
    }
}

#[derive(Clone)]
pub struct SharedCompleter(pub Arc<Mutex<InteractiveCompleter>>);

impl reedline::Completer for SharedCompleter {
    fn complete(
        &mut self,
        line: &str,
        pos: usize,
    ) -> Vec<reedline::Suggestion> {
        if let Ok(mut c) = self.0.lock() {
            c.complete(line, pos)
        } else {
            vec![]
        }
    }
}

pub struct InteractiveEditMode {
    inner: Emacs,
    current_line: Arc<Mutex<String>>,
    state: Arc<Mutex<State>>,
    hinter: Option<Arc<Mutex<InteractiveHinter>>>,
    completer: Option<Arc<Mutex<InteractiveCompleter>>>,
}

impl InteractiveEditMode {
    pub fn new(
        inner: Emacs,
        current_line: Arc<Mutex<String>>,
        state: Arc<Mutex<State>>,
    ) -> Self {
        Self {
            inner,
            current_line,
            state,
            hinter: None,
            completer: None,
        }
    }

    pub fn with_hinter_and_completer(
        inner: Emacs,
        current_line: Arc<Mutex<String>>,
        state: Arc<Mutex<State>>,
        hinter: Arc<Mutex<InteractiveHinter>>,
        completer: Arc<Mutex<InteractiveCompleter>>,
    ) -> Self {
        Self {
            inner,
            current_line,
            state,
            hinter: Some(hinter),
            completer: Some(completer),
        }
    }

    pub fn handle_reedline_event(
        &mut self,
        event: ReedlineEvent,
    ) -> ReedlineEvent {
        match event {
            ReedlineEvent::Esc => {
                if let Some(h) = self.hinter.as_ref().and_then(|h| h.lock().ok()) {
                    h.clear_transient_message();
                }
                let is_empty = self
                    .current_line
                    .lock()
                    .map(|l| l.trim().is_empty())
                    .unwrap_or(true);
                if is_empty {
                    let is_searched = self
                        .state
                        .lock()
                        .map(|s| matches!(*s, State::Searched { .. }))
                        .unwrap_or(false);
                    if is_searched {
                        ReedlineEvent::ExecuteHostCommand("q".to_string())
                    } else {
                        ReedlineEvent::None
                    }
                } else {
                    ReedlineEvent::Edit(vec![EditCommand::Clear])
                }
            }
            ReedlineEvent::UntilFound(ref subcmds)
                if subcmds.iter().any(|c| matches!(c, ReedlineEvent::Menu(name) if name == "completion_menu")) =>
            {
                // Tab キー押下時の補完・ヒント判定
                let has_hint = self
                    .hinter
                    .as_ref()
                    .and_then(|h| h.lock().ok())
                    .map(|h| !h.complete_hint().is_empty())
                    .unwrap_or(false);

                if has_hint {
                    return event;
                }

                let line = self
                    .current_line
                    .lock()
                    .map(|l| l.clone())
                    .unwrap_or_default();
                let has_suggestions = self
                    .completer
                    .as_ref()
                    .and_then(|c| c.lock().ok())
                    .map(|mut c| !c.complete(&line, line.len()).is_empty())
                    .unwrap_or(false);

                if has_suggestions {
                    if let Some(h) = self.hinter.as_ref().and_then(|h| h.lock().ok()) {
                        h.clear_transient_message();
                    }
                    event
                } else {
                    if let Some(h) = self.hinter.as_ref().and_then(|h| h.lock().ok()) {
                        h.set_transient_message(" (no matches)");
                    }
                    ReedlineEvent::Repaint
                }
            }
            ReedlineEvent::UntilFound(ref subcmds)
                if subcmds.contains(&ReedlineEvent::MenuLeft) =>
            {
                if let Some(h) = self.hinter.as_ref().and_then(|h| h.lock().ok()) {
                    h.clear_transient_message();
                }
                ReedlineEvent::Left
            }
            ReedlineEvent::UntilFound(ref subcmds)
                if subcmds.contains(&ReedlineEvent::MenuRight) =>
            {
                if let Some(h) = self.hinter.as_ref().and_then(|h| h.lock().ok()) {
                    h.clear_transient_message();
                }
                event
            }
            ReedlineEvent::UntilFound(ref subcmds)
                if subcmds.contains(&ReedlineEvent::MenuUp) =>
            {
                if let Some(h) = self.hinter.as_ref().and_then(|h| h.lock().ok()) {
                    h.clear_transient_message();
                }
                ReedlineEvent::Up
            }
            ReedlineEvent::UntilFound(ref subcmds)
                if subcmds.contains(&ReedlineEvent::MenuDown) =>
            {
                if let Some(h) = self.hinter.as_ref().and_then(|h| h.lock().ok()) {
                    h.clear_transient_message();
                }
                ReedlineEvent::Down
            }
            ReedlineEvent::Left
            | ReedlineEvent::Right
            | ReedlineEvent::Up
            | ReedlineEvent::Down
            | ReedlineEvent::Edit(_) => {
                if let Some(h) = self.hinter.as_ref().and_then(|h| h.lock().ok()) {
                    h.clear_transient_message();
                }
                event
            }
            other => other,
        }
    }
}

impl EditMode for InteractiveEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let res = self.inner.parse_event(event);
        self.handle_reedline_event(res)
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

pub fn build_keybindings() -> reedline::Keybindings {
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Enter,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Submit,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Right,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::MenuRight,
            ReedlineEvent::Right,
        ]),
    );
    keybindings
}

pub fn run_interactive_terminal(
    store: &Store,
    registry: &TagRegistry,
    config: &Config,
    write_options: WriteOptions,
    initial_query: Option<&str>,
    quiet: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(Mutex::new(State::new()));
    let last_path = Arc::new(Mutex::new(".".to_string()));
    let current_line = Arc::new(Mutex::new(String::new()));

    let home_dir = crate::get_ttfm_home()
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&home_dir);
    let history_path = home_dir.join("history");
    let history: Box<dyn reedline::History> =
        match FileBackedHistory::with_file(1000, history_path) {
            Ok(h) => Box::new(h),
            Err(_) => Box::new(FileBackedHistory::default()),
        };

    let keybindings = build_keybindings();

    let completer =
        Arc::new(Mutex::new(InteractiveCompleter::new(state.clone())));
    let hinter = Arc::new(Mutex::new(InteractiveHinter::with_current_line(
        state.clone(),
        last_path.clone(),
        current_line.clone(),
    )));

    let edit_mode = Box::new(InteractiveEditMode::with_hinter_and_completer(
        Emacs::new(keybindings),
        current_line.clone(),
        state.clone(),
        hinter.clone(),
        completer.clone(),
    ));

    let completion_menu =
        Box::new(ColumnarMenu::default().with_name("completion_menu"));

    let mut line_editor = Reedline::create()
        .with_hinter(Box::new(SharedHinter(hinter)))
        .with_completer(Box::new(SharedCompleter(completer)))
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_history(history)
        .with_edit_mode(edit_mode);

    if !quiet && initial_query.is_none() {
        print_menu(&mut std::io::stdout(), &state.lock().unwrap())?;
    }

    if let Some(q) = initial_query {
        match parse_command(&format!("s {q}"), false) {
            Ok(cmd) => {
                let stdin = std::io::stdin();
                let mut stdin_lock = stdin.lock();
                if let Err(e) = dispatch_command(
                    cmd,
                    &mut state.lock().unwrap(),
                    store,
                    registry,
                    config,
                    write_options.clone(),
                    &last_path,
                    &mut stdin_lock,
                    &mut std::io::stdout(),
                    &mut std::io::stderr(),
                ) {
                    eprintln!("Error: {e}");
                }
            }
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    loop {
        if let Ok(mut l) = current_line.lock() {
            l.clear();
        }
        let prompt_str = state.lock().unwrap().prompt_string();
        let prompt = reedline::DefaultPrompt::new(
            reedline::DefaultPromptSegment::Basic(prompt_str),
            reedline::DefaultPromptSegment::Empty,
        );

        match line_editor.read_line(&prompt) {
            Ok(reedline::Signal::Success(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let is_searched =
                    matches!(*state.lock().unwrap(), State::Searched { .. });
                match parse_command(trimmed, is_searched) {
                    Ok(cmd) => {
                        let stdin = std::io::stdin();
                        let mut stdin_lock = stdin.lock();
                        match dispatch_command(
                            cmd,
                            &mut state.lock().unwrap(),
                            store,
                            registry,
                            config,
                            write_options.clone(),
                            &last_path,
                            &mut stdin_lock,
                            &mut std::io::stdout(),
                            &mut std::io::stderr(),
                        ) {
                            Ok(cont) => {
                                if !cont {
                                    break;
                                }
                            }
                            Err(e) => {
                                eprintln!("Error: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("{e}");
                    }
                }
            }
            Ok(reedline::Signal::CtrlC) => {
                let was_empty = current_line
                    .lock()
                    .map(|l| l.trim().is_empty())
                    .unwrap_or(true);
                let is_searched = state
                    .lock()
                    .map(|s| matches!(*s, State::Searched { .. }))
                    .unwrap_or(false);
                if was_empty && is_searched {
                    let mut st = state.lock().unwrap();
                    st.clear();
                    print_menu(&mut std::io::stdout(), &st)?;
                }
                continue;
            }
            Ok(reedline::Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
    Ok(())
}

pub fn run_interactive_stream<R: Read, W: Write, E: Write>(
    store: &Store,
    registry: &TagRegistry,
    config: &Config,
    write_options: WriteOptions,
    initial_query: Option<&str>,
    input: R,
    output: &mut W,
    err_out: &mut E,
    quiet: bool,
) -> std::io::Result<()> {
    if !quiet {
        writeln!(
            err_out,
            "Warning: Running interactive mode in non-TTY pipe."
        )?;
    }
    let mut state = State::new();
    let last_path = Arc::new(Mutex::new(".".to_string()));
    let mut reader = BufReader::new(input);

    if !quiet && initial_query.is_none() {
        let _ = print_menu(output, &state);
    }

    if let Some(q) = initial_query {
        match parse_command(&format!("s {q}"), false) {
            Ok(cmd) => {
                let _ = dispatch_command(
                    cmd,
                    &mut state,
                    store,
                    registry,
                    config,
                    write_options.clone(),
                    &last_path,
                    &mut reader,
                    output,
                    err_out,
                );
            }
            Err(e) => {
                let _ = writeln!(err_out, "Error: {e}");
            }
        }
    }

    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        if reader.read_line(&mut line_buf)? == 0 {
            break;
        }
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_searched = matches!(state, State::Searched { .. });
        match parse_command(trimmed, is_searched) {
            Ok(cmd) => {
                match dispatch_command(
                    cmd,
                    &mut state,
                    store,
                    registry,
                    config,
                    write_options.clone(),
                    &last_path,
                    &mut reader,
                    output,
                    err_out,
                ) {
                    Ok(cont) => {
                        if !cont {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = writeln!(err_out, "Error: {e}");
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(err_out, "{e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    #[test]
    fn test_runner_stream_quit() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        let config = Config::default();
        let write_opts = WriteOptions::default();

        let input = Cursor::new(b"q\n");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

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
    }

    #[test]
    fn test_interactive_edit_mode_esc_behavior() {
        let state = Arc::new(Mutex::new(State::new()));
        let current_line = Arc::new(Mutex::new(String::new()));
        let mut edit_mode = InteractiveEditMode::new(
            Emacs::default(),
            current_line.clone(),
            state.clone(),
        );

        // In Init state with empty line -> ReedlineEvent::None
        assert_eq!(
            edit_mode.handle_reedline_event(ReedlineEvent::Esc),
            ReedlineEvent::None
        );

        // In Init state with non-empty line -> Edit(Clear)
        *current_line.lock().unwrap() = "some input".to_string();
        assert_eq!(
            edit_mode.handle_reedline_event(ReedlineEvent::Esc),
            ReedlineEvent::Edit(vec![EditCommand::Clear])
        );

        // In Searched state with non-empty line -> Edit(Clear)
        state.lock().unwrap().to_searched(
            "query".to_string(),
            Some("cid".to_string()),
            0,
            Some(5),
            false,
        );
        assert_eq!(
            edit_mode.handle_reedline_event(ReedlineEvent::Esc),
            ReedlineEvent::Edit(vec![EditCommand::Clear])
        );

        // In Searched state with empty line -> ExecuteHostCommand("q")
        *current_line.lock().unwrap() = "".to_string();
        assert_eq!(
            edit_mode.handle_reedline_event(ReedlineEvent::Esc),
            ReedlineEvent::ExecuteHostCommand("q".to_string())
        );
    }

    #[test]
    fn test_keybindings_right_key_until_found() {
        let kb = build_keybindings();
        let event =
            kb.find_binding(KeyModifiers::NONE, KeyCode::Right).unwrap();
        assert_eq!(
            event,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::HistoryHintComplete,
                ReedlineEvent::MenuRight,
                ReedlineEvent::Right,
            ])
        );
    }

    #[test]
    fn test_interactive_edit_mode_tab_no_matches() {
        let state = Arc::new(Mutex::new(State::new()));
        let last_path = Arc::new(Mutex::new(".".to_string()));
        let current_line = Arc::new(Mutex::new("s foo".to_string()));
        let hinter =
            Arc::new(Mutex::new(InteractiveHinter::with_current_line(
                state.clone(),
                last_path,
                current_line.clone(),
            )));
        let completer =
            Arc::new(Mutex::new(InteractiveCompleter::new(state.clone())));

        let mut edit_mode = InteractiveEditMode::with_hinter_and_completer(
            Emacs::default(),
            current_line.clone(),
            state.clone(),
            hinter.clone(),
            completer,
        );

        let tab_event = ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]);

        let event = edit_mode.handle_reedline_event(tab_event);
        assert_eq!(event, ReedlineEvent::Repaint);
        let hint_str = hinter.lock().unwrap().handle_line("s foo", 5);
        assert_eq!(hint_str, " (no matches)");

        // 矢印キーを押すとクリアされる
        let left_event = ReedlineEvent::UntilFound(vec![
            ReedlineEvent::MenuLeft,
            ReedlineEvent::Left,
        ]);
        let left_res = edit_mode.handle_reedline_event(left_event);
        assert_eq!(left_res, ReedlineEvent::Left);
        let cleared = hinter.lock().unwrap().handle_line("s foo", 4);
        assert_eq!(cleared, "");
    }
}
