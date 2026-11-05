//! Omarchy theme colors from `~/.local/state/omarchy/current/theme/colors.toml`,
//! and the panel look (background, border, corners) Omarchy gives its own popups.

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::gio;
use gtk::gio::prelude::{FileExt, FileMonitorExt};
use gtk::glib;
use serde::Deserialize;

const THEME_DIR: &str = "theme";
const THEME_NAME_FILE: &str = "theme.name";
const WATCH_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub background: String,
    pub foreground: String,
    pub accent: String,
    pub muted: String,
    pub selection: String,
    pub panel: Panel,
}

/// A color with alpha, each channel 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

/// The overlay's own surface, styled like an Omarchy popup: the theme's
/// `[popups]` background and border, and Hyprland's window rounding.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    pub background: Rgba,
    /// One color is a solid border; more are a gradient at `border_angle`.
    pub border: Vec<Rgba>,
    pub border_angle: f64,
    pub border_width: f64,
    pub radius: f64,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            background: Rgba { r: 0.07, g: 0.11, b: 0.17, a: 1.0 },
            border: Vec::new(),
            border_angle: 0.0,
            border_width: 0.0,
            radius: 0.0,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "#121c2b".into(),
            foreground: "#e8d4b0".into(),
            accent: "#e8b84a".into(),
            muted: "#4d5e75".into(),
            selection: "#2b2418".into(),
            panel: Panel::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ColorsFile {
    #[serde(default)]
    accent: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    foreground: Option<String>,
    #[serde(default)]
    muted: Option<String>,
    #[serde(default)]
    selection: Option<String>,
}

fn omarchy_current_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/state/omarchy/current")
}

pub fn theme_path() -> PathBuf {
    omarchy_current_dir().join(THEME_DIR).join("colors.toml")
}

pub fn theme_from_str(s: &str) -> Theme {
    let mut theme = Theme::default();
    let Ok(parsed) = toml::from_str::<ColorsFile>(s) else {
        return theme;
    };
    if let Some(v) = parsed.background {
        theme.background = v;
    }
    if let Some(v) = parsed.foreground {
        theme.foreground = v;
    }
    if let Some(v) = parsed.accent {
        theme.accent = v;
    }
    if let Some(v) = parsed.muted {
        theme.muted = v;
    }
    if let Some(v) = parsed.selection {
        theme.selection = v;
    }
    theme
}

pub fn load_omarchy_theme() -> Theme {
    let mut theme = match fs::read_to_string(theme_path()) {
        Ok(s) => theme_from_str(&s),
        Err(_) => Theme::default(),
    };
    let read = |p: PathBuf| fs::read_to_string(p).unwrap_or_default();
    let theme_shell = read(omarchy_current_dir().join(THEME_DIR).join("shell.toml"));
    let user_shell = dirs::home_dir()
        .map(|h| read(h.join(".config/omarchy/shell.toml")))
        .unwrap_or_default();
    theme.panel = panel_from_shell(&theme, &theme_shell, &user_shell, hyprland_rounding());
    theme
}

/// Hyprland's `decoration:rounding`, which Omarchy's popups use for corners.
fn hyprland_rounding() -> f64 {
    std::process::Command::new("hyprctl")
        .args(["getoption", "decoration:rounding", "-j"])
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .and_then(|v| v.get("int").and_then(serde_json::Value::as_f64))
        .unwrap_or(0.0)
        .max(0.0)
}

/// `[popups]` from the theme's shell.toml with the user's shell.toml on top,
/// resolved the way Omarchy does for its own popups. Missing keys fall back to
/// the theme's background and no border, the overlay's look before.
pub fn panel_from_shell(theme: &Theme, theme_shell: &str, user_shell: &str, radius: f64) -> Panel {
    let docs: Vec<toml::Table> = [theme_shell, user_shell]
        .iter()
        .filter_map(|s| s.parse::<toml::Table>().ok())
        .collect();
    let value = |section: &str, key: &str| -> Option<toml::Value> {
        docs.iter()
            .rev()
            .find_map(|d| d.get(section).and_then(|t| t.get(key)).cloned())
    };
    let text = |section: &str, key: &str| value(section, key).and_then(|v| v.as_str().map(str::to_owned));
    let number = |section: &str, key: &str| {
        value(section, key).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
    };
    // "hyprland.active-border" style references point at another key.
    let resolve = |raw: String| -> String {
        let t = raw.trim();
        match t.split_once('.') {
            Some((sec, key)) if !t.contains(' ') && !t.starts_with('#') && !t.starts_with("rgb") => {
                text(sec, key).unwrap_or_default()
            }
            _ => t.to_owned(),
        }
    };

    let fallback_bg = parse_color(&theme.background).unwrap_or(Panel::default().background);
    let bg_alpha = number("popups", "background-alpha").unwrap_or(1.0).clamp(0.0, 1.0);
    let bg = text("popups", "background")
        .map(resolve)
        .and_then(|s| parse_color(&s))
        .unwrap_or(fallback_bg);

    let border_alpha = number("popups", "border-alpha").unwrap_or(1.0).clamp(0.0, 1.0);
    let (mut border, mut angle) = (Vec::new(), 0.0);
    if let Some(raw) = text("popups", "border").map(resolve) {
        for part in raw.split_whitespace() {
            if let Some(deg) = part.strip_suffix("deg").and_then(|d| d.parse::<f64>().ok()) {
                angle = deg;
            } else if let Some(c) = parse_color(part) {
                border.push(Rgba { a: c.a * border_alpha, ..c });
            }
        }
    }
    let border_width = if border.is_empty() {
        0.0
    } else {
        value("popups", "border-width")
            .and_then(|v| match v {
                toml::Value::String(s) => s.split_whitespace().next().and_then(|n| n.parse().ok()),
                toml::Value::Integer(i) => Some(i as f64),
                toml::Value::Float(f) => Some(f),
                _ => None,
            })
            .unwrap_or(2.0)
            .max(0.0)
    };

    Panel {
        background: Rgba { a: bg.a * bg_alpha, ..bg },
        border,
        border_angle: angle,
        border_width,
        radius,
    }
}

/// `#rrggbb`, `#rrggbbaa`, `rgb(rrggbb)` or `rgba(rrggbbaa)`.
pub fn parse_color(s: &str) -> Option<Rgba> {
    let s = s.trim();
    let hex = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))
        .and_then(|h| h.strip_suffix(')'))
        .or_else(|| s.strip_prefix('#'))?;
    if !(hex.len() == 6 || hex.len() == 8) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0)) / 255.0;
    Some(Rgba {
        r: byte(0),
        g: byte(2),
        b: byte(4),
        a: if hex.len() == 8 { byte(6) } else { 1.0 },
    })
}

pub fn watch_omarchy_theme(callback: impl Fn() + 'static) -> anyhow::Result<Vec<gio::FileMonitor>> {
    watch_dirs(&[omarchy_current_dir()], is_theme_change, callback)
}

/// Debounced directory watcher: one monitor per directory, `relevant` filters
/// events, and bursts within [`WATCH_DEBOUNCE`] collapse into one callback.
pub fn watch_dirs(
    dirs: &[PathBuf],
    relevant: fn(&gio::File, Option<&gio::File>) -> bool,
    callback: impl Fn() + 'static,
) -> anyhow::Result<Vec<gio::FileMonitor>> {
    let callback = Rc::new(callback);
    let pending = Rc::new(Cell::new(false));
    let mut monitors = Vec::new();
    let mut first_err = None;
    for dir in dirs {
        let monitor = match gio::File::for_path(dir)
            .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        {
            Ok(m) => m,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
                continue;
            }
        };
        let callback = callback.clone();
        let pending = pending.clone();
        monitor.connect_changed(move |_, file, other, _| {
            if !relevant(file, other) || pending.get() {
                return;
            }
            pending.set(true);
            let pending = pending.clone();
            let callback = callback.clone();
            glib::timeout_add_local_once(WATCH_DEBOUNCE, move || {
                pending.set(false);
                callback();
            });
        });
        monitors.push(monitor);
    }
    if monitors.is_empty() {
        if let Some(e) = first_err {
            return Err(e.into());
        }
    }
    Ok(monitors)
}

fn is_theme_change(file: &gio::File, other: Option<&gio::File>) -> bool {
    let relevant = |f: Option<&gio::File>| {
        f.and_then(|f| f.basename()).is_some_and(|name| {
            let name = name.to_string_lossy();
            name == THEME_DIR || name == THEME_NAME_FILE
        })
    };
    relevant(Some(file)) || relevant(other)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn watcher_fires_on_theme_dir_swap() {
        let dir = std::env::temp_dir().join(format!(
            "omakeys-theme-watch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join(THEME_DIR)).unwrap();
        fs::write(dir.join(THEME_NAME_FILE), "one").unwrap();

        let fired = Rc::new(Cell::new(0u32));
        let fired_cb = fired.clone();
        let _monitor =
            watch_dirs(&[dir.clone()], |_, _| true, move || fired_cb.set(fired_cb.get() + 1))
                .unwrap();

        fs::rename(dir.join(THEME_DIR), dir.join("old-theme")).unwrap();
        fs::create_dir_all(dir.join(THEME_DIR)).unwrap();
        fs::write(dir.join(THEME_NAME_FILE), "two").unwrap();

        let ctx = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(3);
        while fired.get() == 0 && Instant::now() < deadline {
            ctx.iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(fired.get() >= 1, "theme watcher callback did not fire");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_colors_toml() {
        let s = r##"
accent = "#e8b84a"
background = "#121c2b"
foreground = "#e8d4b0"
muted = "#4d5e75"
selection = "#2b2418"
"##;
        let t = theme_from_str(s);
        assert_eq!(t.accent, "#e8b84a");
        assert_eq!(t.background, "#121c2b");
    }

    #[test]
    fn panel_follows_popups_with_user_override_and_border_reference() {
        let theme_shell = r##"
[hyprland]
active-border = "rgba(ffffff66) rgba(ffffff1a) 45deg"

[popups]
background = "#1c1c1e"
background-alpha = 0.55
border = "hyprland.active-border"
"##;
        let user_shell = "[popups]\nbackground-alpha = 0.9\n";
        let p = panel_from_shell(&Theme::default(), theme_shell, user_shell, 16.0);
        assert!((p.background.a - 0.9).abs() < 1e-9, "user alpha wins");
        assert!((p.background.r - 0x1c as f64 / 255.0).abs() < 1e-9);
        assert_eq!(p.border.len(), 2);
        assert!((p.border[0].a - 0x66 as f64 / 255.0).abs() < 1e-9);
        assert_eq!(p.border_angle, 45.0);
        assert_eq!(p.border_width, 2.0);
        assert_eq!(p.radius, 16.0);
    }

    #[test]
    fn panel_without_shell_toml_keeps_the_plain_background() {
        let p = panel_from_shell(&Theme::default(), "", "", 0.0);
        assert_eq!(p.background, parse_color("#121c2b").unwrap());
        assert!(p.border.is_empty());
        assert_eq!(p.border_width, 0.0);
    }

    #[test]
    fn unrendered_template_color_falls_back_to_theme_background() {
        let p = panel_from_shell(&Theme::default(), "[popups]\nbackground = \"{{ background }}\"\n", "", 0.0);
        assert_eq!(p.background, parse_color("#121c2b").unwrap());
    }
}
