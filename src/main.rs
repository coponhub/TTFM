use clap::{Parser, Subcommand};
use ttfm::FileManager;
use ttfm::config::Config;
use anyhow::Result;
use std::path::{Path, PathBuf};
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;
use std::collections::{HashMap, HashSet};
use terminal_size::{Width, terminal_size};

/// TTFM (Typed Tag File Manager) のメインCLI構造体。
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(arg_required_else_help = true)]
#[command(help_template = "Usage: file_manager <COMMAND>\n\n{after-help}\n\nOptions:\n{options}")]
#[command(after_help = "Commands:\n  index <PATH>      Index a directory recursively\n  search <QUERY>    Search for files\n  list              List all files (limited to 100)\n  clear             Clear the entire index")]
struct Cli {
    /// 実行するサブコマンド
    #[command(subcommand)]
    command: Commands,
}

/// TTFM で利用可能なサブコマンド。
#[derive(Subcommand)]
enum Commands {
    /// 指定されたディレクトリを再帰的にスキャンし、インデックスを作成します。
    #[command(hide = true)]
    Index {
        /// スキャンを開始するディレクトリパス（例: "." や "/home/user"）
        path: PathBuf,
        
        /// trueの場合、データベースへの書き込みやParquet保存を行わず、スキャン速度の計測のみを行います。
        #[arg(long)]
        dry_run: bool,
    },
    /// クエリを使用してファイルを検索します。
    #[command(hide = true)]
    Search {
        /// 検索クエリ文字列。論理演算（&, |, -）や型付きタグ（extension:rs等）が使用可能です。
        query: String,
    },
    /// インデックスからファイルの一覧を表示します（最大100件）。
    #[command(hide = true)]
    List,
    /// 作成されたインデックスファイルを削除します。
    #[command(hide = true)]
    Clear,
    /// アイテムにタグを付与します（例: ttfm tag "path/to/file" "project:ttfm"）。
    Tag {
        /// 対象のパスまたはID
        target: String,
        /// 付与するタグ (key:value)
        tag: String,
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
    let mut fm = FileManager::new()?;

    // 設定ファイルの読み込み
    let config = Config::load();

    // プラグインが有効な場合のみロード
    if config.plugins.enabled {
        fm.load_plugins("plugins", &config.plugins.status)?;
    }

    match &cli.command {
        Commands::Index { path, dry_run } => {
            println!("Indexing directory: {:?} (dry-run: {})", path, dry_run);
            
            let pb = ProgressBar::new_spinner();
            pb.set_style(ProgressStyle::default_spinner()
                .tick_chars("/|\\-")
                .template("{spinner:.green} {msg}")?);
            pb.set_message("Scanning...");
            pb.enable_steady_tick(Duration::from_millis(100));

            let count = fm.index_directory(path, Some(&|count| {
                pb.set_message(format!("Indexed {} files...", count));
            }), *dry_run)?;
            
            pb.finish_with_message(format!("Done! Successfully indexed {} files.", count));
        }
        Commands::Search { query } => {
            println!("Searching for: '{}'", query);
            let results = fm.search(query)?;
            print_results(&results);
        }
        Commands::List => {
            println!("Listing files...");
            let results = fm.search("")?;
            print_results(&results);
        }
        Commands::Clear => {
            fm.clear_index()?;
            println!("Index cleared.");
        }
        Commands::Tag { target, tag } => {
            fm.tag_item(target, tag)?;
            println!("Tagged '{}' with '{}'", target, tag);
        }
        Commands::Note { content } => {
            let id = fm.add_item("note", content)?;
            println!("Created note (ID: {})", id);
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
        format!("{}\
...", truncated)
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

/// パス文字列からファイル名部分を抽出します。
fn get_filename(path_str: &str) -> String {
    Path::new(path_str)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path_str)
        .to_string()
}

/// 検索結果の一覧を標準出力に表示します。
fn print_results(results: &[ttfm::types::SearchResult]) {
    if results.is_empty() {
        println!("No items found.");
        return;
    }

    struct DisplayRow {
        id: i64,
        columns: HashMap<String, String>,
    }

    let mut groups: HashMap<Vec<String>, Vec<DisplayRow>> = HashMap::new();

    for res in results {
        let mut row_data = HashMap::new();
        let mut keys = HashSet::new();

        if let Some(path) = res.get_tag_value("path") {
            row_data.insert("filename".to_string(), get_filename(path));
            keys.insert("filename".to_string());
        }

        row_data.insert("kind".to_string(), res.kind.clone());
        keys.insert("kind".to_string());

        for (k, v) in &res.tags {
            if k == "path" || k == "name" || k == "value" || k == "filename" { continue; }
            row_data.insert(k.clone(), v.clone());
            keys.insert(k.clone());
        }

        let mut sorted_keys: Vec<String> = Vec::new();
        let priority_cols = ["filename", "kind", "size_str", "modified_str", "parentdir", "content"];
        for col in &priority_cols {
            if keys.contains(*col) {
                sorted_keys.push(col.to_string());
                keys.remove(*col);
            }
        }
        let mut others: Vec<String> = keys.into_iter().collect();
        others.sort();
        sorted_keys.extend(others);

        groups.entry(sorted_keys).or_default().push(DisplayRow {
            id: res.id,
            columns: row_data,
        });
    }

    let total_results = results.len();
    let term_width = get_terminal_width();

    // グループごとに表示
    for (columns, rows) in groups {
        let mut id_width = 4; // "ID"
        let mut col_widths = vec![0; columns.len()];

        for row in &rows {
            id_width = id_width.max(row.id.to_string().len());
            for (i, col_name) in columns.iter().enumerate() {
                let val_len = row.columns.get(col_name).map(|s| s.chars().count()).unwrap_or(0);
                col_widths[i] = col_widths[i].max(val_len);
            }
        }
        for (i, col_name) in columns.iter().enumerate() {
            col_widths[i] = col_widths[i].max(col_name.len());
        }

        let print_line = |row_vals: Option<&DisplayRow>| {
            let mut current_width = 0;
            let sep = "  "; // 2 spaces
            let sep_len = sep.len();
            let is_header = row_vals.is_none();

            // ID
            let id_str = if let Some(r) = row_vals { r.id.to_string() } else { "ID".to_string() };
            let available_for_id = term_width.saturating_sub(current_width);
            if available_for_id == 0 { return; }
            
            let id_disp = if id_width <= available_for_id {
                format!("{:<width$}", id_str, width = id_width)
            } else {
                truncate_text(&id_str, available_for_id)
            };

            if is_header {
                print!("\x1b[1m{}\x1b[0m", id_disp);
            } else {
                print!("{}", id_disp);
            }
            current_width += id_disp.chars().count();

            for (i, col_name) in columns.iter().enumerate() {
                if current_width + sep_len >= term_width { break; }
                print!("{}", sep);
                current_width += sep_len;

                let val_str = if let Some(r) = row_vals {
                    r.columns.get(col_name).map(|s| s.as_str()).unwrap_or("")
                } else {
                    col_name
                };

                let available = term_width.saturating_sub(current_width);
                if available == 0 { break; }

                let target_width = col_widths[i];
                if target_width <= available {
                    let val_disp = format!("{:<width$}", val_str, width = target_width);
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