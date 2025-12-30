use serde::Deserialize;
use std::path::Path;
use std::fs;
use std::collections::HashMap;
use anyhow::Result;

/// TTFMの全体設定を保持する構造体。
#[derive(Debug, Deserialize)]
pub struct Config {
    /// プラグインに関する設定
    #[serde(default)]
    pub plugins: PluginsConfig,
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

fn default_true() -> bool { true }

impl Config {
    /// デフォルトの設定を生成します。
    pub fn new() -> Self {
        Self {
            plugins: PluginsConfig::default(),
        }
    }

    /// 指定されたパスまたは標準的な場所から設定ファイルを読み込みます。
    pub fn load() -> Self {
        // 1. カレントディレクトリの ttfm.toml を探す
        if let Ok(config) = Self::load_from_file("ttfm.toml") {
            return config;
        }

        // 2. ユーザー設定ディレクトリを探す (~/.config/ttfm/ttfm.toml)
        if let Some(mut config_path) = dirs::config_dir() {
            config_path.push("ttfm");
            config_path.push("ttfm.toml");
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
    }
}
