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

use std::fmt;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::cli::format::{
    format_tag_result, format_untag_result, print_results, ColorWarningSink,
};
use crate::cli::interactive::state::State;
use crate::config::Config;
use crate::db::Store;
use crate::edit::{edit_with_io, QueryType, WriteOptions};
use crate::indexing::Indexer;
use crate::search::{search, SearchOptions};
use crate::tag::TagRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Search(String),
    Tag {
        search_query: String,
        edit_query: String,
    },
    Untag {
        search_query: String,
        edit_query: String,
    },
    Index(Vec<PathBuf>),
    ClearIndex {
        all: bool,
    },
    Next,
    Menu,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandParseError {
    Empty,
    UnknownCommand(String),
    DisabledInInit(char),
    DisabledInSearched(String),
    QuoteRequired(String),
    MissingArgument(String),
}

impl fmt::Display for CommandParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandParseError::Empty => Ok(()),
            CommandParseError::UnknownCommand(cmd) => {
                write!(
                    f,
                    "Unknown command '{cmd}'. Type 'm' for available commands."
                )
            }
            CommandParseError::DisabledInInit(cmd) => {
                write!(
                    f,
                    "'{cmd}' is unavailable before searching. Please search first with 's <query>'."
                )
            }
            CommandParseError::DisabledInSearched(cmd) => {
                write!(
                    f,
                    "'{cmd}' is unavailable during search. Use 'q' to return to main menu first."
                )
            }
            CommandParseError::QuoteRequired(cmd) => {
                write!(
                    f,
                    "First argument to '{cmd}' must be enclosed in double quotes: {cmd} \"<search_query>\" <edit_query>"
                )
            }
            CommandParseError::MissingArgument(arg) => {
                write!(f, "Missing required argument: {arg}")
            }
        }
    }
}

impl std::error::Error for CommandParseError {}

pub fn parse_paths(rest: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for c in rest.chars() {
        match c {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    paths.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        paths.push(PathBuf::from(current));
    }
    paths
}

pub fn parse_quoted_search_and_rest(
    rest: &str,
    cmd_name: &str,
) -> Result<(String, String), CommandParseError> {
    let trimmed = rest.trim_start();
    if !trimmed.starts_with('"') {
        return Err(CommandParseError::QuoteRequired(cmd_name.to_string()));
    }
    let after_open = &trimmed[1..];
    let mut close_pos = None;
    let mut in_escape = false;
    for (i, c) in after_open.char_indices() {
        if in_escape {
            in_escape = false;
        } else if c == '\\' {
            in_escape = true;
        } else if c == '"' {
            let next_part = &after_open[i + 1..];
            if next_part.is_empty()
                || next_part.starts_with(char::is_whitespace)
            {
                close_pos = Some(i);
                break;
            }
        }
    }

    if let Some(idx) = close_pos {
        let raw_search_query = &after_open[..idx];
        let search_query = raw_search_query.replace("\\\"", "\"");
        if search_query.trim().is_empty() {
            return Err(CommandParseError::MissingArgument(
                "search_query".to_string(),
            ));
        }
        let remaining = after_open[idx + 1..].trim();
        if remaining.is_empty() {
            return Err(CommandParseError::MissingArgument(
                "edit_query".to_string(),
            ));
        }
        let edit_query = if remaining == "\"\"" {
            if cmd_name == "u" {
                return Err(CommandParseError::MissingArgument(
                    "edit_query".to_string(),
                ));
            }
            String::new()
        } else if remaining.starts_with('"')
            && remaining.ends_with('"')
            && remaining.len() >= 2
        {
            let inner = &remaining[1..remaining.len() - 1];
            if !inner.contains('"') || inner.contains("\\\"") {
                inner.replace("\\\"", "\"").trim().to_string()
            } else {
                remaining.to_string()
            }
        } else {
            remaining.to_string()
        };
        Ok((search_query, edit_query))
    } else {
        Err(CommandParseError::QuoteRequired(cmd_name.to_string()))
    }
}

pub fn parse_command(
    line: &str,
    is_searched: bool,
) -> Result<Command, CommandParseError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(CommandParseError::Empty);
    }
    let (head, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (trimmed, ""),
    };

    match head {
        "s" => {
            let q = if rest.starts_with('"')
                && rest.ends_with('"')
                && rest.len() >= 2
            {
                let inner = &rest[1..rest.len() - 1];
                if !inner.contains('"') {
                    inner.trim()
                } else {
                    rest.trim()
                }
            } else {
                rest.trim()
            };
            if q.is_empty() {
                Err(CommandParseError::MissingArgument(
                    "search_query".to_string(),
                ))
            } else {
                Ok(Command::Search(q.to_string()))
            }
        }
        "t" => {
            if !is_searched {
                return Err(CommandParseError::DisabledInInit('t'));
            }
            let (search_query, edit_query) =
                parse_quoted_search_and_rest(rest, "t")?;
            Ok(Command::Tag {
                search_query,
                edit_query,
            })
        }
        "u" => {
            if !is_searched {
                return Err(CommandParseError::DisabledInInit('u'));
            }
            let (search_query, edit_query) =
                parse_quoted_search_and_rest(rest, "u")?;
            Ok(Command::Untag {
                search_query,
                edit_query,
            })
        }
        "i" => {
            if is_searched {
                return Err(CommandParseError::DisabledInSearched(
                    "i".to_string(),
                ));
            }
            if rest.is_empty() {
                return Err(CommandParseError::MissingArgument(
                    "paths".to_string(),
                ));
            }
            let paths = parse_paths(rest);
            Ok(Command::Index(paths))
        }
        "n" => {
            if !is_searched {
                Err(CommandParseError::DisabledInInit('n'))
            } else {
                Ok(Command::Next)
            }
        }
        "clear" => {
            if is_searched {
                Err(CommandParseError::DisabledInSearched("clear".to_string()))
            } else {
                let all = rest == "all" || rest == "--all";
                Ok(Command::ClearIndex { all })
            }
        }
        "m" => Ok(Command::Menu),
        "q" => Ok(Command::Quit),
        other => Err(CommandParseError::UnknownCommand(other.to_string())),
    }
}

pub fn dispatch_command<R: BufRead, W: Write, E: Write>(
    cmd: Command,
    state: &mut State,
    store: &Store,
    registry: &TagRegistry,
    _config: &Config,
    write_options: WriteOptions,
    last_indexed_path: &Arc<Mutex<String>>,
    input: &mut R,
    output: &mut W,
    err_out: &mut E,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut sink = ColorWarningSink {
        writer: &mut *err_out,
    };
    match cmd {
        Command::Search(q) => {
            let opts = SearchOptions {
                n: Some(20),
                offset: Some(0),
                cid: None,
                cache: true,
                order: Vec::new(),
            };
            let resp = search(store, registry, &q, opts, &mut sink)?;
            state.to_searched(
                q.clone(),
                resp.cid.clone(),
                resp.results.len(),
                resp.total_count,
                resp.has_more,
            );
            print_results(store, registry, &resp, &q, 20, output, true);
            print_menu(output, state)?;
        }
        Command::Tag {
            search_query,
            edit_query,
        } => {
            let edit_opt = if edit_query.is_empty() {
                None
            } else {
                Some(edit_query.as_str())
            };
            let resp = match edit_with_io(
                store,
                registry,
                &search_query,
                edit_opt,
                QueryType::Tag,
                None,
                write_options,
                &mut sink,
                input,
                output,
            ) {
                Ok(r) => r,
                Err(e) => {
                    writeln!(err_out, "Error: {e}")?;
                    state.clear();
                    print_menu(output, state)?;
                    return Ok(true);
                }
            };
            writeln!(output, "{}", format_tag_result(&resp))?;

            if resp.updated > 0 || resp.deleted > 0 || resp.fs_ops > 0 {
                if let State::Searched { cid, .. } = state {
                    *cid = None;
                }
                if let Some(last_q) = state.last_query().map(|s| s.to_string())
                {
                    let opts = SearchOptions {
                        n: Some(20),
                        offset: Some(0),
                        cid: None,
                        cache: true,
                        order: Vec::new(),
                    };
                    match search(store, registry, &last_q, opts, &mut sink) {
                        Ok(re_resp) => {
                            state.to_searched(
                                last_q.clone(),
                                re_resp.cid.clone(),
                                re_resp.results.len(),
                                re_resp.total_count,
                                re_resp.has_more,
                            );
                            print_results(
                                store, registry, &re_resp, &last_q, 20, output,
                                true,
                            );
                            print_menu(output, state)?;
                        }
                        Err(e) => {
                            writeln!(err_out, "Error refreshing search: {e}")?;
                            state.clear();
                            print_menu(output, state)?;
                        }
                    }
                }
            }
        }
        Command::Untag {
            search_query,
            edit_query,
        } => {
            let edit_opt = if edit_query.is_empty() {
                None
            } else {
                Some(edit_query.as_str())
            };
            let resp = match edit_with_io(
                store,
                registry,
                &search_query,
                edit_opt,
                QueryType::Untag,
                None,
                write_options,
                &mut sink,
                input,
                output,
            ) {
                Ok(r) => r,
                Err(e) => {
                    writeln!(err_out, "Error: {e}")?;
                    state.clear();
                    print_menu(output, state)?;
                    return Ok(true);
                }
            };
            writeln!(output, "{}", format_untag_result(&resp))?;

            if resp.updated > 0 || resp.deleted > 0 || resp.fs_ops > 0 {
                if let State::Searched { cid, .. } = state {
                    *cid = None;
                }
                if let Some(last_q) = state.last_query().map(|s| s.to_string())
                {
                    let opts = SearchOptions {
                        n: Some(20),
                        offset: Some(0),
                        cid: None,
                        cache: true,
                        order: Vec::new(),
                    };
                    match search(store, registry, &last_q, opts, &mut sink) {
                        Ok(re_resp) => {
                            state.to_searched(
                                last_q.clone(),
                                re_resp.cid.clone(),
                                re_resp.results.len(),
                                re_resp.total_count,
                                re_resp.has_more,
                            );
                            print_results(
                                store, registry, &re_resp, &last_q, 20, output,
                                true,
                            );
                            print_menu(output, state)?;
                        }
                        Err(e) => {
                            writeln!(err_out, "Error refreshing search: {e}")?;
                            state.clear();
                            print_menu(output, state)?;
                        }
                    }
                }
            }
        }
        Command::Next => {
            let (query, cid, offset) = match state {
                State::Searched {
                    query, cid, offset, ..
                } => (query.clone(), cid.clone(), *offset),
                _ => return Ok(true),
            };
            let opts = SearchOptions {
                n: Some(20),
                offset: Some(offset),
                cid: cid.clone(),
                cache: true,
                order: Vec::new(),
            };
            let resp = search(store, registry, &query, opts, &mut sink)?;
            if resp.results.is_empty() {
                writeln!(output, "No more items.")?;
            } else {
                let new_offset = offset + resp.results.len();
                print_results(store, registry, &resp, &query, 20, output, true);
                state.to_searched(
                    query,
                    resp.cid.clone(),
                    new_offset,
                    resp.total_count,
                    resp.has_more,
                );
                print_menu(output, state)?;
            }
        }
        Command::Index(paths) => {
            let expanded_paths: Vec<std::path::PathBuf> = paths
                .iter()
                .map(|p| {
                    let s = p.to_string_lossy();
                    if s == "~" {
                        dirs::home_dir().unwrap_or_else(|| p.clone())
                    } else if let Some(rest) = s.strip_prefix("~/") {
                        dirs::home_dir()
                            .map(|h| h.join(rest))
                            .unwrap_or_else(|| p.clone())
                    } else {
                        p.clone()
                    }
                })
                .collect();
            let path_refs: Vec<&std::path::Path> =
                expanded_paths.iter().map(|p| p.as_path()).collect();
            let indexer = Indexer::new(store, registry);
            let n = indexer.run(&path_refs, None::<&fn(usize)>, false)?;
            writeln!(output, "Indexed {n} files.")?;
            if let Some(first) = paths.first() {
                if let Ok(mut p) = last_indexed_path.lock() {
                    *p = first.to_string_lossy().to_string();
                }
            }
        }
        Command::ClearIndex { all } => {
            let should_prompt =
                write_options.confirm != crate::config::ConfirmMode::Never;
            let proceed = if should_prompt {
                let prompt_msg = if all {
                    "Clear entire database including user tags? [y/N]: "
                } else {
                    "Clear indexed files? [y/N]: "
                };
                write!(output, "{prompt_msg}")?;
                output.flush()?;
                let mut line = String::new();
                input.read_line(&mut line)?;
                let ans = line.trim().to_lowercase();
                ans == "y" || ans == "yes"
            } else {
                true
            };
            if proceed {
                if all {
                    store.clear()?;
                    Indexer::new(store, registry).initialize_tables()?;
                    writeln!(output, "Database cleared successfully.")?;
                } else {
                    store.clear_index()?;
                    writeln!(output, "File indexes cleared successfully.")?;
                }
            } else {
                writeln!(output, "Cancelled.")?;
            }
            print_menu(output, state)?;
        }
        Command::Menu => {
            print_menu(output, state)?;
        }
        Command::Quit => {
            if matches!(*state, State::Searched { .. }) {
                state.clear();
                print_menu(output, state)?;
                return Ok(true);
            }
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn print_menu<W: Write>(out: &mut W, state: &State) -> std::io::Result<()> {
    match state {
        State::Init => {
            writeln!(out, "Welcome to ttfm interactive mode\n")?;
            writeln!(out, "Commands:")?;
            writeln!(out, "  s : Search")?;
            writeln!(out, "  i : Index directories")?;
            writeln!(out, "  m : Show menu")?;
            writeln!(out, "  q : Quit")?;
            writeln!(out, "  clear : Clear indexed files (or `clear all`)")?;
            writeln!(out)?;
            writeln!(out, "Examples:")?;
            writeln!(out, "  `s extension:rs`")?;
            writeln!(out, "  `i ~/Documents`")?;
            writeln!(out, "  `clear`")?;
        }
        State::Searched { .. } => {
            writeln!(
                out,
                "s(earch) | t(ag) | u(ntag) | n(ext) | q(uit to main menu)"
            )?;
            writeln!(out, "Examples:")?;
            writeln!(out, "  `t \"extension:rs\" project:alpha`")?;
            writeln!(out, "  `u \"extension:rs\" status:draft`")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_quotes_with_internal_quotes_and_requirements() {
        assert!(matches!(
            parse_command("t", false),
            Err(CommandParseError::DisabledInInit('t'))
        ));
        assert!(matches!(
            parse_command("t unquoted a:b", true),
            Err(CommandParseError::QuoteRequired(_))
        ));
        assert!(matches!(
            parse_command("t \"q & a\"", true),
            Err(CommandParseError::MissingArgument(_))
        ));
        assert!(matches!(
            parse_command("t \"\" project:alpha", true),
            Err(CommandParseError::MissingArgument(_))
        ));
        assert!(matches!(
            parse_command("t \"   \" project:alpha", true),
            Err(CommandParseError::MissingArgument(_))
        ));
        assert!(matches!(
            parse_command("s \"\"", true),
            Err(CommandParseError::MissingArgument(_))
        ));

        let cmd = parse_command(
            "t \"extension:rs & filename:\\\"a b\\\"\" project:core",
            true,
        )
        .unwrap();
        assert_eq!(
            cmd,
            Command::Tag {
                search_query: "extension:rs & filename:\"a b\"".to_string(),
                edit_query: "project:core".to_string(),
            }
        );

        let cmd_single_quoted =
            parse_command("t \"extension:rs\" \"project:alpha\"", true)
                .unwrap();
        assert_eq!(
            cmd_single_quoted,
            Command::Tag {
                search_query: "extension:rs".to_string(),
                edit_query: "project:alpha".to_string(),
            }
        );

        let cmd_quoted_multi = parse_command(
            "t \"extension:rs\" \"project:alpha status:done\"",
            true,
        )
        .unwrap();
        assert_eq!(
            cmd_quoted_multi,
            Command::Tag {
                search_query: "extension:rs".to_string(),
                edit_query: "project:alpha status:done".to_string(),
            }
        );

        let cmd_empty = parse_command("t \"extension:rs\" \"\"", true).unwrap();
        assert_eq!(
            cmd_empty,
            Command::Tag {
                search_query: "extension:rs".to_string(),
                edit_query: "".to_string(),
            }
        );

        assert!(matches!(
            parse_command("u \"extension:rs\" \"\"", true),
            Err(CommandParseError::MissingArgument(_))
        ));

        assert!(matches!(
            parse_command("c", true),
            Err(CommandParseError::UnknownCommand(_))
        ));

        assert!(matches!(
            parse_command("i", false),
            Err(CommandParseError::MissingArgument(_))
        ));
        let cmd_i =
            parse_command("i \"dir with space\" another/path", false).unwrap();
        assert_eq!(
            cmd_i,
            Command::Index(vec![
                PathBuf::from("dir with space"),
                PathBuf::from("another/path")
            ])
        );
    }

    #[test]
    fn test_dispatch_menu() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        let config = Config::default();
        let write_opts = WriteOptions::default();
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut state = State::new();
        let mut input = std::io::Cursor::new(b"");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

        let cont = dispatch_command(
            Command::Menu,
            &mut state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut input,
            &mut output,
            &mut err_out,
        )
        .unwrap();
        assert!(cont);
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Commands:"));
    }

    #[test]
    fn test_dispatch_quit_returns_false() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        let config = Config::default();
        let write_opts = WriteOptions::default();
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut state = State::new();
        let mut input = std::io::Cursor::new(b"");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

        let cont = dispatch_command(
            Command::Quit,
            &mut state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut input,
            &mut output,
            &mut err_out,
        )
        .unwrap();
        assert!(!cont);
    }

    #[test]
    fn test_dispatch_tag_borrowing() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        let config = Config::default();
        let write_opts = WriteOptions::default();
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut state = State::new();
        let mut input = std::io::Cursor::new(b"n\n");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

        let _ = dispatch_command(
            Command::Tag {
                search_query: "nonexistent".to_string(),
                edit_query: "tag:foo".to_string(),
            },
            &mut state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut input,
            &mut output,
            &mut err_out,
        );
    }

    #[test]
    fn test_dispatch_search_updates_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        Indexer::new(&store, &registry).initialize_tables().unwrap();
        let config = Config::default();
        let write_opts = WriteOptions::default();
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut state = State::new();
        let mut input = std::io::Cursor::new(b"");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

        let cont = dispatch_command(
            Command::Search("*:*".to_string()),
            &mut state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut input,
            &mut output,
            &mut err_out,
        )
        .unwrap();
        assert!(cont);
        assert!(matches!(state, State::Searched { .. }));
        assert_eq!(state.last_query(), Some("*:*"));
    }

    #[test]
    fn test_parse_clear_command() {
        assert_eq!(
            parse_command("clear", false).unwrap(),
            Command::ClearIndex { all: false }
        );
        assert_eq!(
            parse_command("clear all", false).unwrap(),
            Command::ClearIndex { all: true }
        );
        assert_eq!(
            parse_command("clear --all", false).unwrap(),
            Command::ClearIndex { all: true }
        );
        assert!(matches!(
            parse_command("clear", true),
            Err(CommandParseError::DisabledInSearched(ref s)) if s == "clear"
        ));
    }

    #[test]
    fn test_dispatch_clear_index_prompt() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        Indexer::new(&store, &registry).initialize_tables().unwrap();
        let config = Config::default();
        let write_opts = WriteOptions::interactive();
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut state = State::new();
        let mut input_cancel = std::io::Cursor::new(b"n\n");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

        let cont = dispatch_command(
            Command::ClearIndex { all: false },
            &mut state,
            &store,
            &registry,
            &config,
            write_opts.clone(),
            &path,
            &mut input_cancel,
            &mut output,
            &mut err_out,
        )
        .unwrap();
        assert!(cont);
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("Cancelled."));

        let mut input_yes = std::io::Cursor::new(b"y\n");
        let mut output_yes = Vec::new();
        let cont = dispatch_command(
            Command::ClearIndex { all: false },
            &mut state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut input_yes,
            &mut output_yes,
            &mut err_out,
        )
        .unwrap();
        assert!(cont);
        let out_yes_str = String::from_utf8(output_yes).unwrap();
        assert!(out_yes_str.contains("File indexes cleared successfully."));
    }

    #[test]
    fn test_print_menu_init_and_searched() {
        let mut out = Vec::new();
        let state = State::new();
        print_menu(&mut out, &state).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Welcome to ttfm interactive mode"));
        let q_pos = s.find("q : Quit").unwrap();
        let clear_pos = s.find("clear : Clear indexed files").unwrap();
        assert!(q_pos < clear_pos);

        let mut out_searched = Vec::new();
        let mut searched_state = State::new();
        searched_state.to_searched(
            "foo".to_string(),
            Some("cid".to_string()),
            0,
            Some(10),
            false,
        );
        print_menu(&mut out_searched, &searched_state).unwrap();
        let s_searched = String::from_utf8(out_searched).unwrap();
        assert!(s_searched.contains(
            "s(earch) | t(ag) | u(ntag) | n(ext) | q(uit to main menu)"
        ));
    }

    #[test]
    fn test_dispatch_quit_hierarchical() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        let config = Config::default();
        let write_opts = WriteOptions::default();
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut input = std::io::Cursor::new(b"");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

        let mut searched_state = State::new();
        searched_state.to_searched(
            "foo".to_string(),
            Some("cid".to_string()),
            0,
            Some(10),
            false,
        );

        let cont = dispatch_command(
            Command::Quit,
            &mut searched_state,
            &store,
            &registry,
            &config,
            write_opts.clone(),
            &path,
            &mut input,
            &mut output,
            &mut err_out,
        )
        .unwrap();
        assert!(cont);
        assert!(matches!(searched_state, State::Init));

        let cont_init = dispatch_command(
            Command::Quit,
            &mut searched_state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut input,
            &mut output,
            &mut err_out,
        )
        .unwrap();
        assert!(!cont_init);
    }

    #[test]
    fn test_parse_search_command_quote_stripping() {
        assert_eq!(
            parse_command("s \"extension:rs\"", false).unwrap(),
            Command::Search("extension:rs".to_string())
        );
        assert_eq!(
            parse_command("s \"a:1\" | \"b:2\"", false).unwrap(),
            Command::Search("\"a:1\" | \"b:2\"".to_string())
        );
    }

    #[test]
    fn test_dispatch_clear_index_confirm_never() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        Indexer::new(&store, &registry).initialize_tables().unwrap();
        let config = Config::default();
        let mut write_opts = WriteOptions::default();
        write_opts.confirm = crate::config::ConfirmMode::Never;
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut state = State::new();
        let mut input = std::io::Cursor::new(b"");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

        let cont = dispatch_command(
            Command::ClearIndex { all: false },
            &mut state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut input,
            &mut output,
            &mut err_out,
        )
        .unwrap();
        assert!(cont);
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("File indexes cleared successfully."));
        assert!(!out_str.contains("[y/N]"));
    }
}
