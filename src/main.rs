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

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::time::Duration;
use ttfm::cli::args::{build_write_options, Cli, Commands};
use ttfm::cli::format::{
    format_tag_result, format_untag_result, print_results,
    print_simple_results, ColorWarningSink,
};
use ttfm::config::Config;
use ttfm::db::Store;
use ttfm::edit::{edit, QueryType};
use ttfm::safe_println;
use ttfm::tag::TagRegistry;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if ttfm::search::maybe_run_worker()? {
        return Ok(());
    }

    let cli = Cli::parse();
    run(cli)
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(Commands::Clear { all }) = cli.command {
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
    let plugins_dir = home.join("plugins");
    if !plugins_dir.exists() {
        std::fs::create_dir_all(&plugins_dir)?;
    }

    let mut registry = TagRegistry::with_standard();
    let config = Config::load();

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

    let write_opts = build_write_options(&cli, &config);

    if let Some(opt_query) = cli.interactive {
        let initial_query = opt_query.as_deref();
        let mut interactive_write_options = write_opts.clone();
        if !cli.yes && cli.confirm != Some(ttfm::config::ConfirmMode::Never) {
            interactive_write_options.confirm =
                ttfm::config::ConfirmMode::Always;
        }
        if std::io::stdin().is_terminal() {
            return ttfm::cli::interactive::runner::run_interactive_terminal(
                &store,
                &registry,
                &config,
                interactive_write_options,
                initial_query,
                cli.quiet,
            );
        } else {
            return ttfm::cli::interactive::runner::run_interactive_stream(
                &store,
                &registry,
                &config,
                interactive_write_options,
                initial_query,
                std::io::stdin(),
                &mut std::io::stdout(),
                &mut std::io::stderr(),
                cli.quiet,
            )
            .map_err(|e| e.into());
        }
    }

    match &cli.command {
        Some(cmd) => match cmd {
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

                let count = ttfm::indexing::Indexer::new(&store, &registry)
                    .run(
                        paths,
                        Some(&|count| {
                            pb.set_message(format!(
                                "Indexed {} files...",
                                count
                            ));
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
                        false,
                    );
                }
            }
            Commands::Tag {
                search_query,
                edit_query,
            } => {
                let edit_query_opt = if edit_query.is_empty() {
                    None
                } else {
                    Some(edit_query.as_str())
                };
                let mut stdout = std::io::stdout();
                let resp = edit(
                    &store,
                    &registry,
                    search_query,
                    edit_query_opt,
                    QueryType::Tag,
                    None,
                    write_opts,
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
                    write_opts,
                    &mut ColorWarningSink {
                        writer: &mut stdout,
                    },
                )?;
                safe_println!("{}", format_untag_result(&resp));
            }
            Commands::Replace { old: _, new_tag: _ } => {
                return Err("replace は未実装です".into());
            }
            Commands::Decal { from: _, to: _ } => {
                return Err("decal は未実装です".into());
            }
            Commands::Clear { .. } => unreachable!("Handled early"),
            Commands::Note { content } => {
                let id = ttfm::tagging::add_item(
                    &store, &registry, "note", content,
                )?;
                safe_println!("Created note (ID: {})", id);
            }
        },
        None => {
            safe_println!("No command specified. Run with --help for usage.");
        }
    }

    Ok(())
}
