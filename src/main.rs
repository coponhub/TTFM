use anyhow::Result;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
// std::collections は不要になったため削除
use std::path::PathBuf;
use std::time::Duration;
use terminal_size::{terminal_size, Width};
use ttfm::config::Config;
use ttfm::{FileManager, SearchOptions};

macro_rules! safe_print {
    ($($arg:tt)*) => {
        {
            use std::io::Write;
            write!(std::io::stdout(), $($arg)*).unwrap_or_else(|e| {
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    std::process::exit(0);
                }
                panic!("failed printing to stdout: {}", e);
            });
        }
    };
}

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
}

/// TTFM で利用可能なサブコマンド。
#[derive(Subcommand)]
enum Commands {
    /// 指定されたディレクトリを再帰的にスキャンし、インデックスを作成します。
    Index {
        /// スキャンを開始するディレクトリパス（例: "." や "/home/user"）
        path: PathBuf,

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
    Clear,
    /// アイテムにタグを付与します（例: ttfm tag "path/to/file" "project:ttfm"）。
    Tag {
        /// 対象のパスまたはID
        item: String,
        /// 付与するタグ (key:value)
        tag: String,
    },
    /// メモを作成します。
    Note {
        /// メモの内容
        content: String,
    },
    /// アイテムに優先度（RANK）を設定します。
    Rank {
        /// 対象のクエリ（例: "extension:rs"）
        item: String,
        /// 設定する優先度（数値が大きいほど上位に表示）
        value: i64,
    },
}

/// アプリケーションのエントリポイント。
fn main() -> Result<()> {
    let cli = Cli::parse();

    // Clear コマンドの場合は完全な初期化をスキップし、破損したDBでも削除できるようにする
    if matches!(cli.command, Commands::Clear) {
        FileManager::delete_database()?;
        safe_println!("Database cleared successfully.");
        return Ok(());
    }

    let mut fm = FileManager::new()?;

    // 設定ファイルの読み込み
    let config = Config::load();

    // プラグインが有効な場合のみロード
    if config.plugins.enabled {
        let plugins_dir = ttfm::get_ttfm_plugins_dir()?;
        fm.load_plugins(plugins_dir, &config.plugins.status)?;
    }

    match &cli.command {
        Commands::Index { path, dry_run } => {
            safe_println!(
                "Indexing directory: {:?} (dry-run: {})",
                path,
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

            let count = fm.index_directory(
                path,
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
                n: *n,
                offset: *offset,
                cid: cid.clone(),
            };
            let response = fm.search(query, opts)?;
            if *short {
                print_simple_results(&response);
            } else {
                print_results(&fm, &response, query, n.unwrap_or(100));
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
                n: *n,
                offset: *offset,
                cid: cid.clone(),
            };
            let response = fm.search("", opts)?;
            if *short {
                print_simple_results(&response);
            } else {
                print_results(&fm, &response, "list", n.unwrap_or(100));
            }
        }
        Commands::Tag { item, tag } => {
            fm.tag_item(item, tag)?;
            safe_println!("Tagged '{}' with '{}'", item, tag);
        }
        Commands::Clear => unreachable!("Handled early"),
        Commands::Note { content } => {
            let id = fm.add_item("note", content)?;
            safe_println!("Created note (ID: {})", id);
        }
        Commands::Rank { item, value } => {
            let response = fm.search(item, SearchOptions::default())?;
            if response.results.is_empty() {
                safe_println!("No items matched query: '{}'", item);
                return Ok(());
            }

            safe_println!("Matched {} items.", response.results.len());
            let do_update = if cli.yes {
                true
            } else {
                safe_print!("Set rank to {}? [y/N]: ", value);
                use std::io::{self, Write};
                std::io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                input.trim().to_lowercase() == "y"
            };

            if do_update {
                fm.update_ranks(&response.results, *value)?;
                safe_println!("Updated {} items.", response.results.len());
            } else {
                safe_println!("Aborted.");
            }
        }
    }

    Ok(())
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

/// 検索結果の一覧を標準出力に表示します。
fn print_results(
    fm: &FileManager,
    response: &ttfm::SearchResponse,
    query: &str,
    current_n: usize,
) {
    if !response.progress.is_finished() {
        safe_println!(
            "\x1b[1;33mSearching... (Background cache generating: {})\x1b[0m",
            response.progress.current
        );
    }

    if let Some(scalar) = response.scalar {
        safe_println!("{}", scalar);
        return;
    }

    if response.results.is_empty() {
        if response.progress.is_finished() {
            safe_println!("No items found.");
        }
        return;
    }

    // 投影 (Projection) がある場合はコンパクトな集約表示にする
    if response.type_for_projection.is_some() {
        print_compact_projections(response, query, current_n);
        return;
    }

    // データベースからタグ型のランクを取得
    let type_ranks = fm.get_type_ranks().unwrap_or_default();
    let term_width = get_terminal_width();

    // TypeGroup ごとに表示を行う
    for group in response.iter_type_groups() {
        // カラム（TagType）をランク順に並び替え
        let mut sorted_keys = group.keys.clone();
        sorted_keys.sort_by(|a, b| {
            let r_a = type_ranks
                .get(a.as_str())
                .cloned()
                .unwrap_or_else(|| fm.get_default_rank(a.as_str()));
            let r_b = type_ranks
                .get(b.as_str())
                .cloned()
                .unwrap_or_else(|| fm.get_default_rank(b.as_str()));
            r_b.cmp(&r_a).then_with(|| a.cmp(b))
        });

        // 「name」カラムがあれば先頭に持ってきたいが、BTreeSetソートにより元々順序はある。
        // ここでは純粋にランク順を優先する。

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

        // 行の出力ヘルパー
        let print_line = |res_opt: Option<&ttfm::SearchResult>| {
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
                safe_print!("\x1b[1m{}\x1b[0m", id_disp);
            } else {
                safe_print!("{}", id_disp);
            }
            current_width += id_disp.chars().count();

            // 各属性カラム
            for (i, key) in sorted_keys.iter().enumerate() {
                if current_width + sep_len >= term_width {
                    safe_print!("...");
                    break;
                }
                safe_print!("{}", sep);
                current_width += sep_len;

                let val_str = res_opt
                    .and_then(|r| r.get_tag_value(key.as_str()))
                    .unwrap_or_else(|| {
                        if is_header {
                            key.as_str().to_string()
                        } else {
                            "".to_string()
                        }
                    });

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
                    safe_print!("\x1b[1m{}\x1b[0m", out);
                } else {
                    safe_print!("{}", out);
                }
                current_width += out.chars().count();
            }
            safe_println!();
        };

        print_line(None); // Header
        for res in group.results {
            print_line(Some(res));
        }
        safe_println!();
    }

    safe_println!("Total: {} results displayed.", response.results.len());

    if response.has_more {
        if let Some(cid) = &response.cid {
            safe_println!(
                "\x1b[1;32mMore results available.\x1b[0m To see next page, run:"
            );
            safe_println!("  ttfm search \"{}\" --cid {}", query, cid);
        }
    }
}

/// 投影クエリの結果をラベルごとに集約してコンパクトに表示します。
fn print_compact_projections(
    response: &ttfm::SearchResponse,
    query: &str,
    _current_n: usize,
) {
    let term_width = get_terminal_width();

    // Phase 2: results には label items（転置）が格納されている
    for label_item in &response.results {
        // total_count を projected_label から取得
        let total_count = label_item
            .projected_label
            .as_ref()
            .and_then(|l| l.as_str().parse::<usize>().ok())
            .unwrap_or(label_item.tags.entries.len());

        // 1行目: ヘッダー (ラベル値 (X items))
        safe_println!(
            "\x1b[1;34m:{}\x1b[0m \x1b[2m({} items)\x1b[0m",
            label_item.name,
            total_count
        );

        // 2行目: アイテムリスト (tagsから抽出: item:name#id, ...)
        let mut all_items_str = String::new();
        for (i, tag_entry) in
            label_item.tags.entries.iter().take(200).enumerate()
        {
            if i > 0 {
                all_items_str.push_str(", ");
            }
            // タグは "item:name#id" 形式
            all_items_str.push_str(&tag_entry.label.as_str());
            if all_items_str.chars().count() > term_width + 10 {
                break;
            }
        }

        safe_println!(
            "  {}",
            truncate_text(&all_items_str, term_width.saturating_sub(2))
        );
    }

    safe_println!(
        "Total: {} unique labels matched the projection.",
        response.results.len()
    );

    if response.has_more {
        if let Some(cid) = &response.cid {
            safe_println!(
                "\n\x1b[1;32mMore items available.\x1b[0m To see next page, run:"
            );
            safe_println!("  ttfm search \"{}\" --cid {}", query, cid);
        }
    }
}

/// シンプルな形式（1行1アイテム、ヘッダーなし、色なし）で結果を出力します。
fn print_simple_results(response: &ttfm::SearchResponse) {
    if let Some(scalar) = response.scalar {
        safe_println!("{}", scalar);
        return;
    }

    if response.type_for_projection.is_none() {
        // プロジェクションなし: アイテムごとに解決済みの名前を出力
        for res in &response.results {
            let line = res.primary_value().unwrap_or_else(|| res.name.clone());
            safe_println!("{}", line);
        }
    } else {
        for label_item in &response.results {
            safe_println!("{}", label_item.name);
        }
    }
}
