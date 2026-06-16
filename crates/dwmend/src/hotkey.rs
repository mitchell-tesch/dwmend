//! Hotkey table & action-string parsing.
//!
//! Two domains live here:
//! * Parsing "SUPER+SHIFT+H" → `(Mods, VK)` plus "focus left" → `Command`.
//! * Building the `HotkeyTable` consumed by `dwmend_platform::keyboard::start`.

use crate::commands::Command;
use crate::ids::WorkspaceId;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use dwmend_platform::Direction;
use dwmend_platform::keyboard::{HotkeyId, HotkeyTable, Mods};
use std::collections::HashMap;

/// Owning side of the hotkey table: maps the integer `HotkeyId` produced by
/// the platform thread back to the `Command` the daemon should run.
///
/// The table is consumed by value at start-up. Config reload throws this
/// router away and constructs a new one — see `main.rs` for the dance.
pub struct HotkeyRouter {
    pub id_to_command: HashMap<HotkeyId, Command>,
    pub table: HotkeyTable,
}

impl HotkeyRouter {
    /// Build a router from the raw key→action strings. Returns the router
    /// alongside any parse errors (each error is a single binding that was
    /// skipped — the rest still register).
    pub fn build(bindings: &HashMap<String, String>) -> (Self, Vec<String>) {
        let mut table = HotkeyTable::new();
        let mut id_to_command: HashMap<HotkeyId, Command> = HashMap::new();
        let mut errors = Vec::new();
        let mut next_id: u32 = 1;

        for (combo, action) in bindings {
            let parsed = (|| {
                let (mods, vk) = parse_combo(combo)?;
                let cmd = parse_action(action)?;
                Ok::<_, color_eyre::Report>((mods, vk, cmd))
            })();
            match parsed {
                Ok((mods, vk, cmd)) => {
                    let id = HotkeyId(next_id);
                    next_id += 1;
                    table.insert((mods, vk), id);
                    id_to_command.insert(id, cmd);
                }
                Err(e) => errors.push(format!("{combo} = {action} → {e}")),
            }
        }

        (
            Self {
                id_to_command,
                table,
            },
            errors,
        )
    }

    pub fn command_for(&self, id: HotkeyId) -> Option<Command> {
        self.id_to_command.get(&id).cloned()
    }
}

// ---- combo parser ----------------------------------------------------------

/// Parse "MOD1+MOD2+...+KEY" → (Mods, VK). Case-insensitive.
pub fn parse_combo(s: &str) -> Result<(Mods, u16)> {
    let mut mods = Mods::empty();
    let mut vk: Option<u16> = None;
    for part in s.split('+').map(str::trim) {
        if part.is_empty() {
            return Err(eyre!("empty token in `{s}`"));
        }
        match part.to_ascii_uppercase().as_str() {
            "SUPER" | "WIN" | "META" => mods |= Mods::SUPER,
            "CTRL" | "CONTROL" => mods |= Mods::CTRL,
            "ALT" => mods |= Mods::ALT,
            "SHIFT" => mods |= Mods::SHIFT,
            key => {
                if vk.is_some() {
                    return Err(eyre!("more than one non-modifier key in `{s}`"));
                }
                vk = Some(name_to_vk(key).ok_or_else(|| eyre!("unknown key `{key}`"))?);
            }
        }
    }
    let vk = vk.ok_or_else(|| eyre!("no key found in `{s}`"))?;
    Ok((mods, vk))
}

/// Map a key NAME (already uppercased) to a Win32 VK code.
fn name_to_vk(name: &str) -> Option<u16> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let vk = match name {
        // Letters
        "A" => VK_A,
        "B" => VK_B,
        "C" => VK_C,
        "D" => VK_D,
        "E" => VK_E,
        "F" => VK_F,
        "G" => VK_G,
        "H" => VK_H,
        "I" => VK_I,
        "J" => VK_J,
        "K" => VK_K,
        "L" => VK_L,
        "M" => VK_M,
        "N" => VK_N,
        "O" => VK_O,
        "P" => VK_P,
        "Q" => VK_Q,
        "R" => VK_R,
        "S" => VK_S,
        "T" => VK_T,
        "U" => VK_U,
        "V" => VK_V,
        "W" => VK_W,
        "X" => VK_X,
        "Y" => VK_Y,
        "Z" => VK_Z,
        // Digits (top row)
        "0" => VK_0,
        "1" => VK_1,
        "2" => VK_2,
        "3" => VK_3,
        "4" => VK_4,
        "5" => VK_5,
        "6" => VK_6,
        "7" => VK_7,
        "8" => VK_8,
        "9" => VK_9,
        // Function keys
        "F1" => VK_F1,
        "F2" => VK_F2,
        "F3" => VK_F3,
        "F4" => VK_F4,
        "F5" => VK_F5,
        "F6" => VK_F6,
        "F7" => VK_F7,
        "F8" => VK_F8,
        "F9" => VK_F9,
        "F10" => VK_F10,
        "F11" => VK_F11,
        "F12" => VK_F12,
        // Arrows
        "LEFT" => VK_LEFT,
        "RIGHT" => VK_RIGHT,
        "UP" => VK_UP,
        "DOWN" => VK_DOWN,
        // Special
        "SPACE" => VK_SPACE,
        "ENTER" | "RETURN" => VK_RETURN,
        "TAB" => VK_TAB,
        "ESC" | "ESCAPE" => VK_ESCAPE,
        "BACKSPACE" => VK_BACK,
        "DELETE" | "DEL" => VK_DELETE,
        "HOME" => VK_HOME,
        "END" => VK_END,
        "PAGEUP" | "PGUP" => VK_PRIOR,
        "PAGEDOWN" | "PGDN" => VK_NEXT,
        // Punctuation (US layout)
        "COMMA" | "," => VK_OEM_COMMA,
        "PERIOD" | "." => VK_OEM_PERIOD,
        "MINUS" | "-" => VK_OEM_MINUS,
        "PLUS" | "+" | "=" => VK_OEM_PLUS,
        ";" => VK_OEM_1,
        "/" => VK_OEM_2,
        "`" => VK_OEM_3,
        "[" => VK_OEM_4,
        "\\" => VK_OEM_5,
        "]" => VK_OEM_6,
        "'" => VK_OEM_7,
        _ => return None,
    };
    Some(vk.0)
}

// ---- action parser ---------------------------------------------------------

pub fn parse_action(s: &str) -> Result<Command> {
    let mut parts = s.split_whitespace();
    let verb = parts.next().ok_or_else(|| eyre!("empty action"))?;
    match verb {
        "focus" => {
            let dir = parse_dir(parts.next())?;
            Ok(Command::FocusDirection(dir))
        }
        "move" => {
            let dir = parse_dir(parts.next())?;
            Ok(Command::MoveDirection(dir))
        }
        "resize" => {
            let dir = parse_dir(parts.next())?;
            let n: i32 = parts
                .next()
                .ok_or_else(|| eyre!("resize: missing delta"))?
                .parse()
                .map_err(|e| eyre!("resize: bad delta: {e}"))?;
            Ok(Command::Resize { dir, delta_px: n })
        }
        "workspace" => {
            let n: u32 = parts
                .next()
                .ok_or_else(|| eyre!("workspace: missing N"))?
                .parse()
                .map_err(|e| eyre!("workspace: bad N: {e}"))?;
            Ok(Command::SwitchWorkspace(WorkspaceId(n)))
        }
        "move_to_workspace" => {
            let n: u32 = parts
                .next()
                .ok_or_else(|| eyre!("move_to_workspace: missing N"))?
                .parse()
                .map_err(|e| eyre!("move_to_workspace: bad N: {e}"))?;
            Ok(Command::MoveFocusedToWorkspace(WorkspaceId(n)))
        }
        "focus_monitor" => {
            let dir = parse_dir(parts.next())?;
            Ok(Command::FocusMonitor(dir))
        }
        "toggle_float" => Ok(Command::ToggleFloat),
        "toggle_monocle" => Ok(Command::ToggleMonocle),
        "toggle_stack" => Ok(Command::ToggleStack),
        "stack_swallow" => {
            let dir = parse_dir(parts.next())?;
            Ok(Command::StackSwallow(dir))
        }
        "stack_pop" => Ok(Command::StackPop),
        "focus_stack_next" => Ok(Command::FocusStackNext),
        "focus_stack_prev" => Ok(Command::FocusStackPrev),
        "close" => Ok(Command::CloseFocused),
        "toggle_pause" => Ok(Command::TogglePause),
        "reload_config" => Ok(Command::ReloadConfig),
        "quit" => Ok(Command::Quit),
        other => Err(eyre!("unknown action `{other}`")),
    }
}

fn parse_dir(tok: Option<&str>) -> Result<Direction> {
    match tok.map(str::to_ascii_lowercase).as_deref() {
        Some("left") => Ok(Direction::Left),
        Some("right") => Ok(Direction::Right),
        Some("up") => Ok(Direction::Up),
        Some("down") => Ok(Direction::Down),
        Some(other) => Err(eyre!("unknown direction `{other}`")),
        None => Err(eyre!("missing direction")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F4, VK_H, VK_LEFT};

    #[test]
    fn parse_super_h() {
        let (m, vk) = parse_combo("SUPER+H").unwrap();
        assert!(m.contains(Mods::SUPER));
        assert_eq!(vk, VK_H.0);
    }

    #[test]
    fn parse_alt_shift_ctrl_combo() {
        let (m, vk) = parse_combo("ALT+SHIFT+CTRL+F4").unwrap();
        assert!(m.contains(Mods::ALT));
        assert!(m.contains(Mods::SHIFT));
        assert!(m.contains(Mods::CTRL));
        assert_eq!(vk, VK_F4.0);
    }

    #[test]
    fn parse_combo_is_case_insensitive() {
        let a = parse_combo("alt+h").unwrap();
        let b = parse_combo("ALT+H").unwrap();
        let c = parse_combo("Alt+h").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn parse_combo_aliases() {
        // SUPER == WIN == META, CTRL == CONTROL, ENTER == RETURN.
        let win = parse_combo("WIN+LEFT").unwrap();
        let meta = parse_combo("META+LEFT").unwrap();
        let sup = parse_combo("SUPER+LEFT").unwrap();
        assert_eq!(win, meta);
        assert_eq!(meta, sup);
        assert_eq!(sup.1, VK_LEFT.0);
        assert_eq!(
            parse_combo("CTRL+H").unwrap(),
            parse_combo("CONTROL+H").unwrap()
        );
    }

    #[test]
    fn parse_combo_rejects_empty_token() {
        assert!(parse_combo("ALT++H").is_err());
    }

    #[test]
    fn parse_combo_rejects_two_non_modifiers() {
        // "ALT+H+J" is ambiguous: which is the trigger key?
        assert!(parse_combo("ALT+H+J").is_err());
    }

    #[test]
    fn parse_combo_rejects_no_key() {
        // Modifiers only — no actual key to bind.
        assert!(parse_combo("ALT+SHIFT").is_err());
    }

    #[test]
    fn parse_combo_rejects_unknown_key() {
        assert!(parse_combo("ALT+BLARGH").is_err());
    }

    #[test]
    fn parse_workspace_action() {
        match parse_action("workspace 3").unwrap() {
            Command::SwitchWorkspace(WorkspaceId(3)) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_move_to_workspace_action() {
        match parse_action("move_to_workspace 10").unwrap() {
            Command::MoveFocusedToWorkspace(WorkspaceId(10)) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_resize_action() {
        match parse_action("resize left 64").unwrap() {
            Command::Resize {
                dir: Direction::Left,
                delta_px: 64,
            } => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_focus_monitor_action() {
        match parse_action("focus_monitor right").unwrap() {
            Command::FocusMonitor(Direction::Right) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_singleton_actions() {
        assert!(matches!(
            parse_action("toggle_float").unwrap(),
            Command::ToggleFloat
        ));
        assert!(matches!(
            parse_action("toggle_monocle").unwrap(),
            Command::ToggleMonocle
        ));
        assert!(matches!(
            parse_action("close").unwrap(),
            Command::CloseFocused
        ));
        assert!(matches!(
            parse_action("toggle_pause").unwrap(),
            Command::TogglePause
        ));
        assert!(matches!(parse_action("quit").unwrap(), Command::Quit));
        assert!(matches!(
            parse_action("reload_config").unwrap(),
            Command::ReloadConfig
        ));
    }

    #[test]
    fn parse_action_errors_on_empty() {
        assert!(parse_action("").is_err());
    }

    #[test]
    fn parse_action_errors_on_unknown_verb() {
        assert!(parse_action("teleport left").is_err());
    }

    #[test]
    fn parse_invalid_direction_errors() {
        assert!(parse_action("focus diagonal").is_err());
    }

    #[test]
    fn parse_resize_errors_on_missing_pixels() {
        assert!(parse_action("resize left").is_err());
    }

    #[test]
    fn parse_workspace_errors_on_missing_number() {
        assert!(parse_action("workspace").is_err());
    }
}
