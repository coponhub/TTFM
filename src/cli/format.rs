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
use crate::safe_println;
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
    let char_count = text.chars().count();
    if char_count <= max_width {
        text.to_string()
    } else {
        if max_width <= 3 {
            return "...".chars().take(max_width).collect();
        }
        let truncated: String = text.chars().take(max_width - 3).collect();
        format!("{}...", truncated)
    }
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

pub fn format_short_result(registry: &TagRegistry, res: &Item) -> String {
    let nvalue_str = res
        .tags
        .entries
        .iter()
        .find(|e| {
            e.typed_tag.tag_type() == crate::types::TagType::from("nvalue")
        })
        .map(|e| e.typed_tag.as_str().to_string());

    let repr = res.representative.display_keys(registry);
    if let Some(nv) = nvalue_str {
        format!("{} {}", repr, nv)
    } else {
        repr
    }
}

pub fn print_results(
    store: &Store,
    registry: &TagRegistry,
    response: &SearchResponse,
    query: &str,
    _current_n: usize,
    writer: &mut dyn Write,
    is_interactive: bool,
) {
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
        return;
    }

    if response.has_projection_results() {
        print_compact_projections(
            registry,
            response,
            query,
            writer,
            is_interactive,
        );
        return;
    }

    let type_ranks = crate::rank::get_type_ranks(store).unwrap_or_default();
    let term_width = get_terminal_width();

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
            item_id_width = item_id_width.max(res.id.to_string().len());
            for (i, key) in sorted_keys.iter().enumerate() {
                let val = res.get_tag_value(key.as_str()).unwrap_or_default();
                col_widths[i] = col_widths[i].max(val.chars().count());
            }
        }
        for (i, key) in sorted_keys.iter().enumerate() {
            col_widths[i] = col_widths[i].max(key.as_str().len());
        }

        {
            let mut print_line = |res_opt: Option<&Item>| {
                let mut current_width = 0;
                let sep = "  ";
                let sep_len = sep.len();
                let is_header = res_opt.is_none();

                let id_str = res_opt
                    .map(|r| r.id.to_string())
                    .unwrap_or_else(|| "item_id".to_string());
                let available = term_width.saturating_sub(current_width);
                if available == 0 {
                    return;
                }

                let id_disp = if item_id_width <= available {
                    format!("{:<width$}", id_str, width = item_id_width)
                } else {
                    truncate_text(&id_str, available)
                };

                if is_header {
                    write!(writer, "\x1b[1m{}\x1b[0m", id_disp).unwrap_or(());
                } else {
                    write!(writer, "{}", id_disp).unwrap_or(());
                }
                current_width += id_disp.chars().count();

                for (i, key) in sorted_keys.iter().enumerate() {
                    if current_width + sep_len >= term_width {
                        write!(writer, "...").unwrap_or(());
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

                    let avail = term_width.saturating_sub(current_width);
                    if avail == 0 {
                        break;
                    }

                    let target_width = col_widths[i];
                    let out = if target_width <= avail {
                        format!("{:<width$}", val_str, width = target_width)
                    } else {
                        truncate_text(&val_str, avail)
                    };

                    if is_header {
                        write!(writer, "\x1b[1m{}\x1b[0m", out).unwrap_or(());
                    } else {
                        write!(writer, "{}", out).unwrap_or(());
                    }
                    current_width += out.chars().count();
                }
                writeln!(writer).unwrap_or(());
            };

            print_line(None);
            for res in group.results {
                print_line(Some(res));
            }
        }
        writeln!(writer).unwrap_or(());
    }

    writeln!(
        writer,
        "Total: {} results displayed.",
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
                "\x1b[1;32mMore results available.\x1b[0m To see next page, run:"
            )
            .unwrap_or(());
            writeln!(writer, "  ttfm search \"{}\" --cid {}", query, cid)
                .unwrap_or(());
        }
    }
}

pub fn print_compact_projections(
    registry: &TagRegistry,
    response: &SearchResponse,
    query: &str,
    writer: &mut dyn Write,
    is_interactive: bool,
) {
    let term_width = get_terminal_width();

    for label_item in &response.results {
        let total_count = label_item
            .item_count
            .as_ref()
            .and_then(|l| l.as_str().parse::<usize>().ok())
            .unwrap_or(label_item.tags.entries.len());

        let repr_display = label_item.representative.display(registry);

        writeln!(
            writer,
            "\x1b[1;34m:{}\x1b[0m \x1b[2m({} items)\x1b[0m",
            repr_display, total_count
        )
        .unwrap_or(());

        let mut all_items_str = String::new();
        for (i, tag_entry) in
            label_item.tags.entries.iter().take(200).enumerate()
        {
            if i > 0 {
                all_items_str.push_str(", ");
            }
            all_items_str.push_str(&tag_entry.typed_tag.as_str());
            if all_items_str.chars().count() > term_width + 10 {
                break;
            }
        }

        writeln!(
            writer,
            "  {}",
            truncate_text(&all_items_str, term_width.saturating_sub(2))
        )
        .unwrap_or(());
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
    if response.has_projection_results() {
        for label_item in &response.results {
            safe_println!("{}", format_short_result(registry, label_item));
        }
    } else {
        for res in &response.results {
            let line = res.primary_value().unwrap_or_else(|| res.raw_repr());
            safe_println!("{}", line);
        }
    }
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
    use crate::types::{Bitical, ItemId, ItemKind, Origin, SType, TypedTag};
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
    fn test_short_format_with_nvalue() {
        let mut res_with_nvalue =
            Item::new_empty(ItemId::new_volatile(), ItemKind::Volatile);
        res_with_nvalue.representative =
            vec![TypedTag::new(SType::Name, "test_label")].into();
        res_with_nvalue.apply_tag(
            TypedTag::new("nvalue", Bitical::Integer(9986)),
            Origin::Builtin,
        );

        let registry = TagRegistry::with_standard();
        let output = format_short_result(&registry, &res_with_nvalue);
        assert_eq!(output, "test_label 9986");
    }

    #[test]
    fn test_short_format_without_nvalue() {
        let mut res_without_nvalue =
            Item::new_empty(ItemId::new_volatile(), ItemKind::Volatile);
        res_without_nvalue.representative =
            vec![TypedTag::new(SType::Name, "test_label_no_nv")].into();

        let registry = TagRegistry::with_standard();
        let output = format_short_result(&registry, &res_without_nvalue);
        assert_eq!(output, "test_label_no_nv");
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
            .run_single(dir.path(), None::<&fn(usize)>, false)
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
            .run_single(dir.path(), None::<&fn(usize)>, false)
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
            .run_single(dir.path(), None::<&fn(usize)>, false)
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
}
