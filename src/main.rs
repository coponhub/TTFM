use clap::{Parser, Subcommand};
use ttfm::{FileManager, FileEntry};
use prettytable::{Table, Row, Cell, format};
use anyhow::Result;
use std::path::PathBuf;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(arg_required_else_help = true)]
#[command(help_template = "Usage: file_manager <COMMAND>\n\n{after-help}\n\nOptions:\n{options}")]
#[command(after_help = "Commands:\n  index <PATH>      Index a directory recursively\n  search <QUERY>    Search for files\n  list              List all files (limited to 100)\n  clear             Clear the entire index")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index a directory recursively
    #[command(hide = true)]
    Index {
        /// The directory path to start indexing from (e.g., "." or "/home/user")
        path: PathBuf,
        
        /// Perform a scan without writing to the database (for benchmarking)
        #[arg(long)]
        dry_run: bool,
    },
    /// Search for files
    #[command(hide = true)]
    Search {
        /// Query string to match against filenames or paths
        query: String,
    },
    /// List all files
    #[command(hide = true)]
    List,
    /// Clear the entire index
    #[command(hide = true)]
    Clear,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let fm = FileManager::new()?;

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
            print_table(&results);
        }
        Commands::List => {
            println!("Listing files...");
            let results = fm.search("")?; // Empty query returns list
            print_table(&results);
        }
        Commands::Clear => {
            fm.clear_index()?;
            println!("Index cleared.");
        }
    }

    Ok(())
}

fn print_table(entries: &[FileEntry]) {
    if entries.is_empty() {
        println!("No files found.");
        return;
    }

    let mut table = Table::new();
    table.set_format(*format::consts::FORMAT_NO_LINESEP_WITH_TITLE);
    table.set_titles(Row::new(vec![
        Cell::new("Name"),
        Cell::new("Type"),
        Cell::new("Size"),
        Cell::new("Modified"),
        Cell::new("Path"),
    ]));

    for entry in entries {
        table.add_row(Row::new(vec![
            Cell::new(&entry.name),
            Cell::new(&entry.kind),
            Cell::new(&entry.size),
            Cell::new(&entry.modified),
            Cell::new(&entry.path),
        ]));
    }

    table.printstd();
}
