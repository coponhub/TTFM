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

    // プラグインが有効な場合のみロード（ユーザー → ビルトインの順、同名はユーザーが優先）
    if config.plugins.enabled {
        let plugins_dir = ttfm::get_ttfm_plugins_dir()?;
        fm.load_plugins(plugins_dir, &config.plugins.status)?;
        fm.load_builtin_plugins(&config.plugins.status)?;
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
                print_results(&fm, &response, query, n.unwrap_or(100), &mut std::io::stdout());
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
                print_results(&fm, &response, "list", n.unwrap_or(100), &mut std::io::stdout());
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

fn print_warnings(warnings: &[String], writer: &mut dyn std::io::Write) {
    for w in warnings {
        writeln!(writer, "\x1b[1;33mWarning: {}\x1b[0m", w).unwrap_or(());
    }
}

/// 検索結果の一覧を標準出力に表示します。
fn print_results(
    fm: &FileManager,
    response: &ttfm::SearchResponse,
    query: &str,
    current_n: usize,
    writer: &mut dyn std::io::Write,
) {
    print_warnings(&response.warnings, writer);

    // 続きがある場合のみ、進捗状況（キャッシュ生成待ち）をチェックして表示
    if response.has_more && !response.progress.is_finished() {
        writeln!(
            writer,
            "\x1b[1;33mSearching... (Background cache generating: {})\x1b[0m",
            response.progress.current
        ).unwrap_or(());
    }

    if response.results.is_empty() {
        if response.progress.is_finished() {
            writeln!(writer, "No items found.").unwrap_or(());
        }
        return;
    }

    // item: タグが注入されている場合は Projection グループ表示
    if response.has_projection_results() {
        print_compact_projections(fm, response, query, current_n, writer);
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
            let mut print_line = |res_opt: Option<&ttfm::SearchResult>| {
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
                        res_opt.map(|_| "".to_string())
                            .unwrap_or_else(|| key.as_str().to_string())
                    } else {
                        res_opt
                            .and_then(|r| r.get_tag_value(key.as_str()))
                            .map(|raw| fm.format_tag_display(key.as_str(), &raw))
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

    writeln!(writer, "Total: {} results displayed.", response.results.len()).unwrap_or(());

    if response.has_more {
        if let Some(cid) = &response.cid {
            writeln!(
                writer,
                "\x1b[1;32mMore results available.\x1b[0m To see next page, run:"
            ).unwrap_or(());
            writeln!(writer, "  ttfm search \"{}\" --cid {}", query, cid).unwrap_or(());
        }
    }
}

/// 投影クエリの結果をラベルごとに集約してコンパクトに表示します。
fn print_compact_projections(
    fm: &FileManager,
    response: &ttfm::SearchResponse,
    query: &str,
    _current_n: usize,
    writer: &mut dyn std::io::Write,
) {
    let term_width = get_terminal_width();

    // クエリからプロジェクションタグ名を抽出して Display フォーマット適用に使う
    // "mtime:"                        → "mtime"
    // "parentdir: &: count(*:*) > 0" → "parentdir"  (Nest 左辺)
    // "extension:rs & size:"          → "size"       (AND 末尾の bare タグ)
    let proj_tag = {
        let before_comma = query.split(',').next().unwrap_or(query);
        // Nest (&:) の左辺のみ見る（右辺は nvalue 条件）
        let before_nest = before_comma.split("&:").next().unwrap_or(before_comma);
        // 空白区切りで "word:" 形式の bare タグを探し、最後のものをプロジェクションとする
        before_nest
            .split_whitespace()
            .filter(|s| {
                s.ends_with(':')
                    && s[..s.len() - 1].chars().all(|c| c.is_alphanumeric() || c == '_')
            })
            .last()
            .map(|s| &s[..s.len() - 1])
            .unwrap_or("")
    };

    // Phase 2: results には label items（転置）が格納されている
    for label_item in &response.results {
        // total_count を projected_label から取得
        let total_count = label_item
            .projected_label
            .as_ref()
            .and_then(|l| l.as_str().parse::<usize>().ok())
            .unwrap_or(label_item.tags.entries.len());

        // nvalue タグの取得（proj_tag と同じ Display を適用）
        let nvalue_str = label_item
            .tags
            .entries
            .iter()
            .find(|e| e.label.tag_type() == ttfm::TagType::from("nvalue"))
            .map(|e| fm.format_tag_display(proj_tag, e.label.as_str()));

        let formatted_name = fm.format_tag_display(proj_tag, &label_item.name);

        // 1行目: ヘッダー (ラベル値 - nvalue (X items))
        if let Some(nv) = &nvalue_str {
            writeln!(
                writer,
                "\x1b[1;34m:{}\x1b[0m - {} \x1b[2m({} items)\x1b[0m",
                formatted_name,
                nv,
                total_count
            ).unwrap_or(());
        } else {
            writeln!(
                writer,
                "\x1b[1;34m:{}\x1b[0m \x1b[2m({} items)\x1b[0m",
                formatted_name,
                total_count
            ).unwrap_or(());
        }

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

        writeln!(
            writer,
            "  {}",
            truncate_text(&all_items_str, term_width.saturating_sub(2))
        ).unwrap_or(());
    }

    writeln!(
        writer,
        "Total: {} unique labels matched the projection.",
        response.results.len()
    ).unwrap_or(());

    if response.has_more {
        if let Some(cid) = &response.cid {
            writeln!(
                writer,
                "\n\x1b[1;32mMore items available.\x1b[0m To see next page, run:"
            ).unwrap_or(());
            writeln!(writer, "  ttfm search \"{}\" --cid {}", query, cid).unwrap_or(());
        }
    }
}

/// シンプルな形式（1行1アイテム、ヘッダーなし、色なし）で結果を出力します。
fn print_simple_results(response: &ttfm::SearchResponse) {
    if response.has_projection_results() {
        for label_item in &response.results {
            safe_println!("{}", format_short_result(label_item));
        }
    } else {
        for res in &response.results {
            let line = res.primary_value().unwrap_or_else(|| res.name.clone());
            safe_println!("{}", line);
        }
    }
}

/// --short 時のアイテム表示に必要な文字列を生成します。
fn format_short_result(res: &ttfm::SearchResult) -> String {
    let nvalue_str = res
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());

    if let Some(nv) = nvalue_str {
        format!("{} {}", res.name, nv)
    } else {
        res.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use ttfm::types::{ItemId, ItemKind, Label, LabelValue, Origin, TagType};
    use ttfm::SearchResult;

    // COLUMNS 環境変数を操作するテストを直列化するための Mutex
    static COLUMNS_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_short_format_with_nvalue() {
        // projection パスでの生成プロセスに近い形でダミーデータを作成
        let mut res_with_nvalue = SearchResult::new_empty(
            ItemId::new_volatile(),
            ItemKind::Volatile,
            "test_label".to_string(),
        );
        res_with_nvalue.apply_tag(
            Label::resolve(TagType::from("nvalue"), LabelValue::Integer(9986)),
            Origin::System,
        );

        let output = format_short_result(&res_with_nvalue);
        assert_eq!(output, "test_label 9986");
    }

    #[test]
    fn test_short_format_without_nvalue() {
        // nvalueを持たないダミーダミーデータを作成
        let res_without_nvalue = SearchResult::new_empty(
            ItemId::new_volatile(),
            ItemKind::Volatile,
            "test_label_no_nv".to_string(),
        );

        let output = format_short_result(&res_without_nvalue);
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
        let fm = ttfm::FileManager::new_with_db_dir(&db_dir).unwrap();

        // 1024バイトのファイルを作成してインデックス
        let test_file = dir.path().join("sized.bin");
        std::fs::write(&test_file, vec![0u8; 1024]).unwrap();
        fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();

        let response = fm
            .search("name:sized.bin", ttfm::SearchOptions::default())
            .unwrap();
        assert!(!response.results.is_empty(), "ファイルがインデックスされていない");

        let mut out = Vec::<u8>::new();
        print_results(&fm, &response, "name:sized.bin", 100, &mut out);
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        // size は "1.0 KB" と表示され、生の "1024" は出てこない
        assert!(output.contains("1.0 KB"), "size should show '1.0 KB', got:\n{}", output);
    }

    #[test]
    fn test_print_results_formats_mtime_as_human_readable() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "500");
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        let fm = ttfm::FileManager::new_with_db_dir(&db_dir).unwrap();

        let test_file = dir.path().join("dated.txt");
        std::fs::write(&test_file, b"hi").unwrap();
        fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();

        let response = fm
            .search("name:dated.txt", ttfm::SearchOptions::default())
            .unwrap();
        assert!(!response.results.is_empty());

        let mut out = Vec::<u8>::new();
        print_results(&fm, &response, "name:dated.txt", 100, &mut out);
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        // mtime は "2026-05-..." のように年を含み、10桁のUnixタイムスタンプではない
        assert!(
            output.contains("2026") || output.contains("2025"),
            "mtime should show year, got:\n{}",
            output
        );
    }

    // --- print_compact_projections の display フォーマット確認 ---

    #[test]
    fn test_print_results_projection_formats_mtime() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "500");

        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        let fm = ttfm::FileManager::new_with_db_dir(&db_dir).unwrap();

        let test_file = dir.path().join("dated.txt");
        std::fs::write(&test_file, b"hi").unwrap();
        fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();

        let response = fm.search("mtime:", ttfm::SearchOptions::default()).unwrap();
        assert!(!response.results.is_empty());
        assert!(response.has_projection_results(), "mtime: should be a projection query");

        let mut out = Vec::<u8>::new();
        print_results(&fm, &response, "mtime:", 100, &mut out);
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        // projection ラベルは "2026-05-..." のように年を含み、10桁のタイムスタンプではない
        assert!(
            output.contains("2026") || output.contains("2025"),
            "projection mtime should show year, got:\n{}",
            output
        );
    }

    #[test]
    fn test_print_results_projection_formats_size() {
        let _guard = COLUMNS_MUTEX.lock().unwrap();
        std::env::set_var("COLUMNS", "500");

        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join("db");
        let fm = ttfm::FileManager::new_with_db_dir(&db_dir).unwrap();

        let test_file = dir.path().join("sized.bin");
        std::fs::write(&test_file, vec![0u8; 1024]).unwrap();
        fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();

        let response = fm.search("size:", ttfm::SearchOptions::default()).unwrap();
        assert!(!response.results.is_empty());
        assert!(response.has_projection_results(), "size: should be a projection query");

        let mut out = Vec::<u8>::new();
        print_results(&fm, &response, "size:", 100, &mut out);
        std::env::remove_var("COLUMNS");

        let output = String::from_utf8(out).unwrap();
        // projection ラベルは "1.0 KB" と表示され、生の "1024" は出てこない
        assert!(output.contains("1.0 KB"), "projection size should show '1.0 KB', got:\n{}", output);
    }

    #[test]
    fn test_print_warnings_outputs_warning_lines() {
        let warnings = vec![
            "Projection intersection ('&') found. Did you mean '&:' (Nest) to group results?".to_string(),
        ];
        let mut out = Vec::<u8>::new();
        print_warnings(&warnings, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(!text.is_empty(), "print_warnings should produce output");
        assert!(text.contains("&:"), "output should contain '&:' suggestion");
    }
}
