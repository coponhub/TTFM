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

use crate::cli::interactive::state::State;
use reedline::{Completer, Hinter, History, Span, Suggestion};
use std::sync::{Arc, Mutex};

pub struct InteractiveHinter {
    state: Arc<Mutex<State>>,
    last_indexed_path: Arc<Mutex<String>>,
    current_line: Arc<Mutex<String>>,
    current_hint: String,
    edited: bool,
    transient_message: Arc<Mutex<Option<(String, std::time::Instant)>>>,
}

impl InteractiveHinter {
    pub fn new(
        state: Arc<Mutex<State>>,
        last_indexed_path: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            state,
            last_indexed_path,
            current_line: Arc::new(Mutex::new(String::new())),
            current_hint: String::new(),
            edited: false,
            transient_message: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_current_line(
        state: Arc<Mutex<State>>,
        last_indexed_path: Arc<Mutex<String>>,
        current_line: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            state,
            last_indexed_path,
            current_line,
            current_hint: String::new(),
            edited: false,
            transient_message: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_transient_message(&self, msg: &str) {
        if let Ok(mut tm) = self.transient_message.lock() {
            *tm = Some((msg.to_string(), std::time::Instant::now()));
        }
    }

    pub fn clear_transient_message(&self) {
        if let Ok(mut tm) = self.transient_message.lock() {
            *tm = None;
        }
    }

    pub fn transient_message_handle(
        &self,
    ) -> Arc<Mutex<Option<(String, std::time::Instant)>>> {
        self.transient_message.clone()
    }

    pub fn current_line(&self) -> Arc<Mutex<String>> {
        self.current_line.clone()
    }

    pub fn reset(&mut self) {
        self.edited = false;
        self.current_hint.clear();
        self.clear_transient_message();
        if let Ok(mut cur) = self.current_line.lock() {
            cur.clear();
        }
    }

    pub fn handle_line(&mut self, line: &str, pos: usize) -> String {
        let history = reedline::FileBackedHistory::default();
        self.handle(line, pos, &history, false, "")
    }
}

impl Hinter for InteractiveHinter {
    fn handle(
        &mut self,
        line: &str,
        _pos: usize,
        _history: &dyn History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        if let Ok(mut tm) = self.transient_message.lock() {
            if let Some((msg, instant)) = tm.as_ref() {
                if instant.elapsed() < std::time::Duration::from_millis(1500) {
                    return if use_ansi_coloring {
                        format!("\x1b[2m{msg}\x1b[0m")
                    } else {
                        msg.clone()
                    };
                } else {
                    *tm = None;
                }
            }
        }

        if let Ok(mut cur) = self.current_line.lock() {
            *cur = line.to_string();
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            self.edited = false;
            self.current_hint.clear();
            return String::new();
        }

        let is_prefix = matches!(
            trimmed,
            "s" | "s " | "t" | "t " | "u" | "u " | "i" | "i "
        );
        if !is_prefix {
            self.edited = true;
        }

        self.current_hint.clear();
        if self.edited {
            return String::new();
        }

        if trimmed == "s" || trimmed == "s " {
            if let Ok(st) = self.state.lock() {
                let q = st.last_query().unwrap_or("*:*");
                self.current_hint = if trimmed == "s" {
                    format!(" {q}")
                } else {
                    q.to_string()
                };
            }
        } else if trimmed == "t"
            || trimmed == "t "
            || trimmed == "u"
            || trimmed == "u "
        {
            if let Ok(st) = self.state.lock() {
                if let Some(q) = st.last_query() {
                    let escaped = q.replace('"', "\\\"");
                    self.current_hint = if trimmed == "t" || trimmed == "u" {
                        format!(" \"{escaped}\" ")
                    } else {
                        format!("\"{escaped}\" ")
                    };
                }
            }
        } else if trimmed == "i" || trimmed == "i " {
            if let Ok(st) = self.state.lock() {
                if matches!(*st, State::Init) {
                    if let Ok(p) = self.last_indexed_path.lock() {
                        let hint_str = if p.contains(char::is_whitespace) {
                            format!("\"{p}\"")
                        } else {
                            p.clone()
                        };
                        self.current_hint = if trimmed == "i" {
                            format!(" {hint_str}")
                        } else {
                            hint_str
                        };
                    }
                }
            }
        }
        if use_ansi_coloring && !self.current_hint.is_empty() {
            format!("\x1b[2m{}\x1b[0m", self.current_hint)
        } else {
            self.current_hint.clone()
        }
    }

    fn complete_hint(&self) -> String {
        self.current_hint.clone()
    }

    fn next_hint_token(&self) -> String {
        self.current_hint
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    }
}

fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" {
        dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("~"))
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest)
        } else {
            std::path::PathBuf::from(path)
        }
    } else {
        std::path::PathBuf::from(path)
    }
}

pub struct InteractiveCompleter {
    state: Arc<Mutex<State>>,
}

impl InteractiveCompleter {
    pub fn new(state: Arc<Mutex<State>>) -> Self {
        Self { state }
    }
}

impl Completer for InteractiveCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        if let Ok(st) = self.state.lock() {
            if !matches!(*st, State::Init) {
                return vec![];
            }
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("i ") && pos >= 2 {
            let prefix_offset = line.len() - trimmed.len() + 2;
            if pos >= prefix_offset {
                let sub = &line[prefix_offset..pos];
                let last_word_offset = sub
                    .char_indices()
                    .rfind(|(_, c)| c.is_whitespace())
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                let word = &sub[last_word_offset..];
                if word == "~" {
                    let total_offset = prefix_offset + last_word_offset;
                    return vec![Suggestion {
                        value: "~/".to_string(),
                        span: Span::new(total_offset, total_offset + 1),
                        append_whitespace: false,
                        ..Default::default()
                    }];
                }

                let (dir_path, file_prefix) = match word.rfind('/') {
                    Some(idx) => (&word[..=idx], &word[idx + 1..]),
                    None => ("", word),
                };
                let read_dir_path = if dir_path.is_empty() {
                    std::path::PathBuf::from(".")
                } else {
                    expand_tilde(dir_path)
                };
                let mut suggestions = Vec::new();
                if let Ok(entries) = std::fs::read_dir(read_dir_path) {
                    for entry in entries.flatten() {
                        let name =
                            entry.file_name().to_string_lossy().to_string();
                        if name.starts_with(file_prefix) {
                            let is_dir = entry
                                .file_type()
                                .map(|t| t.is_dir())
                                .unwrap_or(false);
                            let mut val = format!("{}{}", dir_path, name);
                            if is_dir {
                                val.push('/');
                            }
                            let total_offset = prefix_offset + last_word_offset;
                            suggestions.push(Suggestion {
                                value: val,
                                span: Span::new(
                                    total_offset,
                                    total_offset + word.len(),
                                ),
                                append_whitespace: !is_dir,
                                ..Default::default()
                            });
                        }
                    }
                }
                suggestions.sort_by(|a, b| a.value.cmp(&b.value));
                return suggestions;
            }
        }
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hinter_quoted_output_for_tags() {
        let state = Arc::new(Mutex::new(State::new()));
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut hinter = InteractiveHinter::new(state.clone(), path);

        assert_eq!(hinter.handle_line("t ", 2), "");
        state.lock().unwrap().to_searched(
            "ext:rs & fn:\"a b\"".to_string(),
            None,
            10,
            Some(10),
            false,
        );
        assert_eq!(hinter.handle_line("t ", 2), "\"ext:rs & fn:\\\"a b\\\"\" ");
        assert_eq!(hinter.complete_hint(), "\"ext:rs & fn:\\\"a b\\\"\" ");
        assert_eq!(hinter.next_hint_token(), "\"ext:rs");
    }

    #[test]
    fn test_hinter_edited_clears_hint() {
        let state = Arc::new(Mutex::new(State::new()));
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut hinter = InteractiveHinter::new(state.clone(), path);

        state.lock().unwrap().to_searched(
            "ext:rs".to_string(),
            None,
            10,
            Some(10),
            false,
        );
        assert_eq!(hinter.handle_line("s ", 2), "ext:rs");

        // 文字を入力すると edited に設定される
        assert_eq!(hinter.handle_line("s a", 3), "");

        // プレフィックスまで戻しても edited のためヒントは非表示を維持
        assert_eq!(hinter.handle_line("s ", 2), "");
        assert_eq!(hinter.complete_hint(), "");

        // 空行でリセットされる
        assert_eq!(hinter.handle_line("", 0), "");
        assert_eq!(hinter.handle_line("s ", 2), "ext:rs");
    }

    #[test]
    fn test_hinter_quoted_path_with_spaces() {
        let state = Arc::new(Mutex::new(State::new()));
        let path = Arc::new(Mutex::new("dir with spaces".to_string()));
        let mut hinter = InteractiveHinter::new(state.clone(), path);
        assert_eq!(hinter.handle_line("i ", 2), "\"dir with spaces\"");
    }

    #[test]
    fn test_completer_span_offset_calculation() {
        let state = Arc::new(Mutex::new(State::new()));
        let mut completer = InteractiveCompleter::new(state);
        let suggestions = completer.complete("i src/m", 7);
        assert!(!suggestions.is_empty());
        assert!(suggestions.iter().any(|s| s.value.starts_with("src/m")));
        for s in &suggestions {
            assert_eq!(s.span, Span::new(2, 7));
        }
    }

    #[test]
    fn test_completer_tilde_expansion() {
        let state = Arc::new(Mutex::new(State::new()));
        let mut completer = InteractiveCompleter::new(state);
        let sugg = completer.complete("i ~", 3);
        assert!(sugg.iter().any(|s| s.value == "~/"));
        let sugg_slash = completer.complete("i ~/", 4);
        assert!(!sugg_slash.is_empty());
    }

    #[test]
    fn test_hinter_initial_default_query() {
        let state = Arc::new(Mutex::new(State::new()));
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut hinter = InteractiveHinter::new(state, path);
        assert_eq!(hinter.handle_line("s ", 2), "*:*");
        assert_eq!(hinter.handle_line("s", 1), " *:*");
    }

    #[test]
    fn test_hinter_tracks_current_line() {
        let state = Arc::new(Mutex::new(State::new()));
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut hinter = InteractiveHinter::new(state, path);
        assert_eq!(*hinter.current_line().lock().unwrap(), "");
        hinter.handle_line("s foo", 5);
        assert_eq!(*hinter.current_line().lock().unwrap(), "s foo");
        hinter.reset();
        assert_eq!(*hinter.current_line().lock().unwrap(), "");
    }

    #[test]
    fn test_hinter_ansi_coloring_applies_dimmed_style() {
        let state = Arc::new(Mutex::new(State::new()));
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut hinter = InteractiveHinter::new(state.clone(), path);
        state.lock().unwrap().to_searched(
            "ext:rs".to_string(),
            None,
            10,
            Some(10),
            false,
        );
        let history = reedline::FileBackedHistory::default();
        let uncolored = hinter.handle("s ", 2, &history, false, "");
        assert_eq!(uncolored, "ext:rs");

        let colored = hinter.handle("s ", 2, &history, true, "");
        assert_eq!(colored, "\x1b[2mext:rs\x1b[0m");

        assert_eq!(hinter.complete_hint(), "ext:rs");
    }

    #[test]
    fn test_hinter_transient_message() {
        let state = Arc::new(Mutex::new(State::new()));
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut hinter = InteractiveHinter::new(state, path);

        hinter.set_transient_message(" (no matches)");
        let history = reedline::FileBackedHistory::default();
        let colored = hinter.handle("s foo", 5, &history, true, "");
        assert_eq!(colored, "\x1b[2m (no matches)\x1b[0m");

        // clear_transient_message で消える
        hinter.clear_transient_message();
        let cleared = hinter.handle("s foo", 5, &history, true, "");
        assert_eq!(cleared, "");
    }
}
