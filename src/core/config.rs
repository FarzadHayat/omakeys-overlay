//! `~/.config/omakeys/config.toml`

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::board::labels::LabelStyle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VisibilityMode {
    /// Legacy: treated as [`Auto`] on load. Temp hide is the overlay toggle.
    Hidden,
    Always,
    #[default]
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    #[default]
    BottomRight,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindsConfig {
    /// Show the host keybind layer while modifiers are held.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Merge the built-in Ctrl/Alt shortcut tables (Ctrl+C copy and friends)
    /// under the host binds.
    #[serde(default = "default_true")]
    pub standard: bool,
}

impl Default for BindsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            standard: true,
        }
    }
}

/// What the overlay does when the pointer touches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HoverMode {
    /// Fully click-through; the pointer is ignored.
    Off,
    /// Tooltip with the full key text. The overlay takes pointer input, so
    /// clicks and scroll over it stop passing through to windows below.
    Tooltip,
    /// Hop to another anchor while the pointer is over the overlay's home
    /// area, so the text underneath can be read.
    Dodge,
    /// On-screen keyboard: the overlay keeps pointer input and tooltips, and
    /// a click types that key into the focused window (needs access to `/dev/uinput`).
    #[default]
    Type,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(from = "HoverRaw")]
pub struct HoverConfig {
    pub mode: HoverMode,
}

/// On-disk shape: `mode`, or the legacy `enabled` bool (true = tooltip).
#[derive(Deserialize)]
struct HoverRaw {
    mode: Option<HoverMode>,
    enabled: Option<bool>,
}

impl From<HoverRaw> for HoverConfig {
    fn from(raw: HoverRaw) -> Self {
        let mode = raw.mode.unwrap_or(match raw.enabled {
            Some(true) => HoverMode::Tooltip,
            Some(false) => HoverMode::Off,
            None => HoverMode::default(),
        });
        Self { mode }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub mode: VisibilityMode,
    pub timeout_ms: u64,
    pub anchor: Anchor,
    pub opacity: f64,
    /// Overlay width as a fraction of the monitor's shorter side (0.50 = 50%).
    /// Clamped to 0.20..=1.00 and snapped to 10% steps.
    #[serde(default = "default_scale")]
    pub scale: f64,
    pub toggle_chord: String,
    /// Which board profile to show: `auto` (the plugged-in one) or a profile name.
    pub board: String,
    /// Legacy field; ignored (Omarchy bar widget replaced the SNI tray).
    #[serde(default = "default_true")]
    pub tray_visible: bool,
    #[serde(default)]
    pub label_style: LabelStyle,
    #[serde(default)]
    pub binds: BindsConfig,
    #[serde(default)]
    pub hover: HoverConfig,
}

fn default_true() -> bool {
    true
}

/// 50% of the shorter screen side, close to the old fixed 44 px unit look.
fn default_scale() -> f64 {
    0.50
}

/// Clamp to 20%–100% of the shorter screen side and snap to 10% steps.
pub fn normalize_scale(scale: f64) -> f64 {
    if !scale.is_finite() {
        return default_scale();
    }
    let pct = ((scale * 100.0) / 10.0).round() * 10.0;
    pct.clamp(20.0, 100.0) / 100.0
}

impl Anchor {
    /// Anchors in clockwise order around the screen edge.
    const RING: [Anchor; 8] = [
        Anchor::TopLeft,
        Anchor::Top,
        Anchor::TopRight,
        Anchor::Right,
        Anchor::BottomRight,
        Anchor::Bottom,
        Anchor::BottomLeft,
        Anchor::Left,
    ];

    /// Horizontal and vertical screen position: -1 start, 0 centre, 1 end.
    pub fn grid(self) -> (i32, i32) {
        match self {
            Anchor::TopLeft => (-1, -1),
            Anchor::Top => (0, -1),
            Anchor::TopRight => (1, -1),
            Anchor::Left => (-1, 0),
            Anchor::Right => (1, 0),
            Anchor::BottomLeft => (-1, 1),
            Anchor::Bottom => (0, 1),
            Anchor::BottomRight => (1, 1),
        }
    }

    /// Next spot along the screen edge when the pointer pushes the overlay by
    /// `push` (overlay centre minus pointer, y down). Steps to whichever ring
    /// neighbour moves most with the push; a push into the wall slides along it.
    pub fn dodge(self, push: (f64, f64)) -> Anchor {
        let i = Self::RING.iter().position(|a| *a == self).unwrap_or(0);
        let prev = Self::RING[(i + 7) % 8];
        let next = Self::RING[(i + 1) % 8];
        let (cx, cy) = self.grid();
        let score = |a: Anchor| {
            let (x, y) = a.grid();
            f64::from(x - cx) * push.0 + f64::from(y - cy) * push.1
        };
        if score(prev) > score(next) {
            prev
        } else {
            next
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mode: VisibilityMode::Auto,
            timeout_ms: 2000,
            anchor: Anchor::BottomRight,
            opacity: 0.8,
            scale: default_scale(),
            toggle_chord: "SUPER+ALT+O".into(),
            board: "auto".into(),
            tray_visible: default_true(),
            label_style: LabelStyle::Icons,
            binds: BindsConfig::default(),
            hover: HoverConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn normalized(mut self) -> Self {
        // Mode Hidden removed from UI — temp mute is force_hidden / toggle.
        if self.mode == VisibilityMode::Hidden {
            self.mode = VisibilityMode::Auto;
        }
        self.scale = normalize_scale(self.scale);
        self.opacity = self.opacity.clamp(0.05, 1.0);
        self
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("omakeys")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn load_config() -> AppConfig {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(s) => match toml::from_str::<AppConfig>(&s) {
            Ok(cfg) => cfg.normalized(),
            Err(e) => {
                eprintln!("omakeys: bad {}, using defaults: {e}", path.display());
                AppConfig::default()
            }
        },
        Err(_) => AppConfig::default(),
    }
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let s = toml::to_string_pretty(cfg).context("serialize config")?;
    fs::write(config_path(), s).context("write config.toml")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_set_keys() {
        let cfg: AppConfig = toml::from_str("opacity = 0.5\n").unwrap();
        assert_eq!(cfg.opacity, 0.5);
        assert_eq!(cfg.timeout_ms, AppConfig::default().timeout_ms);
    }

    #[test]
    fn default_round_trip_toml() {
        let cfg = AppConfig::default();
        let s = toml::to_string(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&s).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn config_json_round_trip() {
        let cfg = AppConfig::default();
        let s = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.mode, cfg.mode);
        assert_eq!(back.timeout_ms, cfg.timeout_ms);
        assert_eq!(back.anchor, cfg.anchor);
        assert!((back.opacity - cfg.opacity).abs() < f64::EPSILON);
        assert!((back.scale - cfg.scale).abs() < f64::EPSILON);
        assert_eq!(back.label_style, cfg.label_style);
        assert_eq!(back.binds, cfg.binds);
        assert_eq!(back.hover, cfg.hover);
    }

    #[test]
    fn normalize_scale_clamps_and_steps() {
        assert!((normalize_scale(0.4) - 0.4).abs() < f64::EPSILON);
        assert!((normalize_scale(0.43) - 0.4).abs() < f64::EPSILON);
        assert!((normalize_scale(0.48) - 0.5).abs() < f64::EPSILON);
        assert!((normalize_scale(0.22) - 0.2).abs() < f64::EPSILON);
        assert!((normalize_scale(0.05) - 0.2).abs() < f64::EPSILON);
        assert!((normalize_scale(1.2) - 1.0).abs() < f64::EPSILON);
        assert!((normalize_scale(f64::NAN) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn normalize_migrates_hidden_mode_to_auto() {
        let cfg = AppConfig {
            mode: VisibilityMode::Hidden,
            ..AppConfig::default()
        }
        .normalized();
        assert_eq!(cfg.mode, VisibilityMode::Auto);
    }

    #[test]
    fn missing_binds_table_defaults_enabled() {
        let s = "mode = \"auto\"\ntimeout_ms = 2000\nanchor = \"bottom_right\"\nopacity = 0.9\nscale = 0.5\ntoggle_chord = \"SUPER+ALT+O\"\ntray_visible = true\n";
        let cfg: AppConfig = toml::from_str(s).unwrap();
        assert!(cfg.binds.enabled);
        assert!(cfg.binds.standard);
    }

    #[test]
    fn binds_table_parses_and_round_trips() {
        let cfg = AppConfig {
            binds: BindsConfig {
                enabled: false,
                standard: false,
            },
            ..AppConfig::default()
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(s.contains("[binds]"));
        let back: AppConfig = toml::from_str(&s).unwrap();
        assert!(!back.binds.enabled);
        assert!(!back.binds.standard);
    }

    #[test]
    fn missing_binds_fields_default_enabled_and_standard() {
        let s = "mode = \"auto\"\ntimeout_ms = 2000\nanchor = \"bottom_right\"\nopacity = 0.9\nscale = 0.5\ntoggle_chord = \"SUPER+ALT+O\"\ntray_visible = true\n[binds]\nstandard = false\n";
        let cfg: AppConfig = toml::from_str(s).unwrap();
        assert!(cfg.binds.enabled);
        assert!(!cfg.binds.standard);
    }

    #[test]
    fn missing_hover_table_defaults_to_type() {
        let s = "mode = \"auto\"\ntimeout_ms = 2000\nanchor = \"bottom_right\"\nopacity = 0.9\nscale = 0.5\ntoggle_chord = \"SUPER+ALT+O\"\ntray_visible = true\n";
        let cfg: AppConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.hover.mode, HoverMode::Type);
    }

    #[test]
    fn legacy_hover_enabled_maps_to_mode() {
        let base = "mode = \"auto\"\ntimeout_ms = 2000\nanchor = \"bottom_right\"\nopacity = 0.9\nscale = 0.5\ntoggle_chord = \"SUPER+ALT+O\"\ntray_visible = true\n";
        let on: AppConfig = toml::from_str(&format!("{base}[hover]\nenabled = true\n")).unwrap();
        assert_eq!(on.hover.mode, HoverMode::Tooltip);
        let off: AppConfig = toml::from_str(&format!("{base}[hover]\nenabled = false\n")).unwrap();
        assert_eq!(off.hover.mode, HoverMode::Off);
        let mode: AppConfig = toml::from_str(&format!("{base}[hover]\nmode = \"off\"\n")).unwrap();
        assert_eq!(mode.hover.mode, HoverMode::Off);
    }

    #[test]
    fn dodge_slides_along_wall_and_around_corners() {
        // Corner: push from above slides along the bottom, from the left up the side.
        assert_eq!(Anchor::BottomRight.dodge((0.0, 1.0)), Anchor::Bottom);
        assert_eq!(Anchor::BottomRight.dodge((1.0, 0.0)), Anchor::Right);
        assert_eq!(Anchor::TopLeft.dodge((0.0, -1.0)), Anchor::Top);
        assert_eq!(Anchor::TopLeft.dodge((-1.0, 0.0)), Anchor::Left);
        // Edge: push moves along it in the push direction.
        assert_eq!(Anchor::Bottom.dodge((1.0, 0.0)), Anchor::BottomRight);
        assert_eq!(Anchor::Bottom.dodge((-1.0, 0.0)), Anchor::BottomLeft);
        assert_eq!(Anchor::Left.dodge((0.0, -1.0)), Anchor::TopLeft);
        assert_eq!(Anchor::Top.dodge((1.0, 0.0)), Anchor::TopRight);
        assert_eq!(Anchor::Right.dodge((0.0, 1.0)), Anchor::BottomRight);
    }

    #[test]
    fn dodge_always_moves() {
        for a in Anchor::RING {
            assert_ne!(a.dodge((0.0, 0.0)), a);
        }
    }

    #[test]
    fn hover_table_parses_and_round_trips() {
        let cfg = AppConfig {
            hover: HoverConfig {
                mode: HoverMode::Tooltip,
            },
            ..AppConfig::default()
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(s.contains("[hover]"));
        let back: AppConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.hover.mode, HoverMode::Tooltip);
    }

    #[test]
    fn hover_mode_type_parses_and_round_trips() {
        let cfg = AppConfig {
            hover: HoverConfig {
                mode: HoverMode::Type,
            },
            ..AppConfig::default()
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(s.contains("mode = \"type\""));
        let back: AppConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.hover.mode, HoverMode::Type);
    }
}
