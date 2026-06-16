//! TOML config parsing + window rule matching.
//!
//! The TOML schema is documented in `assets/config.toml`. Two pieces here:
//!
//! * `Config` — the deserialized shape of the file.
//! * `Rule` / `RuleAction` — runtime forms of `[[rules]]` entries, with
//!   compiled regex matchers.
//!
//! The keybinding section is parsed at a higher level (in `hotkey.rs`) where
//! VK / modifier names are known.

use color_eyre::Result;
use color_eyre::eyre::eyre;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: General,
    /// Raw "key combo string" → "action string" map.
    /// Parsed into hotkey table in `hotkey.rs`.
    pub keybindings: HashMap<String, String>,
    /// Raw rule entries — parse into `Rule` via `Self::compile_rules`.
    #[serde(rename = "rules", default)]
    pub raw_rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct General {
    pub gap: i32,
    pub outer_gap_top: i32,
    pub outer_gap_right: i32,
    pub outer_gap_bottom: i32,
    pub outer_gap_left: i32,
    pub workspace_count: u32,
    pub on_workspace_already_visible: AlreadyVisible,
    /// Color of the focused-window highlight overlay, as `"#RRGGBB"`.
    pub focused_border_color: String,
    /// Thickness in pixels of the focused-window highlight overlay.
    /// Set to 0 to disable the overlay entirely.
    pub border_width: i32,
    /// Inner corner radius of the focused-window highlight overlay, in
    /// pixels. Defaults to 8 — the same radius the Windows 11 DWM uses for
    /// rounded windows, so the overlay frame hugs each window's corner
    /// cleanly. Set to 0 for a sharp-cornered rectangular frame.
    pub corner_radius: i32,
    /// Show a thin status bar at the top of each monitor.
    pub bar_enabled: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            gap: 8,
            outer_gap_top: 0,
            outer_gap_right: 0,
            outer_gap_bottom: 0,
            outer_gap_left: 0,
            workspace_count: 5,
            on_workspace_already_visible: AlreadyVisible::Swap,
            focused_border_color: "#4FC3F7".to_string(),
            border_width: 3,
            corner_radius: dwmend_platform::focus_border::DEFAULT_RADIUS,
            bar_enabled: true,
        }
    }
}

/// Parse a `"#RRGGBB"` (or `"RRGGBB"`) string into a Win32 `COLORREF`
/// (`0x00BBGGRR`).
pub fn parse_border_color(s: &str) -> color_eyre::Result<u32> {
    let hex = s.trim().strip_prefix('#').unwrap_or_else(|| s.trim());
    if hex.len() != 6 {
        return Err(eyre!("border color `{s}` must be `#RRGGBB`"));
    }
    let r = u8::from_str_radix(&hex[0..2], 16).map_err(|e| eyre!("bad R in `{s}`: {e}"))?;
    let g = u8::from_str_radix(&hex[2..4], 16).map_err(|e| eyre!("bad G in `{s}`: {e}"))?;
    let b = u8::from_str_radix(&hex[4..6], 16).map_err(|e| eyre!("bad B in `{s}`: {e}"))?;
    Ok(dwmend_platform::dwm::rgb(r, g, b))
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum AlreadyVisible {
    FocusOtherMonitor,
    Swap,
}

impl From<AlreadyVisible> for crate::state::AlreadyVisibleBehaviour {
    fn from(v: AlreadyVisible) -> Self {
        match v {
            AlreadyVisible::FocusOtherMonitor => Self::FocusOtherMonitor,
            AlreadyVisible::Swap => Self::Swap,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RawRule {
    #[serde(rename = "match")]
    pub matcher: RuleMatcher,
    pub action: RuleAction,
}

#[derive(Debug, Deserialize, Default)]
pub struct RuleMatcher {
    /// Match against the executable's basename (case-insensitive equality).
    pub exe: Option<String>,
    /// Regex on the window class.
    pub class: Option<String>,
    /// Regex on the window title.
    pub title: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Ignore,
    Float,
    Tile,
}

/// A compiled rule — matchers as `Regex` for fast repeated evaluation.
#[derive(Debug, Clone)]
pub struct Rule {
    pub exe: Option<String>, // case-insensitive equality
    pub class_re: Option<Regex>,
    pub title_re: Option<Regex>,
    pub action: RuleAction,
}

impl Rule {
    pub fn matches(&self, exe: &str, class: &str, title: &str) -> bool {
        // At least one matcher must be present; if none, treat as never match.
        let mut any = false;
        if let Some(want) = &self.exe {
            any = true;
            if !want.eq_ignore_ascii_case(exe) {
                return false;
            }
        }
        if let Some(re) = &self.class_re {
            any = true;
            if !re.is_match(class) {
                return false;
            }
        }
        if let Some(re) = &self.title_re {
            any = true;
            if !re.is_match(title) {
                return false;
            }
        }
        any
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| eyre!("read config {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| eyre!("parse config {}: {e}", path.display()))?;
        Ok(cfg)
    }

    pub fn compile_rules(&self) -> Result<Vec<Rule>> {
        let mut out = Vec::with_capacity(self.raw_rules.len());
        for raw in &self.raw_rules {
            let class_re = raw
                .matcher
                .class
                .as_deref()
                .map(|s| Regex::new(s).map_err(|e| eyre!("bad class regex `{s}`: {e}")))
                .transpose()?;
            let title_re = raw
                .matcher
                .title
                .as_deref()
                .map(|s| Regex::new(s).map_err(|e| eyre!("bad title regex `{s}`: {e}")))
                .transpose()?;
            out.push(Rule {
                exe: raw.matcher.exe.clone(),
                class_re,
                title_re,
                action: raw.action,
            });
        }
        Ok(out)
    }
}

/// Resolve the canonical config path: `%APPDATA%\dwmend\config.toml`.
pub fn default_path() -> Result<std::path::PathBuf> {
    let appdata =
        dirs::config_dir().ok_or_else(|| eyre!("could not resolve %APPDATA% (config_dir)"))?;
    Ok(appdata.join("dwmend").join("config.toml"))
}

/// Resolve the data dir for logs / hwnd snapshots: `%LOCALAPPDATA%\dwmend`.
pub fn data_dir() -> Result<std::path::PathBuf> {
    let local = dirs::data_local_dir()
        .ok_or_else(|| eyre!("could not resolve %LOCALAPPDATA% (data_local_dir)"))?;
    Ok(local.join("dwmend"))
}

/// Write the embedded default config to `path` if it doesn't already exist.
/// Returns true if a file was placed at `path` by this call.
pub fn ensure_default(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(path, include_str!("../../../assets/config.toml"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_border_color_with_hash() {
        // #4FC3F7 (sky blue): R=0x4F G=0xC3 B=0xF7
        // COLORREF layout is 0x00BBGGRR.
        assert_eq!(parse_border_color("#4FC3F7").unwrap(), 0x00F7C34F);
    }

    #[test]
    fn parse_border_color_without_hash() {
        assert_eq!(parse_border_color("4FC3F7").unwrap(), 0x00F7C34F);
    }

    #[test]
    fn parse_border_color_pure_components() {
        assert_eq!(parse_border_color("#FF0000").unwrap(), 0x000000FF); // pure red
        assert_eq!(parse_border_color("#00FF00").unwrap(), 0x0000FF00); // pure green
        assert_eq!(parse_border_color("#0000FF").unwrap(), 0x00FF0000); // pure blue
        assert_eq!(parse_border_color("#000000").unwrap(), 0);
        assert_eq!(parse_border_color("#FFFFFF").unwrap(), 0x00FFFFFF);
    }

    #[test]
    fn parse_border_color_trims_whitespace() {
        assert_eq!(parse_border_color("  #4FC3F7  ").unwrap(), 0x00F7C34F);
    }

    #[test]
    fn parse_border_color_rejects_short_hex() {
        assert!(parse_border_color("#FFF").is_err());
        assert!(parse_border_color("").is_err());
    }

    #[test]
    fn parse_border_color_rejects_long_hex() {
        assert!(parse_border_color("#FFFFFFFF").is_err());
    }

    #[test]
    fn parse_border_color_rejects_non_hex() {
        assert!(parse_border_color("#GGGGGG").is_err());
        assert!(parse_border_color("#12345Z").is_err());
    }
}
