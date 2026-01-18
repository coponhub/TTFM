use anyhow::Result;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use terminal_size::{terminal_size, Width};
use ttfm::config::Config;
use ttfm::FileManager;

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
        /// 検索クエリ文字列。論理演算（&, |, -）や型付きタグ（extension:rs等）が使用可能です。
        query: String,
    },
    /// インデックスからファイルの一覧を表示します（最大100件）。
    List,
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
        println!("Database cleared successfully.");
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
            println!("Indexing directory: {:?} (dry-run: {})", path, dry_run);

            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .tick_chars("/|\\-")
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
        Commands::Search { query } => {
            println!("Searching for: '{}'", query);
            let response = fm.search(query)?;
            print_results(&fm, &response);
        }
        Commands::List => {
            println!("Listing files...");
            let response = fm.search("")?;
            print_results(&fm, &response);
        }
        Commands::Tag { item, tag } => {
            fm.tag_item(item, tag)?;
            println!("Tagged '{}' with '{}'", item, tag);
        }
        Commands::Clear => unreachable!("Handled early"),
        Commands::Note { content } => {
            let id = fm.add_item("note", content)?;
            println!("Created note (ID: {})", id);
        }
        Commands::Rank { item, value } => {
            let response = fm.search(item)?;
            if response.results.is_empty() {
                println!("No items matched query: '{}'", item);
                return Ok(());
            }

            println!("Matched {} items.", response.results.len());
            let do_update = if cli.yes {
                true
            } else {
                print!("Set rank to {}? [y/N]: ", value);
                use std::io::{self, Write};
                std::io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                input.trim().to_lowercase() == "y"
            };

            if do_update {
                fm.update_ranks(&response.results, *value)?;
                println!("Updated {} items.", response.results.len());
            } else {
                println!("Aborted.");
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

    if let Some((Width(w), _)) = terminal_size() {
        w as usize
    } else {
        100 // default fallback
    }
}

/// 検索結果の一覧を標準出力に表示します。
fn print_results(fm: &FileManager, response: &ttfm::types::SearchResponse) {
    let results = &response.results;
    if results.is_empty() {
        println!("No items found.");
        return;
    }

    // データベースからタグ型のランクを取得
    let type_ranks = fm.get_type_ranks().unwrap_or_default();

    struct DisplayRow {
        id: i64,
        columns: HashMap<String, String>,
        all_keys: Vec<String>,
    }

    let mut groups: HashMap<(String, Vec<String>), Vec<DisplayRow>> =
        HashMap::new();
    let term_width = get_terminal_width();

    for res in results {
        let mut row_data = HashMap::new();
        let mut keys = HashSet::new();

        // 解決済みの名前（最優先列）
        row_data.insert("name".to_string(), res.name.clone());
        keys.insert("name".to_string());

        // アイテムの種類
        row_data.insert("item_kind".to_string(), res.item_kind.clone());
        keys.insert("item_kind".to_string());

        // 全てのタグを表示対象に含める（Rankシステムに表示順序を委ねる）
        for (k, v) in &res.tags {
            // "name" や "item_kind" は既にセット済みなので、タグリストの中に現れてもスキップする
            if k == "name" || k == "item_kind" {
                continue;
            }
            row_data.insert(k.clone(), v.clone());
            keys.insert(k.clone());
        }

        // ランクに基づいてキーをソート
        let mut sorted_keys: Vec<String> = keys.iter().cloned().collect();
        sorted_keys.sort_by(|a, b| {
            // 投影対象の列があれば最優先
            let is_proj_a = response.projections.contains(a);
            let is_proj_b = response.projections.contains(b);

            if is_proj_a != is_proj_b {
                return if is_proj_a {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }

            let r_a = type_ranks
                .get(a)
                .cloned()
                .unwrap_or_else(|| fm.get_default_rank(a));
            let r_b = type_ranks
                .get(b)
                .cloned()
                .unwrap_or_else(|| fm.get_default_rank(b));
            r_b.cmp(&r_a).then_with(|| a.cmp(b))
        });

        // 優先表示される（ランクが高い）カラムをグループキーの識別に使う
        let priority_threshold = 1; // ランクが設定されているものを優先
        let priority_intersection: Vec<String> = sorted_keys
            .iter()
            .filter(|&k| {
                type_ranks.get(k).cloned().unwrap_or(0) >= priority_threshold
            })
            .cloned()
            .collect();

        let group_key = (res.item_kind.clone(), priority_intersection);

        groups.entry(group_key).or_default().push(DisplayRow {
            id: res.id,
            columns: row_data,
            all_keys: sorted_keys,
        });
    }

    let total_results = results.len();

    // グループごとに表示
    for ((_kind, _visible_keys), rows) in groups {
        let mut seen_keys = HashSet::new();
        let mut all_group_keys = Vec::new();

        for row in &rows {
            for k in &row.all_keys {
                if !seen_keys.contains(k) {
                    all_group_keys.push(k.clone());
                    seen_keys.insert(k.clone());
                }
            }
        }

        // グループ全体のカラムもランク順にソート
        all_group_keys.sort_by(|a, b| {
            // 投影対象の列があれば最優先
            let is_proj_a = response.projections.contains(a);
            let is_proj_b = response.projections.contains(b);

            if is_proj_a != is_proj_b {
                return if is_proj_a {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }

            let r_a = type_ranks
                .get(a)
                .cloned()
                .unwrap_or_else(|| fm.get_default_rank(a));
            let r_b = type_ranks
                .get(b)
                .cloned()
                .unwrap_or_else(|| fm.get_default_rank(b));
            r_b.cmp(&r_a).then_with(|| a.cmp(b))
        });

        let final_columns = all_group_keys;

        let mut item_id_width = 7; // "item_id"
        let mut col_widths = vec![0; final_columns.len()];

        for row in &rows {
            item_id_width = item_id_width.max(row.id.to_string().len());
            for (i, col_name) in final_columns.iter().enumerate() {
                let val_len = row
                    .columns
                    .get(col_name)
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                col_widths[i] = col_widths[i].max(val_len);
            }
        }
        for (i, col_name) in final_columns.iter().enumerate() {
            col_widths[i] = col_widths[i].max(col_name.len());
        }

        let print_line = |row_vals: Option<&DisplayRow>| {
            let mut current_width = 0;
            let sep = "  "; // 2 spaces
            let sep_len = sep.len();
            let is_header = row_vals.is_none();

            // item_id
            let id_str = if let Some(r) = row_vals {
                r.id.to_string()
            } else {
                "item_id".to_string()
            };
            let available_for_id = term_width.saturating_sub(current_width);
            if available_for_id == 0 {
                return;
            }

            let id_disp = if item_id_width <= available_for_id {
                format!("{:<width$}", id_str, width = item_id_width)
            } else {
                truncate_text(&id_str, available_for_id)
            };

            if is_header {
                print!("\x1b[1m{}\x1b[0m", id_disp);
            } else {
                print!("{}", id_disp);
            }
            current_width += id_disp.chars().count();

            for (i, col_name) in final_columns.iter().enumerate() {
                if current_width + sep_len >= term_width {
                    print!("...");
                    break;
                }
                print!("{}", sep);
                current_width += sep_len;

                let val_str = if let Some(r) = row_vals {
                    r.columns.get(col_name).map(|s| s.as_str()).unwrap_or("")
                } else {
                    col_name
                };

                let available = term_width.saturating_sub(current_width);
                if available == 0 {
                    break;
                }

                let target_width = col_widths[i];
                if target_width <= available {
                    let val_disp =
                        format!("{:<width$}", val_str, width = target_width);
                    if is_header {
                        print!("\x1b[1m{}\x1b[0m", val_disp);
                    } else {
                        print!("{}", val_disp);
                    }
                    current_width += target_width;
                } else {
                    let truncated = truncate_text(val_str, available);
                    if is_header {
                        print!("\x1b[1m{}\x1b[0m", truncated);
                    } else {
                        print!("{}", truncated);
                    }
                    break;
                }
            }
            println!();
        };

        print_line(None); // Header (Bold)
        for row in rows {
            print_line(Some(&row)); // Data
        }
        println!(); // 空行を追加
    }
    println!("\nTotal: {} results found.", total_results);
}
