//! Host keybinding records and helpers for modifier bind layers.

use std::collections::HashMap;

/// Hyprland modmask bits.
pub const MASK_SHIFT: u8 = 1;
pub const MASK_CAPS: u8 = 2;
pub const MASK_CTRL: u8 = 4;
pub const MASK_ALT: u8 = 8;
pub const MASK_SUPER: u8 = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct Bind {
    pub mask: u8,
    pub key: String,
    pub description: String,
}

/// Parse `omarchy menu keybindings --print` output lines of the form
/// `SUPER SHIFT + RETURN → Browser` (mods space-separated, one `+` before
/// the key). Lines without an arrow, and binds on mouse buttons or switches,
/// are skipped.
pub fn parse_binds(text: &str) -> Vec<Bind> {
    let mut binds = Vec::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once('\u{2192}') else {
            continue;
        };
        let tokens: Vec<&str> = left
            .split_whitespace()
            .filter(|token| *token != "+")
            .collect();
        let Some((key, mods)) = tokens.split_last() else {
            continue;
        };
        // Live output mixes cases (`SUPER + Home`, `SUPER SHIFT + Delete`);
        // xkb names are case-insensitive, so normalize before matching.
        let key = key.to_ascii_uppercase();
        if key.is_empty() || key.starts_with("MOUSE:") || key.starts_with("SWITCH:") {
            continue;
        }
        let mut mask = 0u8;
        let mut known = true;
        for token in mods {
            match token.to_ascii_uppercase().as_str() {
                "SHIFT" => mask |= MASK_SHIFT,
                "CAPS" => mask |= MASK_CAPS,
                "CTRL" => mask |= MASK_CTRL,
                "ALT" => mask |= MASK_ALT,
                "SUPER" => mask |= MASK_SUPER,
                _ => {
                    known = false;
                    break;
                }
            }
        }
        if !known {
            continue;
        }
        binds.push(Bind {
            mask,
            key,
            description: right.trim().to_string(),
        });
    }
    binds
}

/// Resolve the live binding list, or `None` when omarchy is unavailable so
/// callers keep their last good list.
pub fn fetch_binds() -> Option<Vec<Bind>> {
    ensure_hyprland_signature();
    fetch_binds_with("omarchy")
}

/// Retry `fetch_binds` for a while after login (Hypr/omarchy may not be ready
/// when the systemd unit starts). Stops early once a non-empty list arrives.
pub fn fetch_binds_with_retry(attempts: u32, pause: std::time::Duration) -> Option<Vec<Bind>> {
    let mut last = None;
    for i in 0..attempts {
        ensure_hyprland_signature();
        match fetch_binds_with("omarchy") {
            Some(binds) if !binds.is_empty() => return Some(binds),
            Some(binds) => last = Some(binds),
            None => {}
        }
        if i + 1 < attempts {
            std::thread::sleep(pause);
        }
    }
    last
}

/// `omarchy menu keybindings` needs Hyprland's instance signature. Fill it from
/// `$XDG_RUNTIME_DIR/hypr/` when the daemon was started without the session env
/// (otherwise Super bind layers stay empty while Ctrl still works from the
/// built-in table).
pub fn ensure_hyprland_signature() {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return;
    }
    if let Some(name) = discover_hyprland_signature() {
        // Only called when unset; dodge + child `omarchy` both need the process env.
        unsafe { std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", name) };
    }
}

/// Newest Hyprland instance dir that has a live `.socket.sock`, if any.
fn discover_hyprland_signature() -> Option<std::ffi::OsString> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let hypr = std::path::Path::new(&runtime).join("hypr");
    let entries = std::fs::read_dir(&hypr).ok()?;
    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(".socket.sock").exists())
        .collect();
    // Prefer the socket that was modified most recently (current session).
    dirs.sort_by_key(|p| {
        std::fs::metadata(p.join(".socket.sock"))
            .and_then(|m| m.modified())
            .ok()
    });
    dirs.pop()
        .and_then(|p| p.file_name().map(|n| n.to_os_string()))
}

fn fetch_binds_with(command: &str) -> Option<Vec<Bind>> {
    let mut cmd = std::process::Command::new(command);
    cmd.args(["menu", "keybindings", "--print"]);
    // Prefer an explicit env on the child so bind-refresh threads need not race
    // on process-global set_var when the signature was still missing.
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        if let Some(sig) = discover_hyprland_signature() {
            cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
        }
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_binds(&String::from_utf8_lossy(&output.stdout)))
}

/// Hyprland modmask for QMK mod bits (ctrl 0x01, shift 0x02, alt 0x04,
/// GUI 0x08).
pub fn modmask_from_qmk_bits(bits: u8) -> u8 {
    let mut mask = 0u8;
    if bits & 0x01 != 0 {
        mask |= MASK_CTRL;
    }
    if bits & 0x02 != 0 {
        mask |= MASK_SHIFT;
    }
    if bits & 0x04 != 0 {
        mask |= MASK_ALT;
    }
    if bits & 0x08 != 0 {
        mask |= MASK_SUPER;
    }
    mask
}

/// QMK-to-Hyprland modmask for a set of pressed keycodes: standalone mods
/// (0xE0-0xE7) and mod-tap keys (0x2000-0x3FFF).
pub fn modmask_from_keycodes(codes: &[u16]) -> u8 {
    let mut mask = 0u8;
    for &kc in codes {
        let qmk_bit = match kc {
            0x00E0..=0x00E7 => 1u8 << ((kc - 0x00E0) % 4),
            0x2000..=0x3FFF => ((kc >> 8) as u8) & 0x0F,
            _ => 0,
        };
        mask |= modmask_from_qmk_bits(qmk_bit);
    }
    mask
}

pub fn header_for(mask: u8) -> String {
    let parts: Vec<&str> = [
        (MASK_SUPER, "SUPER"),
        (MASK_CTRL, "CTRL"),
        (MASK_ALT, "ALT"),
        (MASK_SHIFT, "SHIFT"),
    ]
    .iter()
    .filter(|(bit, _)| mask & bit != 0)
    .map(|(_, name)| *name)
    .collect();
    parts.join(" + ")
}

/// QMK keycode -> Hyprland key name, for board keys that can carry a host
/// bind. Mod-tap and layer-tap keys resolve through their tap keycode.
pub fn hypr_name(kc: u16) -> Option<String> {
    let basic = match kc {
        0x2000..=0x3FFF | 0x4000..=0x4FFF => kc & 0xFF,
        0x0000..=0x00FF => kc,
        _ => return None,
    };
    let name = match basic {
        0x04..=0x1D => (b'A' + (basic as u8 - 0x04)) as char,
        0x1E..=0x27 => (b'1' + (basic as u8 - 0x1E)) as char,
        _ => return basic_name(basic as u8),
    };
    Some(name.to_string())
}

fn basic_name(kc: u8) -> Option<String> {
    let name = match kc {
        0x28 => "RETURN",
        0x29 => "ESCAPE",
        0x2A => "BACKSPACE",
        0x2B => "TAB",
        0x2C => "SPACE",
        0x2D => "MINUS",
        0x2E => "EQUAL",
        0x2F => "BRACKETLEFT",
        0x30 => "BRACKETRIGHT",
        0x31 => "BACKSLASH",
        0x33 => "SEMICOLON",
        0x34 => "APOSTROPHE",
        0x35 => "GRAVE",
        0x36 => "COMMA",
        0x37 => "PERIOD",
        0x38 => "SLASH",
        0x39 => "CAPSLOCK",
        0x3A..=0x45 => return Some(format!("F{}", kc - 0x39)),
        0x46 => "PRINT",
        0x47 => "SCROLL_LOCK",
        0x48 => "PAUSE",
        0x49 => "INSERT",
        0x4A => "HOME",
        0x4B => "PAGE_UP",
        0x4C => "DELETE",
        0x4D => "END",
        0x4E => "PAGE_DOWN",
        0x65 => "MENU",
        0x4F => "RIGHT",
        0x50 => "LEFT",
        0x51 => "DOWN",
        0x52 => "UP",
        0xA8 => "XF86AudioMute",
        0xA9 => "XF86AudioRaiseVolume",
        0xAA => "XF86AudioLowerVolume",
        0xAB => "XF86AudioNext",
        0xAC => "XF86AudioPrev",
        0xAD => "XF86AudioStop",
        0xAE => "XF86AudioPlay",
        0xAF => "XF86AudioMedia",
        0xBD => "XF86MonBrightnessUp",
        0xBE => "XF86MonBrightnessDown",
        _ => return None,
    };
    Some(name.to_string())
}

/// Key name -> description for one exact modmask, with Hyprland binds
/// shadowing the built-in standard tables on the same key when `standard`
/// is set.
pub fn binds_for_mask(binds: &[Bind], mask: u8, standard: bool) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for bind in binds.iter().filter(|b| b.mask == mask) {
        map.insert(bind.key.clone(), bind.description.clone());
    }
    let table: &[(&str, &str)] = match mask {
        MASK_CTRL => STANDARD_CTRL,
        MASK_ALT => STANDARD_ALT,
        _ => &[],
    };
    if standard {
        for (key, description) in table {
            if action_covered(binds, key, description) {
                continue;
            }
            map.entry(key.to_string())
                .or_insert_with(|| description.to_string());
        }
    }
    map
}

/// Whether a real bind already performs `action` on `key`. Omarchy names its
/// Ctrl passthroughs "Universal <action>" (they literally send Ctrl+key), so
/// the prefix is stripped before comparing. Keeps built-in hints from
/// suggesting a shortcut the host already binds.
fn action_covered(binds: &[Bind], key: &str, action: &str) -> bool {
    binds.iter().any(|bind| {
        if !bind.key.eq_ignore_ascii_case(key) {
            return false;
        }
        let description = bind.description.trim();
        let name = match description.get(..10) {
            Some(head) if head.eq_ignore_ascii_case("universal ") => &description[10..],
            _ => description,
        };
        same_action(name, action)
    })
}

/// Whether a bind action and a hint action mean the same thing. Omarchy-style
/// binds forward another chord, so their names can differ from the hint's.
fn same_action(name: &str, action: &str) -> bool {
    if name.eq_ignore_ascii_case(action) {
        return true;
    }
    const SYNONYMS: &[(&str, &str)] = &[("Refresh", "Reload")];
    SYNONYMS.iter().any(|(a, b)| {
        (name.eq_ignore_ascii_case(a) && action.eq_ignore_ascii_case(b))
            || (name.eq_ignore_ascii_case(b) && action.eq_ignore_ascii_case(a))
    })
}

const STANDARD_CTRL: &[(&str, &str)] = &[
    ("C", "Copy"),
    ("X", "Cut"),
    ("V", "Paste"),
    ("Z", "Undo"),
    ("Y", "Redo"),
    ("A", "Select all"),
    ("F", "Find"),
    ("S", "Save"),
    ("N", "New"),
    ("T", "New tab"),
    ("W", "Close tab"),
    ("R", "Reload"),
];

const STANDARD_ALT: &[(&str, &str)] = &[("F4", "Close window")];

/// What a key would type while Shift is held, for keys without their own
/// Shift binding. Mod-tap and layer-tap keys resolve through their tap
/// keycode.
pub fn shift_label(kc: u16) -> Option<String> {
    let basic = match kc {
        0x2000..=0x3FFF | 0x4000..=0x4FFF => (kc & 0xFF) as u8,
        0x0000..=0x00FF => kc as u8,
        _ => return None,
    };
    match basic {
        0x04..=0x1D => Some(((b'A' + (basic - 0x04)) as char).to_string()),
        0x1E..=0x27 | 0x2D..=0x38 => {
            crate::board::labels::shifted_symbol(basic).map(str::to_string)
        }
        _ => None,
    }
}

/// Glyph for a bind description: workspace rules first (an icon plus the full
/// workspace number, so a bind never looks like a plain typed digit), then the
/// exact table, then family rules below so new Omarchy descriptions keep
/// matching without new table rows.
pub fn glyph_for(description: &str) -> Option<String> {
    let lowered = description.to_lowercase();
    if let Some(rest) = lowered.strip_prefix("switch to workspace ") {
        return workspace_glyph("\u{f0a9}", rest);
    }
    if let Some(rest) = lowered.strip_prefix("move window to workspace ") {
        return workspace_glyph("\u{f047}", rest);
    }
    if let Some(rest) = lowered.strip_prefix("move window silently to workspace ") {
        return workspace_glyph("\u{f047}", rest);
    }
    if let Some((_, glyph)) = GLYPHS.iter().find(|(mark, _)| *mark == lowered) {
        return Some((*glyph).to_string());
    }
    if let Some((head, _)) = lowered.split_once(" (") {
        if let Some((_, glyph)) = GLYPHS.iter().find(|(mark, _)| *mark == head) {
            return Some((*glyph).to_string());
        }
    }
    if let Some(rest) = lowered.strip_prefix("universal ") {
        if let Some((_, glyph)) = GLYPHS.iter().find(|(mark, _)| *mark == rest) {
            return Some((*glyph).to_string());
        }
    }
    if lowered.starts_with("expand window") {
        return Some("\u{f065}".to_string());
    }
    if lowered.starts_with("shrink window") {
        return Some("\u{f066}".to_string());
    }
    if lowered.starts_with("move window to group") {
        return Some("\u{f047}".to_string());
    }
    if lowered.starts_with("switch to group window") {
        return Some("\u{f247}".to_string());
    }
    if lowered.starts_with("swap window") {
        return Some("\u{f0ec}".to_string());
    }
    if lowered.contains("monitor") {
        return Some("\u{f108}".to_string());
    }
    if lowered.contains("notification") {
        if lowered.contains("dismiss") || lowered.contains("silencing") {
            return Some("\u{f1f6}".to_string());
        }
        return Some("\u{f0f3}".to_string());
    }
    None
}

/// Icon plus workspace number for workspace binds. The number is kept a
/// space away: Nerd Font icon ink is wider than its monospace advance, so a
/// digit drawn right after the icon would overlap it.
fn workspace_glyph(icon: &str, number: &str) -> Option<String> {
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("{icon} {number}"))
}

/// First letter of the description, for binds with no table glyph.
pub fn fallback_glyph(description: &str) -> String {
    description
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default()
}

const GLYPHS: &[(&str, &str)] = &[
    ("terminal", "\u{f120}"),
    ("browser", "\u{f0ac}"),
    ("browser (private)", "\u{f0ac}"),
    ("file manager", "\u{f07b}"),
    ("file manager (cwd)", "\u{f07b}"),
    ("editor", "\u{f044}"),
    ("tmux", "\u{f120}"),
    ("herdr", "\u{f120}"),
    ("music", "\u{f001}"),
    ("music tui", "\u{f001}"),
    ("docker", "\u{f1b2}"),
    ("signal", "\u{f1d8}"),
    ("obsidian", "\u{f15c}"),
    ("omawrite", "\u{f044}"),
    ("passwords", "\u{f084}"),
    ("chatgpt", "\u{f0d0}"),
    ("grok", "\u{f0d0}"),
    ("agent", "\u{f0d0}"),
    ("calendar", "\u{f073}"),
    ("email", "\u{f0e0}"),
    ("new email", "\u{f0e0}"),
    ("youtube", "\u{f167}"),
    ("whatsapp", "\u{f232}"),
    ("google messages", "\u{f086}"),
    ("google photos", "\u{f03e}"),
    ("google maps", "\u{f279}"),
    ("x", "\u{f099}"),
    ("x post", "\u{f099}"),
    ("close window", "\u{f2d3}"),
    ("close all windows", "\u{f2d3}"),
    ("toggle window split", "\u{f0db}"),
    ("toggle window floating/tiling", "\u{f24d}"),
    ("full screen", "\u{f065}"),
    ("full width", "\u{f07e}"),
    ("tiled full screen", "\u{f065}"),
    ("pop window out", "\u{f08d}"),
    ("resize window", "\u{f047}"),
    ("move window to scratchpad", "\u{f187}"),
    ("focus on left window", "\u{f060} \u{f2d2}"),
    ("focus on right window", "\u{f061} \u{f2d2}"),
    ("focus on above window", "\u{f062} \u{f2d2}"),
    ("focus on below window", "\u{f063} \u{f2d2}"),
    ("focus on next window", "\u{f0a9}"),
    ("swap window left", "\u{f0ec}"),
    ("swap window right", "\u{f0ec}"),
    ("swap window up", "\u{f0ec}"),
    ("swap window down", "\u{f0ec}"),
    ("move window to group left", "\u{f047}"),
    ("move window to group right", "\u{f047}"),
    ("move window to group top", "\u{f047}"),
    ("move window to group bottom", "\u{f047}"),
    ("move active window out of group", "\u{f047}"),
    ("move grouped window focus left", "\u{f060}"),
    ("move grouped window focus right", "\u{f061}"),
    ("move workspace to left monitor", "\u{f108}"),
    ("move workspace to right monitor", "\u{f108}"),
    ("move workspace to up monitor", "\u{f108}"),
    ("move workspace to down monitor", "\u{f108}"),
    ("next workspace", "\u{f0a9}"),
    ("previous workspace", "\u{f0a8}"),
    ("omarchy menu", "\u{f0c9}"),
    ("keybindings", "\u{f11c}"),
    ("tmux keybindings", "\u{f120}"),
    ("herdr keybindings", "\u{f120}"),
    ("calculator", "\u{f1ec}"),
    ("screenshot", "\u{f030}"),
    ("color picker", "\u{f1fb}"),
    ("extract text (ocr) from screenshot", "\u{f031}"),
    ("screenrecording", "\u{f03d}"),
    ("clipboard manager", "\u{f0ea}"),
    ("lock system", "\u{f023}"),
    ("toggle dictation", "\u{f130}"),
    ("start dictation", "\u{f130}"),
    ("stop dictation", "\u{f130}"),
    ("toggle top bar", "\u{f2d0}"),
    ("theme menu", "\u{f186}"),
    ("dismiss all notifications", "\u{f1f6}"),
    ("select all", "\u{f14a}"),
    ("copy", "\u{f0c5}"),
    ("cut", "\u{f0c4}"),
    ("paste", "\u{f0ea}"),
    ("undo", "\u{f0e2}"),
    ("redo", "\u{f01e}"),
    ("find", "\u{f002}"),
    ("save", "\u{f0c7}"),
    ("new", "\u{f15b}"),
    ("new tab", "\u{f196}"),
    ("close tab", "\u{f00d}"),
    ("reload", "\u{f021}"),
    ("mute", "\u{f026}"),
    ("volume up", "\u{f028}"),
    ("volume down", "\u{f027}"),
    ("mute microphone", "\u{f131}"),
    ("play", "\u{f04b}"),
    ("next", "\u{f04e}"),
    ("previous", "\u{f04a}"),
    ("touchpad", "\u{f0a6}"),
    ("keyboard backlight cycle", "\u{f11c}"),
    ("brightness", "\u{f185}"),
    ("system menu", "\u{f013}"),
    ("emojis", "\u{f118}"),
    ("continue omapilot chat in herdr", "\u{f075}"),
    ("new omapilot chat", "\u{f075}"),
    ("talk to omapilot", "\u{f075}"),
    ("former workspace", "\u{f0a8}"),
    ("focus on previous window", "\u{f0a8}"),
    ("next window in group", "\u{f0a9}"),
    ("previous window in group", "\u{f0a8}"),
    ("reveal active window on top", "\u{f062}"),
    ("toggle scratchpad", "\u{f187}"),
    ("toggle window transparency", "\u{f06e}"),
    ("toggle nightlight", "\u{f186}"),
    ("toggle locking on idle", "\u{f023}"),
    ("toggle window gaps", "\u{f07e}"),
    ("toggle workspace layout", "\u{f009}"),
    ("toggle single-window square aspect", "\u{f096}"),
    ("toggle window grouping", "\u{f247}"),
    ("pseudo window", "\u{f0c8}"),
    ("download video from web app", "\u{f019}"),
    ("copy url from web app", "\u{f0c1}"),
    ("make webcam overlay smaller", "\u{f030}"),
    ("make webcam overlay larger", "\u{f030}"),
    ("omakeys overlay", "\u{f11c}"),
    ("apps menu", "\u{f0c9}"),
    ("toggle menu", "\u{f0c9}"),
    ("cancel omapilot voice", "\u{f00d}"),
    ("audio", "\u{f028}"),
    ("show battery remaining", "\u{f240}"),
    ("show reminders", "\u{f073}"),
    ("set reminder", "\u{f073}"),
    ("clear reminders", "\u{f1f8}"),
    ("show time", "\u{f017}"),
    ("toggle weather", "\u{f0c2}"),
    ("reset zoom", "\u{f002}"),
    ("zoom in", "\u{f00e}"),
    ("bluetooth", "\u{f293}"),
    ("capture menu", "\u{f030}"),
    ("display", "\u{f108}"),
    ("hardware menu", "\u{f085}"),
    ("activity", "\u{f201}"),
    ("transcode", "\u{f008}"),
    ("power", "\u{f011}"),
    ("background switcher", "\u{f03e}"),
    ("share", "\u{f064}"),
    ("restore last tab", "\u{f0e2}"),
    ("network", "\u{f1eb}"),
    ("refresh", "\u{f021}"),
    ("next track", "\u{f04e}"),
    ("previous track", "\u{f04a}"),
    ("switch audio output", "\u{f028}"),
    ("switch media source", "\u{f04b}"),
    ("volume up precise", "\u{f028}"),
    ("volume down precise", "\u{f027}"),
    ("brightness up precise", "\u{f185}"),
    ("brightness down precise", "\u{f185}"),
    ("brightness minimum", "\u{f185}"),
    ("brightness maximum", "\u{f185}"),
    ("brightness down", "\u{f185}"),
    ("brightness up", "\u{f185}"),
    ("apps", "\u{f0c9}"),
    ("cycle workspace layout", "\u{f009}"),
    ("cycle workspace layout back", "\u{f009}"),
    ("direct input for the terminal prefix", "\u{f120}"),
    ("disable touchpad", "\u{f0a6}"),
    ("enable touchpad", "\u{f0a6}"),
    ("toggle touchpad", "\u{f0a6}"),
    ("dismiss", "\u{f1f6}"),
    ("eject media", "\u{f052}"),
    ("keyboard brightness down", "\u{f11c}"),
    ("keyboard brightness up", "\u{f11c}"),
    ("minimize window to the dock", "\u{f2d1}"),
    ("move window to quake console", "\u{f120}"),
    ("toggle quake console", "\u{f120}"),
    ("opensidian", "\u{f15c}"),
    ("pause", "\u{f04c}"),
    ("power menu", "\u{f011}"),
    ("read aloud faster", "\u{f04e}"),
    ("read aloud slower", "\u{f04a}"),
    ("read selection aloud", "\u{f028}"),
    ("record with omareel", "\u{f03d}"),
    ("restore window width", "\u{f07e}"),
    ("save window width", "\u{f07e}"),
    ("scroll active workspace backward", "\u{f0a8}"),
    ("scroll active workspace forward", "\u{f0a9}"),
    ("super space", "\u{f0c9}"),
    ("superwhisper: cancel", "\u{f00d}"),
    ("superwhisper: toggle", "\u{f130}"),
    ("switch input language", "\u{f0ac}"),
    ("switch input language back", "\u{f0ac}"),
    ("toggle full screen desktop", "\u{f065}"),
    ("toggle laptop display", "\u{f108}"),
    ("toggle laptop display mirroring", "\u{f108}"),
    ("toggle transparency on a window", "\u{f06e}"),
    ("world clock", "\u{f017}"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resolves_lines_and_masks() {
        let out = "SUPER + RETURN                    \u{2192} Terminal\n\
                   SUPER + SHIFT + RETURN            \u{2192} Browser\n\
                   CTRL + ALT + DELETE               \u{2192} Close all windows\n\
                   SUPER + 1                         \u{2192} Switch to workspace 1\n";
        let binds = parse_binds(out);
        assert_eq!(binds.len(), 4);
        assert_eq!(
            (
                binds[0].mask,
                binds[0].key.as_str(),
                binds[0].description.as_str()
            ),
            (MASK_SUPER, "RETURN", "Terminal")
        );
        assert_eq!(binds[1].mask, MASK_SUPER | MASK_SHIFT);
        assert_eq!(binds[2].mask, MASK_CTRL | MASK_ALT);
        assert_eq!(binds[3].key, "1");
    }

    #[test]
    fn parse_skips_noise_and_crlf() {
        let out = "SUPER + SPACE \u{2192} Omarchy menu\r\n\
                   \r\n\
                   no arrow here\n\
                   SUPER + mouse:273 \u{2192} Resize window\n\
                   SUPER + switch:on:Lid Switch \u{2192} Something\n\
                   BOGUS + X \u{2192} Weird\n";
        let binds = parse_binds(out);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].description, "Omarchy menu");
    }

    #[test]
    fn modmask_from_qmk_bits_maps_the_canonical_mods() {
        assert_eq!(modmask_from_qmk_bits(0x01), MASK_CTRL);
        assert_eq!(modmask_from_qmk_bits(0x02), MASK_SHIFT);
        assert_eq!(modmask_from_qmk_bits(0x04), MASK_ALT);
        assert_eq!(modmask_from_qmk_bits(0x08), MASK_SUPER);
        assert_eq!(modmask_from_qmk_bits(0x05), MASK_CTRL | MASK_ALT);
        assert_eq!(
            modmask_from_qmk_bits(0x10),
            0,
            "right-hand flag is not a mod"
        );
    }

    #[test]
    fn modmask_from_keycodes_handles_mods_and_mod_tap() {
        assert_eq!(modmask_from_keycodes(&[0x00E0]), MASK_CTRL); // KC_LCTL
        assert_eq!(modmask_from_keycodes(&[0x00E1]), MASK_SHIFT); // KC_LSFT
        assert_eq!(modmask_from_keycodes(&[0x00E6]), MASK_ALT); // KC_RALT
        assert_eq!(modmask_from_keycodes(&[0x00E3, 0x00E7]), MASK_SUPER); // LGUI + RGUI
        assert_eq!(modmask_from_keycodes(&[0x2114]), MASK_CTRL); // LCTL_T(KC_Q)
        assert_eq!(
            modmask_from_keycodes(&[0x00E3, 0x2114]),
            MASK_SUPER | MASK_CTRL
        );
        assert_eq!(modmask_from_keycodes(&[0x0014]), 0); // plain Q
    }

    #[test]
    fn header_order_and_caps_ignored() {
        assert_eq!(header_for(MASK_SUPER), "SUPER");
        assert_eq!(header_for(MASK_SUPER | MASK_SHIFT), "SUPER + SHIFT");
        assert_eq!(
            header_for(MASK_SUPER | MASK_CTRL | MASK_ALT | MASK_SHIFT),
            "SUPER + CTRL + ALT + SHIFT"
        );
        assert_eq!(header_for(MASK_CAPS), "");
    }

    #[test]
    fn hypr_names_for_basics_and_tap_hold() {
        assert_eq!(hypr_name(0x04).as_deref(), Some("A"));
        assert_eq!(hypr_name(0x1E).as_deref(), Some("1"));
        assert_eq!(hypr_name(0x28).as_deref(), Some("RETURN"));
        assert_eq!(hypr_name(0x2C).as_deref(), Some("SPACE"));
        assert_eq!(hypr_name(0x36).as_deref(), Some("COMMA"));
        assert_eq!(hypr_name(0x3A).as_deref(), Some("F1"));
        assert_eq!(hypr_name(0x50).as_deref(), Some("LEFT"));
        assert_eq!(hypr_name(0x46).as_deref(), Some("PRINT"));
        assert_eq!(hypr_name(0x2114).as_deref(), Some("Q")); // LCTL_T(Q) tap
        assert_eq!(hypr_name(0x4200 | 0x29).as_deref(), Some("ESCAPE")); // LT(2, Esc)
        assert_eq!(hypr_name(0x00E0), None); // mods are not bind targets
    }

    #[test]
    fn hypr_names_media_brightness_and_brackets() {
        assert_eq!(hypr_name(0xBD).as_deref(), Some("XF86MonBrightnessUp"));
        assert_eq!(hypr_name(0xBE).as_deref(), Some("XF86MonBrightnessDown"));
        assert_eq!(hypr_name(0x2F).as_deref(), Some("BRACKETLEFT"));
        assert_eq!(hypr_name(0x30).as_deref(), Some("BRACKETRIGHT"));
    }

    #[test]
    fn binds_for_mask_exact_match_and_standard_merge() {
        let binds = parse_binds("SUPER + C \u{2192} Custom close\nSUPER CTRL + J \u{2192} Herdr\n");
        let super_only = binds_for_mask(&binds, MASK_SUPER, true);
        assert_eq!(
            super_only.get("C").map(String::as_str),
            Some("Custom close")
        );
        assert!(
            super_only.get("X").is_none(),
            "standard must not leak into SUPER"
        );

        let ctrl_only = binds_for_mask(&[], MASK_CTRL, true);
        assert_eq!(ctrl_only.get("F").map(String::as_str), Some("Find"));
        assert_eq!(ctrl_only.get("R").map(String::as_str), Some("Reload"));
        assert_eq!(ctrl_only.get("C").map(String::as_str), Some("Copy"));
        assert_eq!(ctrl_only.get("V").map(String::as_str), Some("Paste"));

        let super_ctrl = binds_for_mask(&binds, MASK_SUPER | MASK_CTRL, true);
        assert_eq!(super_ctrl.get("J").map(String::as_str), Some("Herdr"));
        assert!(super_ctrl.get("C").is_none());

        let alt = binds_for_mask(&[], MASK_ALT, true);
        assert_eq!(alt.get("F4").map(String::as_str), Some("Close window"));
        assert_eq!(alt.len(), 1);
    }

    #[test]
    fn binds_for_mask_skips_hints_real_binds_cover() {
        let binds = parse_binds(
            "SUPER + C \u{2192} Universal copy\nSUPER + W \u{2192} Close tab\nSUPER ALT + Z \u{2192} Universal undo\nSUPER + F \u{2192} Full screen\nSUPER + R \u{2192} Refresh\n",
        );
        let ctrl = binds_for_mask(&binds, MASK_CTRL, true);
        assert!(ctrl.get("C").is_none(), "Universal copy covers Ctrl+C");
        assert!(ctrl.get("W").is_none(), "Close tab covers Ctrl+W");
        assert!(ctrl.get("Z").is_none(), "Universal undo covers Ctrl+Z");
        assert!(ctrl.get("R").is_none(), "Super+R Refresh forwards Ctrl+R");
        assert_eq!(ctrl.get("F").map(String::as_str), Some("Find"));
        assert_eq!(ctrl.get("S").map(String::as_str), Some("Save"));
        assert_eq!(ctrl.get("T").map(String::as_str), Some("New tab"));
    }

    #[test]
    fn binds_for_mask_standard_tables_optional() {
        let binds = parse_binds("CTRL + C \u{2192} Custom copy\n");
        let with_standard = binds_for_mask(&binds, MASK_CTRL, true);
        assert_eq!(
            with_standard.get("C").map(String::as_str),
            Some("Custom copy")
        );
        assert_eq!(with_standard.get("F").map(String::as_str), Some("Find"));

        let without_standard = binds_for_mask(&binds, MASK_CTRL, false);
        assert_eq!(
            without_standard.get("C").map(String::as_str),
            Some("Custom copy")
        );
        assert!(without_standard.get("F").is_none());

        assert!(binds_for_mask(&[], MASK_CTRL, false).is_empty());
        assert!(binds_for_mask(&[], MASK_ALT, false).is_empty());
    }

    #[test]
    fn parse_normalizes_key_and_mod_case() {
        let binds =
            parse_binds("SUPER + Home \u{2192} Go home\nsuper shift + Delete \u{2192} Cut line\n");
        assert_eq!(binds.len(), 2);
        assert_eq!((binds[0].mask, binds[0].key.as_str()), (MASK_SUPER, "HOME"));
        assert_eq!(
            (binds[1].mask, binds[1].key.as_str()),
            (MASK_SUPER | MASK_SHIFT, "DELETE")
        );
    }

    #[test]
    fn fetch_binds_with_missing_command_is_none() {
        assert!(fetch_binds_with("definitely-not-a-real-command-xyz").is_none());
    }

    #[test]
    fn hyprland_bind_shadows_standard_table() {
        let binds = parse_binds("CTRL + F \u{2192} Custom find\n");
        let map = binds_for_mask(&binds, MASK_CTRL, true);
        assert_eq!(map.get("F").map(String::as_str), Some("Custom find"));
        assert_eq!(map.get("S").map(String::as_str), Some("Save"));
    }

    #[test]
    fn shift_label_transforms_letters_digits_punctuation() {
        assert_eq!(shift_label(0x04).as_deref(), Some("A")); // KC_A
        assert_eq!(shift_label(0x14).as_deref(), Some("Q"));
        assert_eq!(shift_label(0x1E).as_deref(), Some("!")); // KC_1
        assert_eq!(shift_label(0x27).as_deref(), Some(")")); // KC_0
        assert_eq!(shift_label(0x38).as_deref(), Some("?")); // KC_SLASH
        assert_eq!(shift_label(0x2114).as_deref(), Some("Q")); // mod-tap target
        assert_eq!(shift_label(0x50), None); // arrow stays
    }

    #[test]
    fn glyph_table_and_fallback() {
        assert_eq!(glyph_for("Terminal").as_deref(), Some("\u{f120}"));
        assert_eq!(glyph_for("terminal").as_deref(), Some("\u{f120}"));
        assert_eq!(glyph_for("Full screen").as_deref(), Some("\u{f065}"));
        assert_eq!(
            glyph_for("Switch to workspace 3").as_deref(),
            Some("\u{f0a9} 3")
        );
        assert_eq!(
            glyph_for("Move window to workspace 10").as_deref(),
            Some("\u{f047} 10")
        );
        assert_eq!(
            glyph_for("Move window silently to workspace 2").as_deref(),
            Some("\u{f047} 2")
        );
        assert_eq!(glyph_for("Some unknown action").as_deref(), None);
        assert_eq!(fallback_glyph("some unknown action"), "S");
        assert_eq!(fallback_glyph(""), "");
    }

    #[test]
    fn split_and_layout_glyphs_differ() {
        assert_eq!(
            glyph_for("Toggle window split").as_deref(),
            Some("\u{f0db}")
        );
        assert_eq!(
            glyph_for("Toggle workspace layout").as_deref(),
            Some("\u{f009}")
        );
    }

    #[test]
    fn glyph_rules_cover_common_bind_variants() {
        assert_eq!(glyph_for("Universal copy").as_deref(), Some("\u{f0c5}"));
        assert_eq!(glyph_for("Universal paste").as_deref(), Some("\u{f0ea}"));
        assert_eq!(glyph_for("Universal cut").as_deref(), Some("\u{f0c4}"));
        assert_eq!(glyph_for("Universal undo").as_deref(), Some("\u{f0e2}"));
        assert_eq!(glyph_for("Universal redo").as_deref(), Some("\u{f01e}"));
        assert_eq!(
            glyph_for("Pop window out (float & pin)").as_deref(),
            Some("\u{f08d}")
        );
        assert_eq!(
            glyph_for("Start dictation (push-to-talk)").as_deref(),
            Some("\u{f130}")
        );
        assert_eq!(
            glyph_for("Stop dictation (push-to-talk)").as_deref(),
            Some("\u{f130}")
        );
        assert_eq!(
            glyph_for("Expand window left a little").as_deref(),
            Some("\u{f065}")
        );
        assert_eq!(glyph_for("Shrink window up").as_deref(), Some("\u{f066}"));
        assert_eq!(
            glyph_for("Move window to group on left").as_deref(),
            Some("\u{f047}")
        );
        assert_eq!(
            glyph_for("Swap window to the right").as_deref(),
            Some("\u{f0ec}")
        );
        assert_eq!(
            glyph_for("Focus on next monitor").as_deref(),
            Some("\u{f108}")
        );
        assert_eq!(glyph_for("Recover monitors").as_deref(), Some("\u{f108}"));
        assert_eq!(
            glyph_for("Dismiss last notification").as_deref(),
            Some("\u{f1f6}")
        );
        assert_eq!(
            glyph_for("Toggle silencing notifications").as_deref(),
            Some("\u{f1f6}")
        );
        assert_eq!(
            glyph_for("Open notification history").as_deref(),
            Some("\u{f0f3}")
        );
        assert_eq!(glyph_for("System menu").as_deref(), Some("\u{f013}"));
        assert_eq!(glyph_for("Emojis").as_deref(), Some("\u{f118}"));
        assert_eq!(glyph_for("Next track").as_deref(), Some("\u{f04e}"));
        assert_eq!(
            glyph_for("Toggle window grouping").as_deref(),
            Some("\u{f247}")
        );
        assert_eq!(glyph_for("Full screen").as_deref(), Some("\u{f065}"));
        assert_eq!(
            glyph_for("Focus on left window").as_deref(),
            Some("\u{f060} \u{f2d2}")
        );
        assert_eq!(
            glyph_for("Focus on right window").as_deref(),
            Some("\u{f061} \u{f2d2}")
        );
        assert_eq!(
            glyph_for("Focus on above window").as_deref(),
            Some("\u{f062} \u{f2d2}")
        );
        assert_eq!(
            glyph_for("Focus on below window").as_deref(),
            Some("\u{f063} \u{f2d2}")
        );
        assert_eq!(glyph_for("Some unknown action").as_deref(), None);
    }
}
