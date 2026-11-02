//! Omarchy theme colors from `~/.local/state/omarchy/current/theme/colors.toml`.

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
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "#121c2b".into(),
            foreground: "#e8d4b0".into(),
            accent: "#e8b84a".into(),
            muted: "#4d5e75".into(),
            selection: "#2b2418".into(),
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
    match fs::read_to_string(theme_path()) {
        Ok(s) => theme_from_str(&s),
        Err(_) => Theme::default(),
    }
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
}
