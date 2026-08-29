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

use anyhow::Result;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
// std::collections は不要になったため削除
use std::path::PathBuf;
use std::time::Duration;
use terminal_size::{terminal_size, Width};
use ttfm::config::Config;
use ttfm::db::Store;
use ttfm::edit::{edit, QueryType, WriteOptions};
use ttfm::tag::TagRegistry;

macro_rules! safe_println {
    ($($arg:tt)*) => {
        {
            use std::io::Write;
            writeln!(std::io::stdout(), $($arg)*).unwrap_or_else(|e| {
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    std::process::exit(0);
                }
                panic!("failed printing to stdout: {}", e);
            });
        }
    };
}

/// TTFM (Typed Tag File Manager) のメインCLI構造体。
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(arg_required_else_help = true)]
struct Cli {
    /// 実行するサブコマンド
    #[command(subcommand)]
    command: Commands,

    /// 全ての確認をスキップして 'yes' と回答します
    #[arg(short, long, global = true)]
    yes: bool,

    /// 確認プロンプトの動作 (auto, always, never)
    #[arg(long, global = true)]
    confirm: Option<ttfm::config::ConfirmMode>,

    /// 移動先の重複・衝突時のポリシー (abort, skip, serial, first)
    #[arg(long, global = true)]
    on_conflict: Option<ttfm::config::ConflictPolicy>,

    /// ハードリンク検出時のポリシー (abort, skip, all)
    #[arg(long, global = true)]
    on_hardlink: Option<ttfm::config::HardlinkPolicy>,

    /// スキップ時の除外範囲 (item, fs-only)
    #[arg(long, global = true)]
    skip_scope: Option<ttfm::config::SkipScope>,
}

/// TTFM で利用可能なサブコマンド。
#[derive(Subcommand)]
enum Commands {
    /// 指定されたディレクトリを再帰的にスキャンし、インデックスを作成します。
    Index {
        /// スキャンを開始するディレクトリパス（例: "." や "/home/user"。複数指定可）
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,

        /// trueの場合、データベースへの書き込みやParquet保存を行わず、スキャン速度の計測のみを行います。
        #[arg(long)]
        dry_run: bool,
    },
    /// クエリを使用してファイルを検索します。
    Search {
        /// 検索クエリ文字列。
        query: String,
        /// シンプルな出力モード。
        #[arg(short, long)]
        short: bool,
        /// 取得件数 (None または 0 は全件)
        #[arg(short, long)]
        n: Option<usize>,
        /// 開始位置
        #[arg(long)]
        offset: Option<usize>,
        /// キャッシュID (ページング用)
        #[arg(long)]
        cid: Option<String>,
    },
    /// インデックスからファイルの一覧を表示します。
    List {
        /// シンプルな出力モード。
        #[arg(short, long)]
        short: bool,
        /// 取得件数
        #[arg(short, long)]
        n: Option<usize>,
        /// 開始位置
        #[arg(long)]
        offset: Option<usize>,
        /// キャッシュID
        #[arg(long)]
        cid: Option<String>,
    },
    /// 作成されたインデックスファイルを削除します。
    Clear {
        /// データベース全体（設定やタグ情報など）を削除します。
        #[arg(short, long)]
        all: bool,
    },
    /// マッチしたアイテムにタグを付与します。
    Tag {
        /// 対象を絞るクエリ（例: "filename:foo.txt"）
        search_query: String,
        /// 付与するタグ（例: "project:A status:done"）
        edit_query: Option<String>,
    },
    /// マッチしたアイテムからタグを削除します。
    Untag {
        /// 対象を絞るクエリ
        search_query: String,
        /// 削除するタグ（TypedTag または Projection）
        tag_query: String,
        /// 削除条件（例: "tagged_at:>2024-01-01"）
        #[arg(long)]
        condition: Option<String>,
    },
    /// タグを付け替えます（OLD → NEW）。
    Replace {
        /// 対象を絞るクエリ兼 Replace 対象（例: "project:A"）
        old: String,
        /// 新しいタグ（例: "status:A"）
        new_tag: String,
    },
    /// From のアイテムが持つタグ群を To のアイテムへ転写します。
    Decal {
        /// 転写元クエリ
        from: String,
        /// 転写先クエリ
        to: String,
    },
    /// メモを作成します。
    Note {
        /// メモの内容
        content: String,
    },
}

/// アプリケーションのエントリポイント。
fn main() -> Result<()> {
    let cli = Cli::parse();

    // Clear コマンドの場合は完全な初期化をスキップし、破損したDBでも削除できるようにする
    if let Commands::Clear { all } = cli.command {
        let home = ttfm::get_ttfm_home()?;
        if all {
            Store::delete_database(home.join("db").as_path())?;
            safe_println!("Database cleared successfully.");
        } else {
            let store = Store::open(home.join("db"))?;
            store.clear_index()?;
            safe_println!("File indexes cleared successfully.");
        }
        return Ok(());
    }

    let home = ttfm::get_ttfm_home()?;

    // ユーザープラグイン用ディレクトリの準備
    let plugins_dir = home.join("plugins");
    if !plugins_dir.exists() {
        std::fs::create_dir_all(&plugins_dir)?;
    }

    let mut registry = TagRegistry::with_standard();

    // 設定ファイルの読み込み
    let config = Config::load();

    // プラグインが有効な場合のみロード（ユーザー → ビルトインの順、同名はユーザーが優先）
    if config.plugins.enabled {
        registry.load_from_dir(
            ttfm::get_ttfm_plugins_dir()?,
            &config.plugins.status,
        )?;
        registry.load_builtins(&config.plugins.status)?;
    }

    let store = Store::open(home.join("db"))?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    registry.load_type_configs(&store)?;

    match &cli.command {
        Commands::Index { paths, dry_run } => {
            safe_println!(
                "Indexing directories: {:?} (dry-run: {})",
                paths,
                dry_run
            );

            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .tick_chars(r"/|\-")
                    .template("{spinner:.green} {msg}")?,
            );
            pb.set_message("Scanning...");
            pb.enable_steady_tick(Duration::from_millis(100));

            let count = ttfm::indexing::Indexer::new(&store, &registry).run(
                paths,
                Some(&|count| {
                    pb.set_message(format!("Indexed {} files...", count));
                }),
                *dry_run,
            )?;

            pb.finish_with_message(format!(
                "Done! Successfully indexed {} files.",
                count
            ));
        }
        Commands::Search {
            query,
            short,
            n,
            offset,
            cid,
        } => {
            if !*short {
                safe_println!("Searching for: '{}'", query);
            }
            let opts = ttfm::SearchOptions {
                n: Some(n.unwrap_or(100)),
                offset: *offset,
                cid: cid.clone(),
                ..Default::default()
            };
            let mut stdout = std::io::stdout();
            let response = ttfm::search::search(
                &store,
                &registry,
                query,
                opts,
                &mut ColorWarningSink {
                    writer: &mut stdout,
                },
            )?;
            if *short {
                print_simple_results(&registry, &response);
            } else {
                print_results(
                    &store,
                    &registry,
                    &response,
                    query,
                    n.unwrap_or(100),
                    &mut std::io::stdout(),
                );
            }
        }
        Commands::List {
            short,
            n,
            offset,
            cid,
        } => {
            if !*short {
                safe_println!("Listing files...");
            }
            let opts = ttfm::SearchOptions {
                n: Some(n.unwrap_or(100)),
                offset: *offset,
                cid: cid.clone(),
                ..Default::default()
            };
            let mut stdout = std::io::stdout();
            let response = ttfm::search::search(
                &store,
                &registry,
                "",
                opts,
                &mut ColorWarningSink {
                    writer: &mut stdout,
                },
            )?;
            if *short {
                print_simple_results(&registry, &response);
            } else {
                print_results(
                    &store,
                    &registry,
                    &response,
                    "list",
                    n.unwrap_or(100),
                    &mut std::io::stdout(),
                );
            }
        }
        Commands::Tag {
            search_query,
            edit_query,
        } => {
            let mut stdout = std::io::stdout();
            let resp = edit(
                &store,
                &registry,
                search_query,
                edit_query.as_deref(),
                QueryType::Tag,
                None,
                build_write_options(&cli, &config),
                &mut ColorWarningSink {
                    writer: &mut stdout,
                },
            )?;
            safe_println!("{}", format_tag_result(&resp));
        }
        Commands::Untag {
            search_query,
            tag_query,
            condition,
        } => {
            let mut stdout = std::io::stdout();
            let resp = edit(
                &store,
                &registry,
                search_query,
                Some(tag_query.as_str()),
                QueryType::Untag,
                condition.as_deref(),
                build_write_options(&cli, &config),
                &mut ColorWarningSink {
                    writer: &mut stdout,
                },
            )?;
            safe_println!("{}", format_untag_result(&resp));
        }
        Commands::Replace { old: _, new_tag: _ } => {
            anyhow::bail!("replace は未実装です");
        }
        Commands::Decal { from: _, to: _ } => {
            anyhow::bail!("decal は未実装です");
        }
        Commands::Clear { .. } => unreachable!("Handled early"),
        Commands::Note { content } => {
            let id =
                ttfm::tagging::add_item(&store, &registry, "note", content)?;
            safe_println!("Created note (ID: {})", id);
        }
    }

    Ok(())
}

fn build_write_options(cli: &Cli, config: &Config) -> WriteOptions {
    let confirm = if cli.yes {
        ttfm::config::ConfirmMode::Never
    } else {
        cli.confirm.unwrap_or(config.edit.confirm)
    };
    WriteOptions {
        confirm,
        on_conflict: cli.on_conflict.or(config.edit.on_conflict),
        on_hardlink: cli.on_hardlink.or(config.edit.on_hardlink),
        skip_scope: cli.skip_scope.unwrap_or(config.edit.skip_scope),
    }
}

/// 文字列を指定された最大幅で切り詰めます。
fn truncate_text(text: &str, max_width: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_width {
        text.to_string()
    } else {
        if max_width <= 3 {
            return "...".chars().take(max_width).collect();
        }
        let truncated: String = text.chars().take(max_width - 3).collect();
        format!(
            "{}\
...",
            truncated
        )
    }
}

/// ターミナルの幅を取得します。
fn get_terminal_width() -> usize {
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
    // stderr からの取得を試みる (stdout が head 等にパイプされている場合のため)
    // terminal_size クエリ自体が内部で fd を試行してくれるが、明示的に stderr を見る
    if let Some((Width(w), _)) = terminal_size() {
        return w as usize;
    }

    100 // default fallback
}

struct ColorWarningSink<'a> {
    writer: &'a mut dyn std::io::Write,
}

impl ttfm::query::error::WarningSink for ColorWarningSink<'_> {
    fn warn(&mut self, warning: ttfm::query::error::Warning) {
        writeln!(self.writer, "\x1b[1;33m{}\x1b[0m", warning).unwrap_or(());
    }
}

/// 検索結果の一覧を標準出力に表示します。
fn print_results(
    store: &Store,
    registry: &TagRegistry,
    response: &ttfm::SearchResponse,
    query: &str,
    current_n: usize,
    writer: &mut dyn std::io::Write,
) {
    // 続きがある場合のみ、進捗状況（キャッシュ生成待ち）をチェックして表示
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

    // item: タグが注入されている場合は Projection グループ表示
    if response.has_projection_results() {
        print_compact_projections(registry, response, query, current_n, writer);
        return;
    }

    // データベースからタグ型のランクを取得
    let type_ranks = ttfm::rank::get_type_ranks(store).unwrap_or_default();
    let term_width = get_terminal_width();

    // volatile スカラー結果のフォーマット済み representative をテーブル上部に表示
    // ("value" タグ (System origin) を持つ結果に限定し、定義アイテムを持つ結果等の誤爆を防ぐ)
    if let Some(res) = response.results.first().filter(|r| {
        r.id.is_volatile()
            && !r.representative.is_empty()
            && r.tags.entries.iter().any(|e| {
                e.typed_tag.tag_type().as_str() == "value"
                    && matches!(e.origin, ttfm::types::Origin::Builtin)
            })
    }) {
        let repr = res.representative.display_keys(registry);
        writeln!(writer, "\x1b[1m{}\x1b[0m", repr).unwrap_or(());
    }

    // TypeGroup ごとに表示を行う
    for group in response.iter_type_groups() {
        // カラム（TagType）をランク順に並び替え
        let mut sorted_keys = group.keys.clone();
        sorted_keys.sort_by(|a, b| {
            let r_a = type_ranks
                .get(a.as_str())
                .filter(|&&r| r != 0)
                .cloned()
                .unwrap_or_else(|| {
                    ttfm::rank::get_rank_by_name(registry, a.as_str())
                });
            let r_b = type_ranks
                .get(b.as_str())
                .filter(|&&r| r != 0)
                .cloned()
                .unwrap_or_else(|| {
                    ttfm::rank::get_rank_by_name(registry, b.as_str())
                });
            r_b.cmp(&r_a).then_with(|| a.cmp(b))
        });

        // テーブル幅の計算
        let mut item_id_width = 7; // "item_id"
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

        // 行の出力ヘルパー（writer を借用するためブロックで囲む）
        {
            let mut print_line = |res_opt: Option<&ttfm::Item>| {
                let mut current_width = 0;
                let sep = "  ";
                let sep_len = sep.len();
                let is_header = res_opt.is_none();

                // item_id
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

                // 各属性カラム
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

            print_line(None); // Header
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
        if let Some(cid) = &response.cid {
            writeln!(
                writer,
                "\x1b[1;32mMore results available.\x1b[0m To see next page, run:"
            ).unwrap_or(());
            writeln!(writer, "  ttfm search \"{}\" --cid {}", query, cid)
                .unwrap_or(());
        }
    }
}

/// 投影クエリの結果をラベルごとに集約してコンパクトに表示します。
fn print_compact_projections(
    registry: &TagRegistry,
    response: &ttfm::SearchResponse,
    query: &str,
    _current_n: usize,
    writer: &mut dyn std::io::Write,
) {
    let term_width = get_terminal_width();

    // Phase 2: results には label items（転置）が格納されている
    for label_item in &response.results {
        // total_count を item_count から取得
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

        // 2行目: アイテムリスト (tagsから抽出: item:name#id, ...)
        let mut all_items_str = String::new();
        for (i, tag_entry) in
            label_item.tags.entries.iter().take(200).enumerate()
        {
            if i > 0 {
                all_items_str.push_str(", ");
            }
            // タグは "item:name#id" 形式
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
        if let Some(cid) = &response.cid {
            writeln!(
                writer,
                "\n\x1b[1;32mMore items available.\x1b[0m To see next page, run:"
            ).unwrap_or(());
            writeln!(writer, "  ttfm search \"{}\" --cid {}", query, cid)
                .unwrap_or(());
        }
    }
}

/// シンプルな形式（1行1アイテム、ヘッダーなし、色なし）で結果を出力します。
fn print_simple_results(
    registry: &TagRegistry,
    response: &ttfm::SearchResponse,
) {
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

/// --short 時のアイテム表示に必要な文字列を生成します。
fn format_short_result(registry: &TagRegistry, res: &ttfm::Item) -> String {
    let nvalue_str = res
        .tags
        .entries
        .iter()
        .find(|e| {
            e.typed_tag.tag_type() == ttfm::types::TagType::from("nvalue")
        })
        .map(|e| e.typed_tag.as_str().to_string());

    let repr = res.representative.display_keys(registry);
    if let Some(nv) = nvalue_str {
        format!("{} {}", repr, nv)
    } else {
        repr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use ttfm::types::{Bitical, ItemId, ItemKind, Origin, SType, TypedTag};
    use ttfm::Item;

    // COLUMNS 環境変数を操作するテストを直列化するための Mutex
    static COLUMNS_MUTEX: Mutex<()> = Mutex::new(());

    fn make_store_and_registry(
        db_dir: &std::path::Path,
    ) -> (ttfm::db::Store, TagRegistry) {
        let store = ttfm::db::Store::open(db_dir).unwrap();
        let registry = TagRegistry::with_standard();
        ttfm::indexing::Indexer::new(&store, &registry)
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

    // --- print_results の display フォーマット確認 ---
    // get_terminal_width() は COLUMNS 環境変数を優先するため、
    // 列が切れないよう十分な幅を指定してから print_results を呼ぶ。

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
        ttfm::indexing::Indexer::new(&store, &registry)
            .run_single(dir.path(), None::<&fn(usize)>, false)
            .unwrap();

        let response = ttfm::search::search_nowarn(
            &store,
            &registry,
            "name:sized.bin",
            ttfm::SearchOptions {
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
        ttfm::indexing::Indexer::new(&store, &registry)
            .run_single(dir.path(), None::<&fn(usize)>, false)
            .unwrap();

        let response = ttfm::search::search_nowarn(
            &store,
            &registry,
            "name:dated.txt",
            ttfm::SearchOptions {
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
        ttfm::indexing::Indexer::new(&store, &registry)
            .run_single(dir.path(), None::<&fn(usize)>, false)
            .unwrap();

        let response = ttfm::search::search_nowarn(
            &store,
            &registry,
            "sum(size:)",
            ttfm::SearchOptions::default(),
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
        );
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        let first_line = output.lines().next().unwrap_or("");
        assert!(
            !first_line.contains("item_id"),
            "scalar result should still show the bold representative \
             header before the table, got:\n{}",
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

        let response = ttfm::search::search_nowarn(
            &store,
            &registry,
            "type:*",
            ttfm::SearchOptions::default(),
        )
        .unwrap();

        let mut out = Vec::<u8>::new();
        print_results(&store, &registry, &response, "type:*", 100, &mut out);
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        let first_line = output.lines().next().unwrap_or("");
        assert!(
            first_line.contains("item_id"),
            "definition item results should not misfire the scalar bold header, \
             expected the table header first, got:\n{}",
            output
        );
    }

    #[test]
    fn test_color_warning_sink_writes_immediately() {
        use ttfm::query::error::{Warning, WarningSink};
        let mut out = Vec::<u8>::new();
        let mut sink = ColorWarningSink { writer: &mut out };
        sink.warn(Warning(
            "Projection intersection ('&') found. Did you mean '&:' (Nest) to group results?".to_string(),
        ));
        let text = String::from_utf8(out).unwrap();
        assert!(!text.is_empty(), "ColorWarningSink should produce output");
        assert!(text.contains("&:"), "output should contain '&:' suggestion");
    }

    #[test]
    fn test_format_tag_result() {
        let resp = ttfm::edit::EditResponse {
            updated: 1,
            deleted: 0,
            fs_ops: 2,
            has_skipped: false,
        };
        assert_eq!(format_tag_result(&resp), "Updated tags: 1, files: 2.");

        let resp_zero = ttfm::edit::EditResponse {
            updated: 0,
            deleted: 0,
            fs_ops: 1,
            has_skipped: false,
        };
        assert_eq!(format_tag_result(&resp_zero), "Updated tags: 0, files: 1.");
    }

    #[test]
    fn test_format_untag_result() {
        let resp = ttfm::edit::EditResponse {
            updated: 0,
            deleted: 3,
            fs_ops: 0,
            has_skipped: false,
        };
        assert_eq!(format_untag_result(&resp), "Deleted tags: 3, files: 0.");
    }
}

fn format_tag_result(resp: &ttfm::edit::EditResponse) -> String {
    format!("Updated tags: {}, files: {}.", resp.updated, resp.fs_ops)
}

fn format_untag_result(resp: &ttfm::edit::EditResponse) -> String {
    format!("Deleted tags: {}, files: {}.", resp.deleted, resp.fs_ops)
}
