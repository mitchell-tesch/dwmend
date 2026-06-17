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
    /// Status bar customisation: height, per-segment toggles, colours.
    /// All fields default to the v0.1 hard-coded values, so omitting
    /// the `[bar]` section keeps the previous look exactly.
    pub bar: Bar,
    /// Notification toast subsystem. Defaults match the built-in
    /// [`dwmend::ui::toast::ToastConfig::default`] so omitting this
    /// section keeps the previously hard-coded behaviour.
    pub notifications: Notifications,
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
    /// Initial BSP layout mode applied to every workspace at startup.
    /// Runtime swaps via the `toggle_layout` action are workspace-local
    /// and survive until the next config reload, which re-applies this
    /// value to every workspace.
    pub layout: LayoutKind,
}

/// TOML-facing layout selector. Mapped onto
/// [`dwmend_layout::bsp::LayoutMode`] in `daemon.rs` to keep the layout
/// crate's public surface free of `serde` dependencies.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LayoutKind {
    /// Aspect-ratio driven splits (default \u2014 i3 / Hyprland behaviour).
    #[default]
    Dwindle,
    /// Alternating axis splits (canonical BSP spiral).
    Spiral,
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
            layout: LayoutKind::Dwindle,
        }
    }
}

/// Status bar settings: dimensions, per-segment toggles, theme colours.
///
/// `height` is captured at startup; runtime changes require a daemon
/// restart because the bar reserves screen real-estate via the gaps
/// composer at process bring-up. Every other field is hot-reloadable
/// (colours via `bar::set_colors`, segment toggles via
/// `bar::set_segments`).
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct Bar {
    /// Bar height in pixels. Values <16 are clamped at startup so text
    /// stays readable.
    pub height: i32,
    /// Show the app icon at the very left of the bar.
    pub show_icon: bool,
    /// Show workspace pills.
    pub show_workspaces: bool,
    /// Show the focused window's title centred on the bar.
    pub show_focused_title: bool,
    /// Show the live clock at the right edge.
    pub show_clock: bool,
    /// Show the battery indicator (collapses on devices without one).
    pub show_battery: bool,
    /// Show the network indicator (collapses when no adapter is up).
    pub show_network: bool,
    /// Show the "PAUSED" right-edge indicator while DWMend is paused.
    pub show_pause_indicator: bool,
    /// Theme colours.
    pub colors: BarColorsConfig,
}

impl Default for Bar {
    fn default() -> Self {
        Self {
            // Height default mirrors `ui::bar::DEFAULT_HEIGHT`. Duplicated
            // as a literal here because `dwmend-layout` (which `config.rs`
            // could otherwise pull from) doesn't know about the bar, and
            // we'd rather avoid pulling the host crate into the platform
            // dep graph just to share one i32.
            height: 28,
            show_icon: true,
            show_workspaces: true,
            show_focused_title: true,
            show_clock: true,
            show_battery: true,
            show_network: true,
            show_pause_indicator: true,
            colors: BarColorsConfig::default(),
        }
    }
}

/// `[bar.colors]` sub-table. Each value is an `"#RRGGBB"` string parsed
/// via [`parse_border_color`] at apply time. The defaults reproduce the
/// pre-config-driven look exactly so omitting the section is a no-op.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct BarColorsConfig {
    /// Bar background fill.
    pub background: String,
    /// Default text colour for inactive workspace numbers, focused title,
    /// clock / battery / network glyphs.
    pub foreground: String,
    /// Active workspace pill fill.
    pub active_bg: String,
    /// Active workspace pill text.
    pub active_fg: String,
    /// 1-px outline drawn on a pill whose workspace is visible on a
    /// *different* monitor.
    pub visible_outline: String,
    /// Dimmed text used for empty inactive workspaces.
    pub dim_fg: String,
}

impl Default for BarColorsConfig {
    fn default() -> Self {
        Self {
            background: "#1E1E2E".to_string(),
            foreground: "#C0C0C0".to_string(),
            active_bg: "#4FC3F7".to_string(),
            active_fg: "#101018".to_string(),
            visible_outline: "#808080".to_string(),
            dim_fg: "#606060".to_string(),
        }
    }
}

/// Notification toast subsystem settings. Mirrors the runtime shape of
/// [`dwmend::ui::toast::ToastConfig`] in TOML so the daemon can hot-
/// reload the whole struct without restarting the listener thread.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct Notifications {
    /// Master switch. When `false`, every `notify` call (hotkey or
    /// IPC) becomes a silent no-op and existing log lines remain the
    /// only feedback. The listener thread stays alive so flipping
    /// this back to `true` via reload re-enables toasts immediately.
    pub enabled: bool,
    /// Hold-phase duration in milliseconds. The fade-in (~150 ms) and
    /// fade-out (~200 ms) are fixed and added on top, so a `ttl_ms`
    /// of 2200 gives a total visible lifetime of ~2.55 s.
    pub ttl_ms: u32,
    /// Maximum concurrent toasts on a single monitor. Beyond this,
    /// the oldest active toast is forced into fade-out.
    pub max_concurrent: u32,
    /// Stack anchor corner. Currently only `"top_right"` is wired up;
    /// other corners may be added without breaking older configs.
    pub anchor: NotificationAnchor,
    /// Severity colour palette.
    pub colors: NotificationColors,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_ms: 2200,
            max_concurrent: 3,
            anchor: NotificationAnchor::TopRight,
            colors: NotificationColors::default(),
        }
    }
}

/// TOML-facing toast anchor selector. Mapped onto
/// [`dwmend::ui::toast::ToastAnchor`] in `daemon.rs`.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAnchor {
    #[default]
    TopRight,
}

/// `[notifications.colors]` sub-table. Each value is a `"#RRGGBB"`
/// string parsed at apply time. The defaults give info=sky-blue,
/// warn=amber, error=red, all with text colours that contrast against
/// the fill.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct NotificationColors {
    pub info_bg: String,
    pub info_fg: String,
    pub warn_bg: String,
    pub warn_fg: String,
    pub error_bg: String,
    pub error_fg: String,
}

impl Default for NotificationColors {
    fn default() -> Self {
        Self {
            info_bg: "#4FC3F7".to_string(),
            info_fg: "#101018".to_string(),
            warn_bg: "#F9A825".to_string(),
            warn_fg: "#101018".to_string(),
            error_bg: "#E53935".to_string(),
            error_fg: "#FFFFFF".to_string(),
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
    pub action: RawRuleAction,
    /// Required when `action = "workspace"`; ignored otherwise. Resolved
    /// into [`RuleAction::Workspace`] by [`Config::compile_rules`].
    pub workspace: Option<u32>,
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

/// Wire-level action tag. Lives only as long as TOML deserialisation; the
/// host crate consumes the compiled [`RuleAction`] which folds the
/// `workspace` sibling field in for the `Workspace` variant.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawRuleAction {
    Ignore,
    Float,
    Tile,
    /// Pin the window to a specific workspace at admit time. The
    /// workspace number is supplied via the rule's sibling `workspace`
    /// key \u2014 see [`RawRule`].
    Workspace,
}

/// Resolved rule action consumed by `filter.rs` / `commands.rs`.
///
/// `Workspace(N)` carries the validated workspace number so admit-time
/// dispatch never has to revisit the raw TOML representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Ignore,
    Float,
    Tile,
    Workspace(u32),
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
            // Resolve the wire-level action tag into the runtime variant.
            // The `Workspace` tag REQUIRES the sibling `workspace` field;
            // a missing or zero value is a hard error so a config typo
            // doesn't silently fall back to "no rule".
            let action = match raw.action {
                RawRuleAction::Ignore => RuleAction::Ignore,
                RawRuleAction::Float => RuleAction::Float,
                RawRuleAction::Tile => RuleAction::Tile,
                RawRuleAction::Workspace => {
                    let n = raw.workspace.ok_or_else(|| {
                        eyre!(
                            "rule with action = \"workspace\" must set `workspace = N` \
                             (matcher: exe={:?} class={:?} title={:?})",
                            raw.matcher.exe,
                            raw.matcher.class,
                            raw.matcher.title
                        )
                    })?;
                    if n == 0 {
                        return Err(eyre!(
                            "rule workspace must be >= 1 (matcher: exe={:?} class={:?} title={:?})",
                            raw.matcher.exe,
                            raw.matcher.class,
                            raw.matcher.title
                        ));
                    }
                    RuleAction::Workspace(n)
                }
            };
            out.push(Rule {
                exe: raw.matcher.exe.clone(),
                class_re,
                title_re,
                action,
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
