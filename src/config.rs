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
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// 確認プロンプトの動作モード。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmMode {
    Auto,
    Always,
    Never,
}

/// 移動先の重複・衝突時の解決ポリシー。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    Abort,
    Skip,
    Serial,
    First,
}

/// ハードリンク（複数location）検出時の解決ポリシー。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HardlinkPolicy {
    Abort,
    Skip,
    All,
}

/// スキップ発生時の除外範囲。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SkipScope {
    Item,
    #[serde(rename = "fs_only")]
    #[value(name = "fs-only")]
    FsOnly,
}

/// 編集操作に関する設定。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EditConfig {
    #[serde(default = "default_confirm")]
    pub confirm: ConfirmMode,
    #[serde(default)]
    pub on_conflict: Option<ConflictPolicy>,
    #[serde(default)]
    pub on_hardlink: Option<HardlinkPolicy>,
    #[serde(default = "default_skip_scope")]
    pub skip_scope: SkipScope,
}

fn default_confirm() -> ConfirmMode {
    ConfirmMode::Auto
}

fn default_skip_scope() -> SkipScope {
    SkipScope::Item
}

impl Default for EditConfig {
    fn default() -> Self {
        Self {
            confirm: default_confirm(),
            on_conflict: None,
            on_hardlink: None,
            skip_scope: default_skip_scope(),
        }
    }
}

/// TTFMの全体設定を保持する構造体。
#[derive(Debug, Deserialize)]
pub struct Config {
    /// プラグインに関する設定
    #[serde(default)]
    pub plugins: PluginsConfig,

    /// 編集操作に関する設定
    #[serde(default)]
    pub edit: EditConfig,
}

/// プラグインに関する設定。
#[derive(Debug, Deserialize)]
pub struct PluginsConfig {
    /// Wasmプラグインをロードするかどうか
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 個別プラグインの有効/無効設定
    #[serde(default)]
    pub status: HashMap<String, bool>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            status: HashMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    /// デフォルトの設定を生成します。
    pub fn new() -> Self {
        Self {
            plugins: PluginsConfig::default(),
            edit: EditConfig::default(),
        }
    }

    /// 指定されたパスまたは標準的な場所から設定ファイルを読み込みます。
    pub fn load() -> Self {
        if let Ok(home) = crate::get_ttfm_home() {
            let config_path = home.join("ttfm.toml");
            if let Ok(config) = Self::load_from_file(&config_path) {
                return config;
            }
        }

        // ファイルが見つからない場合はデフォルト値を返す
        Self::new()
    }

    fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::new();
        assert!(config.plugins.enabled);
        assert!(config.plugins.status.is_empty());
    }

    #[test]
    fn test_parse_enabled_false() {
        let toml = r#"[plugins]
enabled = false"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.plugins.enabled);
    }

    #[test]
    fn test_parse_plugin_status() {
        let toml = r#"[plugins]
enabled = true

[plugins.status]
sample = false
mimetype = true"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.plugins.enabled);
        assert_eq!(config.plugins.status.get("sample"), Some(&false));
        assert_eq!(config.plugins.status.get("mimetype"), Some(&true));
    }

    #[test]
    fn test_empty_toml_uses_defaults() {
        let toml = "";
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.plugins.enabled);
        assert_eq!(config.edit.confirm, ConfirmMode::Auto);
        assert_eq!(config.edit.on_conflict, None);
        assert_eq!(config.edit.on_hardlink, None);
        assert_eq!(config.edit.skip_scope, SkipScope::Item);
    }

    #[test]
    fn test_parse_edit_config() {
        let toml = r#"[edit]
confirm = "never"
on_conflict = "serial"
on_hardlink = "all"
skip_scope = "fs_only""#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.edit.confirm, ConfirmMode::Never);
        assert_eq!(config.edit.on_conflict, Some(ConflictPolicy::Serial));
        assert_eq!(config.edit.on_hardlink, Some(HardlinkPolicy::All));
        assert_eq!(config.edit.skip_scope, SkipScope::FsOnly);
    }
}
