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
    }

    Ok(())
}

/// 検索結果のパス一覧を標準出力に表示します。
fn print_results(paths: &[String]) {
    if paths.is_empty() {
        println!("No files found.");
        return;
    }

    for path in paths {
        println!("{}", path);
    }
    println!("\nTotal: {} results found.", paths.len());
}