//! GUI preferences (per user), stored as JSON in %APPDATA%\quickseek\gui.json
//! (or `QUICKSEEK_GUI_DIR` for tests).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Global show/hide hotkey, e.g. `Ctrl+Alt+Space` (empty = none).
    pub hotkey: String,
    /// Closing the window keeps Quickseek in the tray.
    pub run_in_background: bool,
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub regex: bool,
    /// Column to sort by: name, path, size, modified, created, runs.
    pub sort: String,
    pub descending: bool,
    /// Index into the filter dropdown.
    pub filter: usize,
    /// x, y, width, height in pixels.
    pub window: Option<(i32, i32, i32, i32)>,
    pub column_widths: Vec<i32>,
    /// Folder-size tree instead of the result list.
    pub tree_mode: bool,
    /// Tree sort: name, size, files, modified.
    pub tree_sort: String,
    pub tree_descending: bool,
    pub tree_column_widths: Vec<i32>,
    /// The preview pane is open, and its width (96-DPI pixels).
    pub preview: bool,
    pub preview_width: i32,
    /// Content search (`content:`): skip files larger than this many MB.
    pub content_max_mb: u64,
    /// Content search: also search files that look binary.
    pub content_binary: bool,
    /// Content search: read Office documents and PDFs through their IFilter.
    pub content_ifilter: bool,
    /// View → Theme: system (Windows' app mode), light or dark.
    pub theme: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hotkey: "Ctrl+Alt+Space".into(),
            run_in_background: true,
            match_case: false,
            whole_word: false,
            match_path: false,
            regex: false,
            sort: "name".into(),
            descending: false,
            filter: 0,
            window: None,
            column_widths: Vec::new(),
            tree_mode: false,
            tree_sort: "size".into(),
            tree_descending: true,
            tree_column_widths: Vec::new(),
            preview: false,
            preview_width: 380,
            content_max_mb: 64,
            content_binary: false,
            content_ifilter: true,
            theme: "system".into(),
        }
    }
}

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("QUICKSEEK_GUI_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("quickseek")
}

impl Settings {
    pub fn load() -> Settings {
        std::fs::read_to_string(dir().join("gui.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let d = dir();
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(
            d.join("gui.json"),
            serde_json::to_string_pretty(self).unwrap(),
        );
    }
}

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

/// `Ctrl+Alt+Space` → (modifiers, virtual key).
pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut vk = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" => mods |= MOD_WIN,
            "space" => vk = Some(0x20),
            "tab" => vk = Some(0x09),
            "enter" => vk = Some(0x0D),
            p if p.len() == 1 && p.as_bytes()[0].is_ascii_alphanumeric() => {
                vk = Some(p.to_ascii_uppercase().as_bytes()[0] as u32)
            }
            p if p.starts_with('f')
                && p[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) =>
            {
                vk = Some(0x70 + p[1..].parse::<u32>().unwrap() - 1)
            }
            "" => {}
            _ => return None,
        }
    }
    let vk = vk?;
    (mods != 0 || (0x70..=0x87).contains(&vk)).then_some((mods, vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+Space"),
            Some((MOD_CONTROL | MOD_ALT, 0x20))
        );
        assert_eq!(
            parse_hotkey("win+shift+f"),
            Some((MOD_WIN | MOD_SHIFT, b'F' as u32))
        );
        assert_eq!(parse_hotkey("F11"), Some((0, 0x7A)));
        assert_eq!(parse_hotkey("a"), None, "a bare letter would steal typing");
        assert_eq!(parse_hotkey("Ctrl+Banana"), None);
        assert_eq!(parse_hotkey(""), None);
    }

    #[test]
    fn json_defaults() {
        let s: Settings = serde_json::from_str(r#"{"hotkey":"F12"}"#).unwrap();
        assert_eq!(s.hotkey, "F12");
        assert!(s.run_in_background);
    }
}
