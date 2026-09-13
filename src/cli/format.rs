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

use crate::db::Store;
use crate::edit::EditResponse;
use crate::query::error::{Warning, WarningSink};
use crate::response::SearchResponse;
use crate::tag::TagRegistry;
use crate::Item;
use std::io::Write;
use terminal_size::{terminal_size, Width};

pub struct ColorWarningSink<W: Write> {
    pub writer: W,
}

impl<W: Write> WarningSink for ColorWarningSink<W> {
    fn warn(&mut self, warning: Warning) {
        let _ = writeln!(self.writer, "\x1b[1;33mWarning: {}\x1b[0m", warning);
    }
}

pub fn truncate_text(text: &str, max_width: usize) -> String {
    if console::measure_text_width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return "...".chars().take(max_width).collect();
    }
    console::truncate_str(text, max_width, "...").to_string()
}

pub fn wrap_text_slices<'a>(
    text: &'a str,
    first_width: usize,
    sub_width: usize,
) -> impl Iterator<Item = &'a str> {
    let mut remaining = text;
    let mut width = first_width;

    std::iter::from_fn(move || {
        if remaining.is_empty() {
            return None;
        }
        let target_w = width.max(1);
        let split_at = remaining
            .char_indices()
            .scan(0, |acc, (idx, ch)| {
                *acc += console::measure_text_width(&ch.to_string());
                Some((idx, *acc, ch.len_utf8()))
            })
            .take_while(|&(idx, w, _)| idx == 0 || w <= target_w)
            .last()
            .map(|(idx, _, len)| idx + len)
            .unwrap_or(remaining.len());

        let (chunk, rest) = remaining.split_at(split_at);
        remaining = rest;
        width = sub_width;
        Some(chunk)
    })
}

pub fn get_terminal_width() -> usize {
    // 環境変数 COLUMNS を最優先（テスト用）
    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(width) = cols.parse() {
            return width;
        }
    }

    // 標準出力、標準エラー、標準入力の順にターミナルサイズ取得を試みる
    if let Some((Width(w), _)) = terminal_size() {
        return w as usize;
    }
    if let Some((Width(w), _)) = terminal_size() {
        return w as usize;
    }

    100 // default fallback
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FormatOptions {
    pub is_interactive: bool,
    pub wide: bool,
    pub col_offset: usize,
    pub current_offset: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColumnPagingInfo {
    pub has_next: bool,
    pub next_col_offset: usize,
    pub has_prev: bool,
}

pub fn print_results(
    store: &Store,
    registry: &TagRegistry,
    response: &SearchResponse,
    query: &str,
    current_n: usize,
    writer: &mut dyn Write,
    is_interactive: bool,
) {
    let _ = print_results_with_options(
        store,
        registry,
        response,
        query,
        current_n,
        writer,
        FormatOptions {
            is_interactive,
            wide: false,
            col_offset: 0,
            ..Default::default()
        },
    );
}

pub fn print_results_with_options(
    store: &Store,
    registry: &TagRegistry,
    response: &SearchResponse,
    query: &str,
    _current_n: usize,
    writer: &mut dyn Write,
    options: FormatOptions,
) -> ColumnPagingInfo {
    debug_assert!(
        !response
            .results
            .iter()
            .any(|r| !r.representative.is_empty() && r.tags.entries.is_empty()),
        "short search results must not be passed to table formatting"
    );
    let mut paging_info = ColumnPagingInfo::default();
    paging_info.has_prev = options.col_offset > 0;

    if response.has_more && !response.progress.is_finished() {
        writeln!(
            writer,
            "\x1b[1;33mSearching... (Background cache generating: {})\x1b[0m",
            response.progress.current
        )
        .unwrap_or(());
    }

    if response.results.is_empty() {
        if response.progress.is_finished() {
            writeln!(writer, "No items found.").unwrap_or(());
        }
        return paging_info;
    }

    if response.has_projection_results() {
        print_compact_projections(
            registry,
            response,
            query,
            writer,
            options.is_interactive,
            options.wide,
        );
        return paging_info;
    }

    let type_ranks = crate::rank::get_type_ranks(store).unwrap_or_default();
    let effective_width = if options.wide {
        usize::MAX
    } else {
        get_terminal_width().saturating_sub(4)
    };

    if let Some(res) = response.results.first().filter(|r| {
        r.id.is_volatile()
            && !r.representative.is_empty()
            && r.tags.entries.iter().any(|e| {
                e.typed_tag.tag_type().as_str() == "value"
                    && matches!(e.origin, crate::types::Origin::Builtin)
            })
    }) {
        let repr = res.representative.display_keys(registry);
        writeln!(writer, "\x1b[1m{}\x1b[0m", repr).unwrap_or(());
    }

    for group in response.iter_type_groups() {
        let mut sorted_keys = group.keys.clone();
        sorted_keys.sort_by(|a, b| {
            let r_a = type_ranks
                .get(a.as_str())
                .filter(|&&r| r != 0)
                .cloned()
                .unwrap_or_else(|| {
                    crate::rank::get_rank_by_name(registry, a.as_str())
                });
            let r_b = type_ranks
                .get(b.as_str())
                .filter(|&&r| r != 0)
                .cloned()
                .unwrap_or_else(|| {
                    crate::rank::get_rank_by_name(registry, b.as_str())
                });
            r_b.cmp(&r_a).then_with(|| a.cmp(b))
        });

        let mut item_id_width = 7;
        let mut col_widths = vec![0; sorted_keys.len()];

        for res in &group.results {
            item_id_width = item_id_width
                .max(console::measure_text_width(&res.id.to_string()));
            for (i, key) in sorted_keys.iter().enumerate() {
                let val_str = res
                    .get_tag_value(key.as_str())
                    .map(|raw| registry.format_display(key.as_str(), &raw))
                    .unwrap_or_default();
                col_widths[i] =
                    col_widths[i].max(console::measure_text_width(&val_str));
            }
        }
        for (i, key) in sorted_keys.iter().enumerate() {
            col_widths[i] =
                col_widths[i].max(console::measure_text_width(key.as_str()));
        }

        let mut group_has_next = false;
        let mut group_next_offset = 0;

        {
            let mut print_line = |res_opt: Option<&Item>| {
                let mut current_width = 0;
                let sep = "  ";
                let sep_len = sep.len();
                let is_header = res_opt.is_none();

                let id_str = res_opt
                    .map(|r| r.id.to_string())
                    .unwrap_or_else(|| "item_id".to_string());
                let available = effective_width.saturating_sub(current_width);
                if available == 0 {
                    return;
                }

                let id_disp = if item_id_width <= available {
                    console::pad_str(
                        &id_str,
                        item_id_width,
                        console::Alignment::Left,
                        None,
                    )
                    .to_string()
                } else {
                    truncate_text(&id_str, available)
                };

                if is_header {
                    write!(writer, "\x1b[1m{}\x1b[0m", id_disp).unwrap_or(());
                } else {
                    write!(writer, "{}", id_disp).unwrap_or(());
                }
                current_width += console::measure_text_width(&id_disp);

                if paging_info.has_prev
                    && current_width + sep_len + 3 <= effective_width
                {
                    write!(writer, "{}{}", sep, "...").unwrap_or(());
                    current_width += sep_len + 3;
                }

                for (i, key) in
                    sorted_keys.iter().enumerate().skip(options.col_offset)
                {
                    let target_width = col_widths[i];
                    let available_content_w =
                        effective_width.saturating_sub(current_width + sep_len);
                    let is_single_oversized = !options.wide
                        && i == options.col_offset
                        && target_width > available_content_w;

                    if is_single_oversized {
                        write!(writer, "{}", sep).unwrap_or(());
                        let val_str = if is_header {
                            key.as_str().to_string()
                        } else {
                            res_opt
                                .and_then(|r| r.get_tag_value(key.as_str()))
                                .map(|raw| {
                                    registry.format_display(key.as_str(), &raw)
                                })
                                .unwrap_or_default()
                        };
                        let pad_indent = " ".repeat(current_width + sep_len);
                        let wrapped = wrap_text_slices(
                            &val_str,
                            available_content_w,
                            available_content_w,
                        )
                        .enumerate()
                        .map(|(w_idx, w_line)| {
                            if w_idx == 0 {
                                if is_header {
                                    format!("\x1b[1m{}\x1b[0m", w_line)
                                } else {
                                    w_line.to_string()
                                }
                            } else {
                                if is_header {
                                    format!(
                                        "{}\x1b[1m{}\x1b[0m",
                                        pad_indent, w_line
                                    )
                                } else {
                                    format!("{}{}", pad_indent, w_line)
                                }
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                        write!(writer, "{}", wrapped).unwrap_or(());
                        if i + 1 < sorted_keys.len() {
                            group_has_next = true;
                            group_next_offset = i + 1;
                        }
                        break;
                    }

                    if !options.wide
                        && current_width + sep_len + target_width
                            > effective_width
                    {
                        if current_width + sep_len + 3 <= effective_width {
                            write!(writer, "{}{}", sep, "...").unwrap_or(());
                        }
                        group_has_next = true;
                        group_next_offset = if i == options.col_offset {
                            (i + 1).min(sorted_keys.len())
                        } else {
                            i
                        };
                        break;
                    }
                    write!(writer, "{}", sep).unwrap_or(());
                    current_width += sep_len;

                    let val_str = if is_header {
                        res_opt
                            .map(|_| "".to_string())
                            .unwrap_or_else(|| key.as_str().to_string())
                    } else {
                        res_opt
                            .and_then(|r| r.get_tag_value(key.as_str()))
                            .map(|raw| {
                                registry.format_display(key.as_str(), &raw)
                            })
                            .unwrap_or_default()
                    };

                    let out = console::pad_str(
                        &val_str,
                        target_width,
                        console::Alignment::Left,
                        None,
                    );

                    if is_header {
                        write!(writer, "\x1b[1m{}\x1b[0m", out).unwrap_or(());
                    } else {
                        write!(writer, "{}", out).unwrap_or(());
                    }
                    current_width += console::measure_text_width(&out);
                }
                writeln!(writer).unwrap_or(());
            };

            print_line(None);
            for res in &group.results {
                print_line(Some(res));
            }
        }
        writeln!(writer).unwrap_or(());

        paging_info.has_next |= group_has_next;
        if group_has_next && paging_info.next_col_offset == 0 {
            paging_info.next_col_offset = group_next_offset;
        }
    }

    if options.is_interactive {
        let count_str = if let Some(total) = response.total_count {
            format!("{total}")
        } else {
            let base_count =
                options.current_offset.unwrap_or(response.results.len());
            if response.has_more {
                format!("{base_count}+")
            } else {
                format!("{base_count}")
            }
        };
        writeln!(writer, "{count_str} items matched.").unwrap_or(());
    } else {
        writeln!(
            writer,
            "Total: {} results displayed.",
            response.results.len()
        )
        .unwrap_or(());
    }

    if response.has_more {
        if options.is_interactive {
            writeln!(
                writer,
                "\x1b[1;32mMore results available.\x1b[0m Type 'n' for next page."
            )
            .unwrap_or(());
        } else if let Some(cid) = &response.cid {
            writeln!(
                writer,
                "\x1b[1;32mMore results available.\x1b[0m To see next page, run:"
            )
            .unwrap_or(());
            writeln!(writer, "  ttfm search \"{}\" --cid {}", query, cid)
                .unwrap_or(());
        }
    }

    paging_info
}

pub fn print_compact_projections(
    registry: &TagRegistry,
    response: &SearchResponse,
    query: &str,
    writer: &mut dyn Write,
    is_interactive: bool,
    wide: bool,
) {
    let effective_width = if wide {
        usize::MAX
    } else {
        get_terminal_width().saturating_sub(4)
    };

    for label_item in &response.results {
        let total_count = label_item
            .item_count
            .as_ref()
            .and_then(|l| l.as_str().parse::<usize>().ok())
            .unwrap_or(label_item.tags.entries.len());

        let repr_display = label_item.representative.display(registry);
        let count_str = format!(" ({} items)", total_count);
        let full_header = format!(":{}", repr_display);

        if !wide
            && console::measure_text_width(&full_header)
                + console::measure_text_width(&count_str)
                > effective_width
        {
            let wrapped = wrap_text_slices(
                &full_header,
                effective_width,
                effective_width.saturating_sub(2),
            )
            .enumerate()
            .map(|(idx, line)| {
                if idx == 0 {
                    format!("\x1b[1;34m{}\x1b[0m", line)
                } else {
                    format!("  \x1b[1;34m{}\x1b[0m", line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
            writeln!(writer, "{}\x1b[2m{}\x1b[0m", wrapped, count_str)
                .unwrap_or(());
        } else {
            writeln!(
                writer,
                "\x1b[1;34m:{}\x1b[0m\x1b[2m{}\x1b[0m",
                repr_display, count_str
            )
            .unwrap_or(());
        }

        let mut all_items_str = String::new();
        let max_items = if wide { usize::MAX } else { 200 };
        for (i, tag_entry) in
            label_item.tags.entries.iter().take(max_items).enumerate()
        {
            if i > 0 {
                all_items_str.push_str(", ");
            }
            all_items_str.push_str(&tag_entry.typed_tag.as_str());
            if !wide
                && console::measure_text_width(&all_items_str)
                    > effective_width + 10
            {
                break;
            }
        }

        if wide {
            writeln!(writer, "  {}", all_items_str).unwrap_or(());
        } else {
            writeln!(
                writer,
                "  {}",
                truncate_text(
                    &all_items_str,
                    effective_width.saturating_sub(2)
                )
            )
            .unwrap_or(());
        }
    }

    writeln!(
        writer,
        "Total: {} unique labels matched the projection.",
        response.results.len()
    )
    .unwrap_or(());

    if response.has_more {
        if is_interactive {
            writeln!(
                writer,
                "\x1b[1;32mMore results available.\x1b[0m Type 'n' for next page."
            )
            .unwrap_or(());
        } else if let Some(cid) = &response.cid {
            writeln!(
                writer,
                "\n\x1b[1;32mMore items available.\x1b[0m To see next page, run:"
            )
            .unwrap_or(());
            writeln!(writer, "  ttfm search \"{}\" --cid {}", query, cid)
                .unwrap_or(());
        }
    }
}

pub fn print_simple_results(registry: &TagRegistry, response: &SearchResponse) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if let Err(e) =
        print_simple_results_to_writer(registry, response, &mut handle)
    {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        panic!("failed printing to stdout: {e}");
    }
}

pub fn print_simple_results_to_writer(
    registry: &TagRegistry,
    response: &SearchResponse,
    writer: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    for res in &response.results {
        if !res.representative.is_empty() {
            writeln!(writer, "{}", res.representative.display_short(registry))?;
        } else {
            let line = res.primary_value().unwrap_or_else(|| res.raw_repr());
            writeln!(writer, "{}", line)?;
        }
    }
    Ok(())
}

pub fn format_tag_result(resp: &EditResponse) -> String {
    let mut msg =
        format!("Updated tags: {}, files: {}.", resp.updated, resp.fs_ops);
    if resp.has_skipped {
        msg.push_str(" (Some items skipped)");
    }
    msg
}

pub fn format_untag_result(resp: &EditResponse) -> String {
    let mut msg =
        format!("Deleted tags: {}, files: {}.", resp.deleted, resp.fs_ops);
    if resp.has_skipped {
        msg.push_str(" (Some items skipped)");
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::Representative;
    use crate::types::{ItemId, ItemKind, Origin, SType, TypedTag};
    use std::sync::Mutex;

    static COLUMNS_MUTEX: Mutex<()> = Mutex::new(());

    fn make_store_and_registry(
        db_dir: &std::path::Path,
    ) -> (Store, TagRegistry) {
        let store = Store::open(db_dir).unwrap();
        let registry = TagRegistry::with_standard();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();
        (store, registry)
    }

    #[test]
    fn test_print_simple_results_to_writer_by_representative_presence() {
        let registry = TagRegistry::default();
        let mut resp = SearchResponse::default();
        let mut item1 = Item::new_empty(ItemId::Stored(1), ItemKind::File);
        item1.representative = Representative {
            tags: vec![TypedTag::new(SType::Name, "rust")],
            nvalue: None,
        };
        let mut item2 = Item::new_empty(ItemId::Stored(2), ItemKind::File);
        item2
            .tags
            .push(TypedTag::new(SType::Path, "src/main.rs"), Origin::Builtin);
        resp.results = vec![item1, item2];
        let mut out = Vec::new();
        print_simple_results_to_writer(&registry, &resp, &mut out).unwrap();
        let out_str = String::from_utf8(out).unwrap();
        assert!(out_str.contains("rust\n"));
        assert!(out_str.contains("src/main.rs\n"));
    }

    #[test]
    #[should_panic(
        expected = "short search results must not be passed to table formatting"
    )]
    fn test_print_results_with_options_panics_on_short_projection_results() {
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let (store, registry) = make_store_and_registry(&db_dir);
        let mut resp = SearchResponse::default();
        let mut item = Item::new_empty(ItemId::Stored(1), ItemKind::File);
        item.representative = Representative {
            tags: vec![TypedTag::new(SType::Name, "rust")],
            nvalue: None,
        };
        resp.results = vec![item];
        let mut out = Vec::new();
        print_results_with_options(
            &store,
            &registry,
            &resp,
            "extension:",
            10,
            &mut out,
            FormatOptions::default(),
        );
    }

    #[test]
    fn test_print_results_formats_size_as_human_readable() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "500");
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let (store, registry) = make_store_and_registry(&db_dir);

        let test_file = dir.path().join("sized.bin");
        std::fs::write(&test_file, vec![0u8; 1024]).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .run_single(dir.path(), None, false)
            .unwrap();

        let response = crate::search::search_nowarn(
            &store,
            &registry,
            "name:sized.bin",
            crate::SearchOptions {
                n: Some(100),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !response.results.is_empty(),
            "ファイルがインデックスされていない"
        );

        let mut out = Vec::<u8>::new();
        print_results(
            &store,
            &registry,
            &response,
            "name:sized.bin",
            100,
            &mut out,
            false,
        );
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        assert!(
            output.contains("1.00KB"),
            "size should show '1.00KB', got:\n{}",
            output
        );
    }

    #[test]
    fn test_print_results_formats_mtime_as_human_readable() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "500");
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let (store, registry) = make_store_and_registry(&db_dir);

        let test_file = dir.path().join("dated.txt");
        std::fs::write(&test_file, b"hi").unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .run_single(dir.path(), None, false)
            .unwrap();

        let response = crate::search::search_nowarn(
            &store,
            &registry,
            "name:dated.txt",
            crate::SearchOptions {
                n: Some(100),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!response.results.is_empty());

        let mut out = Vec::<u8>::new();
        print_results(
            &store,
            &registry,
            &response,
            "name:dated.txt",
            100,
            &mut out,
            false,
        );
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        assert!(
            output.contains("2026") || output.contains("2025"),
            "mtime should show year, got:\n{}",
            output
        );
    }

    #[test]
    fn test_print_results_bold_header_for_scalar_result() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "500");
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let (store, registry) = make_store_and_registry(&db_dir);

        std::fs::write(dir.path().join("sized.bin"), vec![0u8; 1024]).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .run_single(dir.path(), None, false)
            .unwrap();

        let response = crate::search::search_nowarn(
            &store,
            &registry,
            "sum(size:)",
            crate::SearchOptions::default(),
        )
        .unwrap();

        let mut out = Vec::<u8>::new();
        print_results(
            &store,
            &registry,
            &response,
            "sum(size:)",
            100,
            &mut out,
            false,
        );
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        let first_line = output.lines().next().unwrap_or("");
        assert!(
            !first_line.contains("item_id"),
            "scalar result should still show the bold representative header before the table, got:\n{}",
            output
        );
    }

    #[test]
    fn test_print_results_no_bold_header_for_definition_item_results() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "500");
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let (store, registry) = make_store_and_registry(&db_dir);

        let response = crate::search::search_nowarn(
            &store,
            &registry,
            "type:*",
            crate::SearchOptions::default(),
        )
        .unwrap();

        let mut out = Vec::<u8>::new();
        print_results(
            &store, &registry, &response, "type:*", 100, &mut out, false,
        );
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        let first_line = output.lines().next().unwrap_or("");
        assert!(
            first_line.contains("item_id"),
            "definition item results should not misfire the scalar bold header, expected the table header first, got:\n{}",
            output
        );
    }

    #[test]
    fn test_color_warning_sink_writes_immediately() {
        let mut out = Vec::<u8>::new();
        let mut sink = ColorWarningSink { writer: &mut out };
        sink.warn(Warning(
            "Projection intersection ('&') found. Did you mean '&:' (Nest) to group results?".to_string(),
        ));
        let text = String::from_utf8(out).unwrap();
        assert!(!text.is_empty(), "ColorWarningSink should produce output");
        assert!(text.contains("&:"), "output should contain '&:' suggestion");
        assert!(
            !text.contains("Warning: Warning:"),
            "should not contain duplicate Warning: prefix"
        );
        assert!(
            text.starts_with("\x1b[1;33mWarning: Projection"),
            "should start with colored single Warning: prefix, got: {:?}",
            text
        );
    }

    #[test]
    fn test_format_tag_result() {
        let resp = EditResponse {
            updated: 1,
            deleted: 0,
            fs_ops: 2,
            has_skipped: false,
        };
        assert_eq!(format_tag_result(&resp), "Updated tags: 1, files: 2.");

        let resp_zero = EditResponse {
            updated: 0,
            deleted: 0,
            fs_ops: 1,
            has_skipped: false,
        };
        assert_eq!(format_tag_result(&resp_zero), "Updated tags: 0, files: 1.");
    }

    #[test]
    fn test_format_untag_result() {
        let resp = EditResponse {
            updated: 0,
            deleted: 3,
            fs_ops: 0,
            has_skipped: false,
        };
        assert_eq!(format_untag_result(&resp), "Deleted tags: 3, files: 0.");
    }

    #[test]
    fn test_print_results_no_generating_warning_on_completed_cache_with_has_more(
    ) {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "500");
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let (store, registry) = make_store_and_registry(&db_dir);

        let mut response = crate::search::search_nowarn(
            &store,
            &registry,
            "type:*",
            crate::SearchOptions::default(),
        )
        .unwrap();

        response.has_more = true;
        response.progress.is_done = true;

        let mut out = Vec::<u8>::new();
        print_results(
            &store, &registry, &response, "type:*", 10, &mut out, false,
        );
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        assert!(
            !output.contains("Background cache generating"),
            "Completed cache with has_more=true must not display generating warning, got:\n{}",
            output
        );
    }

    #[test]
    fn test_truncate_text_boundary_safety() {
        assert_eq!(truncate_text("abcdef", 10), "abcdef");
        assert_eq!(truncate_text("a", 2), "a");
        assert_eq!(truncate_text("abcdef", 2), "..");
        assert_eq!(truncate_text("abcdef", 0), "");
    }

    #[test]
    fn test_print_results_drops_whole_columns_cleanly() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "40");
        let dir = tempfile::tempdir().unwrap();
        let (store, registry) = make_store_and_registry(&dir.path().join("db"));
        let response = crate::search::search_nowarn(
            &store,
            &registry,
            "type:*",
            Default::default(),
        )
        .unwrap();
        let mut out = Vec::new();
        print_results(
            &store, &registry, &response, "type:*", 100, &mut out, false,
        );
        std::env::remove_var("COLUMNS");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("item_id"));
        for line in text.lines().filter(|l| {
            !l.is_empty()
                && !l.contains("displayed")
                && !l.contains("available")
        }) {
            assert!(
                console::measure_text_width(line) <= 40 - 4,
                "Line width {} exceeds 36 for line: {:?}",
                console::measure_text_width(line),
                line
            );
        }
    }

    #[test]
    fn test_print_results_wide_bypasses_effective_width() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "40");
        let dir = tempfile::tempdir().unwrap();
        let (store, registry) = make_store_and_registry(&dir.path().join("db"));
        let response = crate::search::search_nowarn(
            &store,
            &registry,
            "type:*",
            Default::default(),
        )
        .unwrap();
        let mut out = Vec::new();
        let paging = print_results_with_options(
            &store,
            &registry,
            &response,
            "type:*",
            100,
            &mut out,
            FormatOptions {
                is_interactive: false,
                wide: true,
                col_offset: 0,
                ..Default::default()
            },
        );
        std::env::remove_var("COLUMNS");
        let text = String::from_utf8(out).unwrap();
        assert!(!paging.has_next);
        assert!(!text.contains("..."));
    }

    #[test]
    fn test_print_results_col_offset_inserts_leading_and_trailing_ellipsis() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "40");
        let dir = tempfile::tempdir().unwrap();
        let (store, registry) = make_store_and_registry(&dir.path().join("db"));
        let response = crate::search::search_nowarn(
            &store,
            &registry,
            "type:*",
            Default::default(),
        )
        .unwrap();
        let mut out = Vec::new();
        let paging = print_results_with_options(
            &store,
            &registry,
            &response,
            "type:*",
            100,
            &mut out,
            FormatOptions {
                is_interactive: true,
                wide: false,
                col_offset: 1,
                ..Default::default()
            },
        );
        std::env::remove_var("COLUMNS");
        let text = String::from_utf8(out).unwrap();
        assert!(paging.has_prev);
        let clean = console::strip_ansi_codes(&text).to_string();
        assert!(clean.contains("item_id  ..."));
    }

    #[test]
    fn test_print_results_interactive_matched_footer() {
        let dir = tempfile::tempdir().unwrap();
        let (store, registry) = make_store_and_registry(&dir.path().join("db"));
        let mut response = SearchResponse::default();
        response.results =
            vec![Item::new_empty(ItemId::new_volatile(), ItemKind::Volatile)];
        response.has_more = true;

        let mut out = Vec::new();
        let _ = print_results_with_options(
            &store,
            &registry,
            &response,
            "ext:rs",
            20,
            &mut out,
            FormatOptions {
                is_interactive: true,
                wide: false,
                col_offset: 0,
                current_offset: Some(20),
            },
        );
        let out_str = String::from_utf8(out).unwrap();
        assert!(out_str.contains("20+ items matched."));
        assert!(!out_str.contains("results displayed."));

        let mut out_final = Vec::new();
        response.has_more = false;
        let _ = print_results_with_options(
            &store,
            &registry,
            &response,
            "ext:rs",
            20,
            &mut out_final,
            FormatOptions {
                is_interactive: true,
                wide: false,
                col_offset: 0,
                current_offset: Some(45),
            },
        );
        let out_final_str = String::from_utf8(out_final).unwrap();
        assert!(out_final_str.contains("45 items matched."));
        assert!(!out_final_str.contains("results displayed."));
    }

    #[test]
    fn test_wrap_text_slices_functional() {
        let text = "a".repeat(100);
        let slices: Vec<&str> = wrap_text_slices(&text, 40, 30).collect();
        assert_eq!(slices.len(), 3);
        assert_eq!(slices[0].len(), 40);
        assert_eq!(slices[1].len(), 30);
        assert_eq!(slices[2].len(), 30);
    }

    #[test]
    fn test_print_compact_projections_wide_bypasses_truncation() {
        let registry = TagRegistry::with_standard();
        let mut resp = SearchResponse::default();
        let mut item =
            Item::new_empty(ItemId::new_volatile(), ItemKind::Volatile);
        for i in 0..50 {
            item.tags.push(
                crate::types::TypedTag::new("project", format!("p_{i}")),
                crate::types::Origin::User,
            );
        }
        resp.results.push(item);
        let mut out = Vec::new();
        print_compact_projections(
            &registry,
            &resp,
            "extension:",
            &mut out,
            false,
            true,
        );
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("..."));
    }
}
