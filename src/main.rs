use clap::{Parser, Subcommand};
use ttfm::FileManager;
use ttfm::config::Config;
use anyhow::Result;
use std::path::PathBuf;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

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
                .tick_chars("/|\\- ")
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

/// 検索結果の一覧を標準出力に表示します。
fn print_results(results: &[ttfm::types::SearchResult]) {
    if results.is_empty() {
        println!("No items found.");
        return;
    }

    for res in results {
        let primary = res.primary_value().unwrap_or("(no primary value)");
        println!("{} (ID: {}, Kind: {})", primary, res.id, res.kind);
        
        // タグの表示（間引きロジック）
        let mut shown_tags = 0;
        for (k, v) in &res.tags {
            if k == "path" || k == "content" || k == "name" || k == "value" { continue; } // メインで表示済み
            if shown_tags >= 5 {
                println!("  ... and more");
                break;
            }
            println!("  {}: {}", k, v);
            shown_tags += 1;
        }
        println!();
    }
    println!("Total: {} results found.", results.len());
}