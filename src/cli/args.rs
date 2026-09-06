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

use crate::config::{
    Config, ConfirmMode, ConflictPolicy, HardlinkPolicy, SkipScope,
};
use crate::edit::WriteOptions;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// TTFM (Typed Tag File Manager) のメインCLI構造体。
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(arg_required_else_help = true)]
pub struct Cli {
    /// 対話型（REPL）セッションを起動します。初期クエリを任意で指定可能。
    #[arg(
        short = 'i',
        long = "interactive",
        num_args = 0..=1,
        value_name = "QUERY",
        help = "Launch interactive REPL session"
    )]
    pub interactive: Option<Option<String>>,

    /// 実行するサブコマンド
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// 全ての確認をスキップして 'yes' と回答します
    #[arg(short, long, global = true)]
    pub yes: bool,

    /// 進捗バーや詳細なメッセージの出力を抑制します
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// 確認プロンプトの動作 (auto, always, never)
    #[arg(long, global = true)]
    pub confirm: Option<ConfirmMode>,

    /// 移動先の重複・衝突時のポリシー (abort, skip, serial, first)
    #[arg(long, global = true)]
    pub on_conflict: Option<ConflictPolicy>,

    /// ハードリンク検出時のポリシー (abort, skip, all)
    #[arg(long, global = true)]
    pub on_hardlink: Option<HardlinkPolicy>,

    /// スキップ時の除外範囲 (item, fs-only)
    #[arg(long, global = true)]
    pub skip_scope: Option<SkipScope>,
}

/// TTFM で利用可能なサブコマンド。
#[derive(Subcommand)]
pub enum Commands {
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
        /// 付与するタグ（例: "project:A status:done"。DB登録は ""）
        edit_query: String,
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

pub fn build_write_options(cli: &Cli, config: &Config) -> WriteOptions {
    let confirm = if cli.yes {
        ConfirmMode::Never
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_parse_tag_requires_edit_query() {
        assert!(Cli::try_parse_from(["ttfm", "tag", "extension:txt"]).is_err());
        let parsed_empty =
            Cli::try_parse_from(["ttfm", "tag", "extension:txt", ""]).unwrap();
        match parsed_empty.command {
            Some(Commands::Tag {
                search_query,
                edit_query,
            }) => {
                assert_eq!(search_query, "extension:txt");
                assert_eq!(edit_query, "");
            }
            _ => panic!("expected Tag command"),
        }

        let parsed_with_tag =
            Cli::try_parse_from(["ttfm", "tag", "extension:txt", "project:a"])
                .unwrap();
        match parsed_with_tag.command {
            Some(Commands::Tag {
                search_query,
                edit_query,
            }) => {
                assert_eq!(search_query, "extension:txt");
                assert_eq!(edit_query, "project:a");
            }
            _ => panic!("expected Tag command"),
        }
    }

    #[test]
    fn test_cli_parse_interactive_flag() {
        let cli = Cli::try_parse_from(["ttfm", "-i"]).unwrap();
        assert_eq!(cli.interactive, Some(None));

        let cli_with_q = Cli::try_parse_from(["ttfm", "-i", "ext:rs"]).unwrap();
        assert_eq!(cli_with_q.interactive, Some(Some("ext:rs".to_string())));
    }
}
