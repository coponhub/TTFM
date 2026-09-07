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
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::cli::format::{
    format_tag_result, format_untag_result, print_results_with_options,
    ColorWarningSink, FormatOptions,
};
use crate::cli::interactive::state::State;
use crate::cli::progress::MultiStageProgressView;
use crate::config::Config;
use crate::db::Store;
use crate::edit::{edit_with_io, QueryType, WriteOptions};
use crate::indexing::Indexer;
use crate::search::{search, SearchOptions};
use crate::tag::TagRegistry;

pub const INTERACTIVE_PAGE_SIZE: usize = 20;

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
    Prev,
    NextCols,
    PrevCols,
    Help,
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
        "p" | "prev" => {
            if !is_searched {
                Err(CommandParseError::DisabledInInit('p'))
            } else {
                Ok(Command::Prev)
            }
        }
        "h" | "help" => Ok(Command::Help),
        ">" => {
            if !is_searched {
                Err(CommandParseError::DisabledInInit('>'))
            } else {
                Ok(Command::NextCols)
            }
        }
        "<" => {
            if !is_searched {
                Err(CommandParseError::DisabledInInit('<'))
            } else {
                Ok(Command::PrevCols)
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

fn render_response<W: Write>(
    resp: &crate::response::SearchResponse,
    query: &str,
    state: &mut State,
    store: &Store,
    registry: &TagRegistry,
    output: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let cur_col_offset = state.current_col_offset();
    let cur_offset = match state {
        State::Searched { offset, .. } => Some(*offset),
        _ => None,
    };
    let paging = print_results_with_options(
        store,
        registry,
        resp,
        query,
        INTERACTIVE_PAGE_SIZE,
        output,
        FormatOptions {
            is_interactive: true,
            wide: false,
            col_offset: cur_col_offset,
            current_offset: cur_offset,
        },
    );
    state.set_next_col_offset(if paging.has_next {
        Some(paging.next_col_offset)
    } else {
        None
    });
    print_menu(output, state)?;
    Ok(())
}

fn render_current_page<W: Write, E: Write>(
    state: &mut State,
    store: &Store,
    registry: &TagRegistry,
    output: &mut W,
    err_out: &mut E,
) -> Result<(), Box<dyn std::error::Error>> {
    let (query, cid, page_start) = match state {
        State::Searched {
            query,
            cid,
            page_start,
            ..
        } => (query.clone(), cid.clone(), *page_start),
        _ => return Ok(()),
    };
    let opts = SearchOptions {
        n: Some(INTERACTIVE_PAGE_SIZE),
        offset: Some(page_start),
        cid,
        cache: true,
        order: Vec::new(),
    };
    let mut sink = ColorWarningSink { writer: err_out };
    let resp = search(store, registry, &query, opts, &mut sink)?;
    render_response(&resp, &query, state, store, registry, output)
}

fn execute_search_and_render<W: Write, E: Write>(
    query: &str,
    page_start: usize,
    cid: Option<String>,
    state: &mut State,
    store: &Store,
    registry: &TagRegistry,
    output: &mut W,
    err_out: &mut E,
) -> Result<(), Box<dyn std::error::Error>> {
    let opts = SearchOptions {
        n: Some(INTERACTIVE_PAGE_SIZE),
        offset: Some(page_start),
        cid: cid.clone(),
        cache: true,
        order: Vec::new(),
    };
    let mut sink = ColorWarningSink { writer: err_out };
    let resp = search(store, registry, query, opts, &mut sink)?;
    if page_start > 0 && resp.results.is_empty() {
        writeln!(output, "No more items.")?;
        if let State::Searched {
            has_more,
            next_col_offset,
            ..
        } = state
        {
            *has_more = false;
            *next_col_offset = None;
        }
        print_menu(output, state)?;
        return Ok(());
    }
    state.to_searched_at_page(
        query.to_string(),
        resp.cid.clone(),
        page_start,
        resp.results.len(),
        resp.total_count,
        resp.has_more,
    );
    render_response(&resp, query, state, store, registry, output)
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
            execute_search_and_render(
                &q, 0, None, state, store, registry, output, err_out,
            )?;
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
                if let Err(e) = execute_search_and_render(
                    &search_query,
                    0,
                    None,
                    state,
                    store,
                    registry,
                    output,
                    err_out,
                ) {
                    writeln!(err_out, "Error refreshing search: {e}")?;
                    state.clear();
                    print_menu(output, state)?;
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
                if let Err(e) = execute_search_and_render(
                    &search_query,
                    0,
                    None,
                    state,
                    store,
                    registry,
                    output,
                    err_out,
                ) {
                    writeln!(err_out, "Error refreshing search: {e}")?;
                    state.clear();
                    print_menu(output, state)?;
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
            execute_search_and_render(
                &query, offset, cid, state, store, registry, output, err_out,
            )?;
        }
        Command::Prev => {
            let (query, cid, page_start) = match state {
                State::Searched {
                    query,
                    cid,
                    page_start,
                    ..
                } => (query.clone(), cid.clone(), *page_start),
                _ => return Ok(true),
            };
            if page_start == 0 {
                writeln!(output, "Already at first page.")?;
                print_menu(output, state)?;
            } else {
                let prev_offset =
                    page_start.saturating_sub(INTERACTIVE_PAGE_SIZE);
                execute_search_and_render(
                    &query,
                    prev_offset,
                    cid,
                    state,
                    store,
                    registry,
                    output,
                    err_out,
                )?;
            }
        }
        Command::NextCols => {
            let next_opt = match state {
                State::Searched {
                    next_col_offset, ..
                } => *next_col_offset,
                _ => None,
            };
            if let Some(next) = next_opt {
                state.push_col_offset(next);
                render_current_page(state, store, registry, output, err_out)?;
            } else {
                writeln!(output, "No more columns.")?;
                print_menu(output, state)?;
            }
        }
        Command::PrevCols => {
            if state.can_prev_cols() {
                state.pop_col_offset();
                render_current_page(state, store, registry, output, err_out)?;
            } else {
                writeln!(output, "Already at first columns.")?;
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
            let is_interactive = std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal();
            let view = MultiStageProgressView::for_stdout(is_interactive);
            let indexer = Indexer::new(store, registry);
            let n = indexer.run(
                &path_refs,
                Some(&|p| view.handle_progress(p)),
                false,
            )?;
            view.finish();
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
        Command::Help => {
            print_help(output)?;
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

const HELP_TEXT: &str = "\
TTFM Interactive Mode Help

Commands:
  s <query>                 \x1b[2m# Search with TTQL query\x1b[0m          p \x1b[2m# Previous page of search results\x1b[0m
  t \"<search_query>\" <edit> \x1b[2m# Add or update tags (empty store)\x1b[0m n \x1b[2m# Next page of search results\x1b[0m
  u \"<search_query>\" <tag>  \x1b[2m# Remove tags from matched items\x1b[0m  < \x1b[2m# Show left columns (horizontal scroll)\x1b[0m
  i <paths>...              \x1b[2m# Index directories (Init only)\x1b[0m   > \x1b[2m# Show right columns (horizontal scroll)\x1b[0m
  clear [all]               \x1b[2m# Clear file index or database\x1b[0m    h \x1b[2m# Show this help\x1b[0m / m \x1b[2m# Show menu\x1b[0m
  q                         \x1b[2m# Clear search context / Quit\x1b[0m

Syntax & Examples:
Basic Tags (type:label):                                              Show types/labels/tags definitions & Storing:
  s extension:rs \x1b[2m# Search items by tag\x1b[0m                                  s extension: \x1b[2m# List distinct labels of type\x1b[0m
  t \"extension:rs\" project:alpha \x1b[2m# Add tag to matched items\x1b[0m             s label: \x1b[2m# List all labels across indexed items\x1b[0m
  u \"extension:rs\" status:draft \x1b[2m# Remove tag from matched items\x1b[0m         s type:* \x1b[2m# List all type definitions\x1b[0m
                                                                        s type: \x1b[2m# List distinct types across indexed items\x1b[0m
Set Operations (&, |, -):                                               s tag: \x1b[2m# List all tags across indexed items\x1b[0m
  s extension:rs & project:ttfm \x1b[2m# Intersection (AND)\x1b[0m                    u \"extension:rs\" project: \x1b[2m# Remove all tags of type\x1b[0m
  s extension:rs | extension:ts \x1b[2m# Union (OR)\x1b[0m                            t \"extension:rs\" \"\" \x1b[2m# Store volatile search results\x1b[0m
  s extension:rs - status:draft \x1b[2m# Difference (DIFF / EXCLUDE)\x1b[0m
  t \"extension:rs & size:>1MB\" project:large \x1b[2m# Tag filtered set\x1b[0m       Aggregations (count, sum, avg, ...):
                                                                        s count() \x1b[2m# Total count of matched items\x1b[0m
Glob Patterns & Captures (*, {n}):                                      s sum(size:) \x1b[2m# Sum of numeric projection\x1b[0m
  s filename:*.rs \x1b[2m# Wildcard pattern search\x1b[0m                             s count(extension:) \x1b[2m# Count distinct labels of type\x1b[0m
  t \"filename:*_draft.txt\" filename:{1}.txt \x1b[2m# Rename files\x1b[0m
  t \"name:*.old\" name:{1} \x1b[2m# Rename item display name\x1b[0m                  Nesting (&:):
  t \"path:*.md\" path:/new/dir/{1} \x1b[2m# Move files to new directory\x1b[0m         s project: &: extension: \x1b[2m# Multi-key compound grouping\x1b[0m
                                                                        s path:^/mnt/*/: &: extension: \x1b[2m# Pattern grouping\x1b[0m
Comparisons & Ranges:
  s size:>100MB \x1b[2m# Stuck label comparison\x1b[0m                              Nesting with Aggregation (Combined):
  s 10MB :< size: :< 1GB \x1b[2m# Chained range comparison\x1b[0m                     s parentdir: &: count() \x1b[2m# Group and aggregate\x1b[0m
  s mtime:today \x1b[2m# Relative date search\x1b[0m                                  s path:^/mnt/*/: &: sum(size:) \x1b[2m# Pattern group with sum\x1b[0m
  t \"size:>1GB\" tag:huge \x1b[2m# Tag comparison match\x1b[0m                         s parentdir: &: count() :> 10 \x1b[2m# Filter groups\x1b[0m
                                                                        t \"parentdir: &: count() :> 10\" status:busy \x1b[2m# Tag groups\x1b[0m
Eval (q()):
  s q(milestone:m1) & extension:rs \x1b[2m# Expand definition tags\x1b[0m
  t \"q(milestone:m1)\" status:ready \x1b[2m# Tag from definition\x1b[0m
";

pub fn print_help<W: Write>(out: &mut W) -> std::io::Result<()> {
    if console::colors_enabled() {
        write!(out, "{HELP_TEXT}")
    } else {
        write!(out, "{}", console::strip_ansi_codes(HELP_TEXT))
    }
}

pub fn print_menu<W: Write>(out: &mut W, state: &State) -> std::io::Result<()> {
    match state {
        State::Init => {
            writeln!(out, "Welcome to ttfm interactive mode\n")?;
            writeln!(out, "Commands:")?;
            writeln!(out, "  s : Search")?;
            writeln!(out, "  i : Index directories")?;
            writeln!(out, "  h : Help")?;
            writeln!(out, "  m : Show menu")?;
            writeln!(out, "  q : Quit")?;
            writeln!(out, "  clear : Clear indexed files (or `clear all`)")?;
        }
        State::Searched {
            page_start,
            has_more,
            next_col_offset,
            col_offsets,
            ..
        } => {
            let mut items = vec![
                "s \x1b[2msearch\x1b[0m".to_string(),
                "t \x1b[2mtag\x1b[0m".to_string(),
                "u \x1b[2muntag\x1b[0m".to_string(),
            ];
            if *page_start > 0 {
                items.push("p \x1b[2mprev\x1b[0m".to_string());
            }
            if *has_more {
                items.push("n \x1b[2mnext\x1b[0m".to_string());
            }
            if col_offsets.len() > 1 {
                items.push("< \x1b[2mshow left\x1b[0m".to_string());
            }
            if next_col_offset.is_some() {
                items.push("> \x1b[2mshow right\x1b[0m".to_string());
            }
            items.push("h \x1b[2mhelp\x1b[0m".to_string());
            items.push("q \x1b[2mquit to main menu\x1b[0m".to_string());
            writeln!(out, "{}", items.join(" | "))?;
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
    fn test_dispatch_tag_and_untag_refreshes_with_edit_search_query() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        Indexer::new(&store, &registry).initialize_tables().unwrap();

        let file_path = dir.path().join("test_sample.txt");
        std::fs::write(&file_path, "sample content").unwrap();
        Indexer::new(&store, &registry)
            .run_single(dir.path(), None, false)
            .unwrap();

        let config = Config::default();
        let mut write_opts = WriteOptions::default();
        write_opts.confirm = crate::config::ConfirmMode::Never;
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut state = State::new();

        // 1. Initial search
        let mut out1 = Vec::new();
        let mut err1 = Vec::new();
        dispatch_command(
            Command::Search("extension:txt".to_string()),
            &mut state,
            &store,
            &registry,
            &config,
            write_opts.clone(),
            &path,
            &mut std::io::Cursor::new(b""),
            &mut out1,
            &mut err1,
        )
        .unwrap();
        assert_eq!(state.last_query(), Some("extension:txt"));

        // 2. Tag with a different search_query
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        dispatch_command(
            Command::Tag {
                search_query: "extension:txt & size:>0".to_string(),
                edit_query: "project:alpha".to_string(),
            },
            &mut state,
            &store,
            &registry,
            &config,
            write_opts.clone(),
            &path,
            &mut std::io::Cursor::new(b""),
            &mut out2,
            &mut err2,
        )
        .unwrap();
        // After tag execution, state query must be the tag command's search_query
        assert_eq!(state.last_query(), Some("extension:txt & size:>0"));

        // 3. Untag with another search_query
        let mut out3 = Vec::new();
        let mut err3 = Vec::new();
        dispatch_command(
            Command::Untag {
                search_query: "project:alpha".to_string(),
                edit_query: "project:alpha".to_string(),
            },
            &mut state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut std::io::Cursor::new(b""),
            &mut out3,
            &mut err3,
        )
        .unwrap();
        // After untag execution, state query must be the untag command's search_query
        assert_eq!(state.last_query(), Some("project:alpha"));
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
    fn test_parse_prev_and_help() {
        assert!(matches!(
            parse_command("p", false),
            Err(CommandParseError::DisabledInInit('p'))
        ));
        assert_eq!(parse_command("p", true).unwrap(), Command::Prev);
        assert_eq!(parse_command("prev", true).unwrap(), Command::Prev);
        assert_eq!(parse_command("h", false).unwrap(), Command::Help);
        assert_eq!(parse_command("help", true).unwrap(), Command::Help);
    }

    #[test]
    fn test_print_menu_init_and_searched() {
        let mut out = Vec::new();
        let state = State::new();
        print_menu(&mut out, &state).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Welcome to ttfm interactive mode"));
        assert!(!s.contains("Examples:"));
        assert!(s.contains("h : Help"));
        let q_pos = s.find("q : Quit").unwrap();
        let clear_pos = s.find("clear : Clear indexed files").unwrap();
        assert!(q_pos < clear_pos);

        let mut out_searched = Vec::new();
        let mut searched_state = State::new();
        searched_state.to_searched("foo".to_string(), None, 0, Some(10), false);
        print_menu(&mut out_searched, &searched_state).unwrap();
        let s_searched = String::from_utf8(out_searched).unwrap();
        assert!(!s_searched.contains("Examples:"));
        assert!(s_searched.contains(
            "s \x1b[2msearch\x1b[0m | t \x1b[2mtag\x1b[0m | u \x1b[2muntag\x1b[0m"
        ));
    }

    #[test]
    fn test_print_menu_searched_dynamic_hints() {
        let mut out_first = Vec::new();
        let mut st_first = State::new();
        st_first.to_searched_at_page(
            "foo".to_string(),
            None,
            0,
            20,
            None,
            true,
        );
        st_first.set_next_col_offset(Some(3));
        print_menu(&mut out_first, &st_first).unwrap();
        let s_first = String::from_utf8(out_first).unwrap();
        assert!(s_first.contains("n \x1b[2mnext\x1b[0m"));
        assert!(!s_first.contains("p \x1b[2mprev\x1b[0m"));
        assert!(s_first.contains("> \x1b[2mshow right\x1b[0m"));
        assert!(!s_first.contains("< \x1b[2mshow left\x1b[0m"));

        let mut out_second = Vec::new();
        let mut st_second = State::new();
        st_second.to_searched_at_page(
            "foo".to_string(),
            None,
            20,
            10,
            None,
            false,
        );
        st_second.push_col_offset(3);
        print_menu(&mut out_second, &st_second).unwrap();
        let s_second = String::from_utf8(out_second).unwrap();
        assert!(!s_second.contains("n \x1b[2mnext\x1b[0m"));
        assert!(s_second.contains("p \x1b[2mprev\x1b[0m"));
        assert!(!s_second.contains("> \x1b[2mshow right\x1b[0m"));
        assert!(s_second.contains("< \x1b[2mshow left\x1b[0m"));
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

    #[test]
    fn test_parse_and_dispatch_column_paging() {
        assert!(matches!(
            parse_command(">", false),
            Err(CommandParseError::DisabledInInit('>'))
        ));
        assert!(matches!(
            parse_command("<", false),
            Err(CommandParseError::DisabledInInit('<'))
        ));
        assert_eq!(parse_command(">", true).unwrap(), Command::NextCols);
        assert_eq!(parse_command("<", true).unwrap(), Command::PrevCols);

        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        let config = Config::default();
        let write_opts = WriteOptions::default();
        let path = Arc::new(Mutex::new(".".to_string()));
        let mut state = State::new();
        state.to_searched("dummy".to_string(), None, 0, Some(0), false);

        // next_col_offset is None initially
        let mut out_next = Vec::new();
        let mut err_next = Vec::new();
        dispatch_command(
            Command::NextCols,
            &mut state,
            &store,
            &registry,
            &config,
            write_opts.clone(),
            &path,
            &mut std::io::Cursor::new(b""),
            &mut out_next,
            &mut err_next,
        )
        .unwrap();
        let s_next = String::from_utf8(out_next).unwrap();
        assert!(s_next.contains("No more columns."));

        // cannot prev cols initially
        let mut out_prev = Vec::new();
        let mut err_prev = Vec::new();
        dispatch_command(
            Command::PrevCols,
            &mut state,
            &store,
            &registry,
            &config,
            write_opts,
            &path,
            &mut std::io::Cursor::new(b""),
            &mut out_prev,
            &mut err_prev,
        )
        .unwrap();
        let s_prev = String::from_utf8(out_prev).unwrap();
        assert!(s_prev.contains("Already at first columns."));
    }

    #[test]
    fn test_execute_search_and_render_no_more_items_prints_menu() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();
        let mut state = State::new();
        state.to_searched_at_page(
            "ext:rs".to_string(),
            None,
            0,
            5,
            Some(5),
            false,
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        execute_search_and_render(
            "ext:rs", 20, None, &mut state, &store, &registry, &mut out,
            &mut err,
        )
        .unwrap();

        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("No more items."));
        assert!(s.contains("s \x1b[2msearch\x1b[0m"));
    }

    #[test]
    fn test_print_help_output_and_ansi_handling() {
        let mut out = Vec::new();
        print_help(&mut out).unwrap();
        let s = String::from_utf8(out).unwrap();

        assert!(s.contains("TTFM Interactive Mode Help"));
        assert!(s.contains("Commands:"));
        assert!(s.contains("Syntax & Examples:"));
        assert!(s.contains("Basic Tags (type:label):"));
        assert!(s.contains("Set Operations (&, |, -):"));
        assert!(s.contains("Show types/labels/tags definitions & Storing:"));
        assert!(s.contains("# Search with TTQL query"));

        // Verify that raw HELP_TEXT contains dimmed escape codes
        assert!(HELP_TEXT.contains("\x1b[2m"));
        assert!(HELP_TEXT.contains("\x1b[0m"));

        // Verify that each line's visual width is within 140 columns
        for line in HELP_TEXT.lines() {
            let visible_width = console::measure_text_width(line);
            assert!(
                visible_width <= 140,
                "Line exceeds 140 chars width ({}): {:?}",
                visible_width,
                line
            );
        }

        // Verify that strip_ansi_codes completely strips ANSI escapes
        let stripped = console::strip_ansi_codes(HELP_TEXT);
        assert!(!stripped.contains("\x1b["));
    }
}
