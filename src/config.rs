use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// User configuration, loaded from `~/.config/mdview/config.toml` (or the
/// platform config dir). Every field has a default so a partial file is fine.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Maximum column paragraphs are reflowed to. The effective width is the
    /// smaller of this and the terminal width.
    pub wrap_width: usize,
    /// Syntect theme used for fenced code blocks.
    pub code_theme: String,
    /// How mermaid/latex blocks start out: "rendered" as diagrams, or as
    /// their "text" source (toggled in the pager with v).
    pub default_view: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wrap_width: 80,
            code_theme: "base16-ocean.dark".to_string(),
            default_view: "rendered".to_string(),
        }
    }
}

/// `$XDG_CONFIG_HOME/mdview/config.toml`, defaulting to `~/.config` — the
/// terminal-tool convention even on macOS (where `dirs::config_dir()` would
/// point at `~/Library/Application Support`).
pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .map(|d| d.join("mdview").join("config.toml"))
}

pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match toml::from_str(&contents) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("mdview: ignoring invalid {}: {err}", path.display());
            Config::default()
        }
    }
}

/// Written when `--config` is used before any config file exists.
pub const TEMPLATE: &str = r#"# mdview configuration

# Paragraphs reflow to at most this many columns (capped at the terminal width).
wrap_width = 80

# Theme for fenced code blocks. One of:
#   base16-ocean.dark, base16-eighties.dark, base16-mocha.dark,
#   base16-ocean.light, InspiredGitHub, Solarized (dark), Solarized (light)
code_theme = "base16-ocean.dark"

# How mermaid/latex blocks start out: "rendered" diagrams, or their "text"
# source (toggle with v).
default_view = "rendered"
"#;

/// Validates the config file, returning an error message if it won't load
/// cleanly (bad TOML, unknown keys, or an unknown theme).
pub fn check(path: &std::path::Path) -> Result<Config> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let cfg: Config = toml::from_str(&contents)?;
    if cfg.wrap_width < 20 {
        bail!("wrap_width must be at least 20");
    }
    if !["rendered", "text"].contains(&cfg.default_view.as_str()) {
        bail!("default_view must be \"rendered\" or \"text\"");
    }
    let themes = syntect::highlighting::ThemeSet::load_defaults().themes;
    if !themes.contains_key(&cfg.code_theme) {
        let known: Vec<&str> = themes.keys().map(String::as_str).collect();
        bail!(
            "unknown code_theme `{}`; available: {}",
            cfg.code_theme,
            known.join(", ")
        );
    }
    Ok(cfg)
}
