//! Physical key geometry and identity of a keyboard. The built-in profile is
//! the LoremIpsum36; other boards come from `~/.config/omakeys/boards/*.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::evdev::{evdev_layout, ANSI, TRIO_500};
use crate::core::config::config_dir;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyPos {
    pub row: u8,
    pub col: u8,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Where a board's keys come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardSource {
    /// QMK firmware over Raw HID: layers, keymap and key events.
    RawHid,
    /// The kernel's evdev nodes: key presses only, one fixed layout.
    Evdev,
}

/// Name of the built-in any-keyboard profile.
pub const GENERIC: &str = ANSI;

#[derive(Debug, Clone, PartialEq)]
pub struct BoardProfile {
    pub name: String,
    pub source: BoardSource,
    /// USB ids of the board's Raw HID interface.
    pub vid: u16,
    pub pid: u16,
    pub keys: Vec<KeyPos>,
    pub rows: u8,
    pub cols: u8,
    /// Highest layer index drawn.
    pub max_layer_inclusive: u8,
    /// Layer names by index. Missing indices show as `L{n}`.
    pub layer_names: Vec<String>,
    /// Shorter names for layer-switch keys, by index. Falls back to the name.
    pub layer_short_names: Vec<String>,
    /// Author firmware: QK_KB_0..11 (0x7E00..) macros, GAME and one-hand keys.
    /// Off for every other board, where those codes mean something else.
    pub custom_keycodes: bool,
    pub game_layer: u8,
    pub one_hand_layer: u8,
    /// Evdev boards: the keyboards it reads, by part of the device name.
    /// None reads every keyboard that no other board claims.
    pub evdev_match: Option<&'static str>,
}

impl BoardProfile {
    pub fn layer_name(&self, layer: u8) -> Option<&str> {
        self.layer_names
            .get(layer as usize)
            .map(String::as_str)
            .filter(|n| !n.is_empty())
    }

    /// Name of a layer, `L{n}` when it has none.
    pub fn layer_label(&self, layer: u8) -> String {
        self.layer_name(layer)
            .map_or_else(|| format!("L{layer}"), str::to_string)
    }

    /// Layer-switch key label: the short name when the profile has one.
    pub fn switch_layer_label(&self, layer: u8) -> String {
        self.layer_short_names
            .get(layer as usize)
            .filter(|n| !n.is_empty())
            .cloned()
            .unwrap_or_else(|| self.layer_label(layer))
    }
}

fn visual_x(col: u8) -> f64 {
    let c = col as f64;
    if col >= 5 {
        c + 1.5
    } else {
        c
    }
}

/// LoremIpsum36 geometry from `keyboard/penk/loremipsum36/keyboard.json` LAYOUT,
/// with VIA-style 1.5u split gap between halves.
pub fn loremipsum36() -> BoardProfile {
    const MATRIX: &[(u8, u8)] = &[
        (0, 0),
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (0, 5),
        (0, 6),
        (0, 7),
        (0, 8),
        (0, 9),
        (1, 0),
        (1, 1),
        (1, 2),
        (1, 3),
        (1, 4),
        (1, 5),
        (1, 6),
        (1, 7),
        (1, 8),
        (1, 9),
        (2, 0),
        (2, 1),
        (2, 2),
        (2, 3),
        (2, 4),
        (2, 5),
        (2, 6),
        (2, 7),
        (2, 8),
        (2, 9),
        (3, 2),
        (3, 3),
        (3, 4),
        (3, 5),
        (3, 6),
        (3, 7),
    ];

    let keys = MATRIX
        .iter()
        .map(|&(row, col)| KeyPos {
            row,
            col,
            x: visual_x(col),
            y: row as f64,
            w: 1.0,
            h: 1.0,
        })
        .collect();

    let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect();
    BoardProfile {
        name: "loremipsum36".into(),
        source: BoardSource::RawHid,
        vid: 0x5254,
        pid: 0x0008,
        keys,
        rows: 4,
        cols: 10,
        max_layer_inclusive: 13,
        layer_names: names(&[
            "BASE",
            "QWERTY",
            "SYM",
            "NAV",
            "FUN",
            "ADJ",
            "GAME",
            "GAME X",
            "GAME Y",
            "ONE HAND",
            "ONE HAND FLIP",
            "ONE HAND NAV",
            "ONE HAND NUM",
            "ONE HAND SYM",
        ]),
        // Inside the one-hand layers the mode prefix is obvious.
        layer_short_names: names(&[
            "", "", "", "", "", "", "", "", "", "", "FLIP", "NAV", "NUM", "SYM",
        ]),
        custom_keycodes: true,
        game_layer: 6,
        one_hand_layer: 9,
        evdev_match: None,
    }
}

/// One board file under `~/.config/omakeys/boards/`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardFile {
    name: String,
    vid: u16,
    pid: u16,
    /// Matrix size. Defaults to the largest row and col used by `keys`, plus one.
    rows: Option<u8>,
    cols: Option<u8>,
    /// Columns from this one on shift right by `split_gap` (split boards).
    split_col: Option<u8>,
    #[serde(default = "default_split_gap")]
    split_gap: f64,
    /// Layer names by index. The layer count follows its length.
    #[serde(default)]
    layers: Vec<String>,
    /// Highest layer index to read. Defaults to the last named layer, or 7.
    max_layer: Option<u8>,
    #[serde(default)]
    custom_keycodes: bool,
    game_layer: Option<u8>,
    one_hand_layer: Option<u8>,
    keys: Vec<KeyFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    row: u8,
    col: u8,
    x: Option<f64>,
    y: Option<f64>,
    #[serde(default = "one")]
    w: f64,
    #[serde(default = "one")]
    h: f64,
}

fn default_split_gap() -> f64 {
    1.5
}

fn one() -> f64 {
    1.0
}

impl BoardProfile {
    pub fn from_toml(text: &str) -> Result<Self> {
        let file: BoardFile = toml::from_str(text)?;
        if file.keys.is_empty() {
            bail!("no [[keys]] listed");
        }
        let split = file.split_col;
        let keys: Vec<KeyPos> = file
            .keys
            .iter()
            .map(|k| KeyPos {
                row: k.row,
                col: k.col,
                x: k.x.unwrap_or_else(|| {
                    let gap = split
                        .filter(|&s| k.col >= s)
                        .map_or(0.0, |_| file.split_gap);
                    f64::from(k.col) + gap
                }),
                y: k.y.unwrap_or(f64::from(k.row)),
                w: k.w,
                h: k.h,
            })
            .collect();
        let rows = file
            .rows
            .unwrap_or_else(|| keys.iter().map(|k| k.row).max().unwrap_or(0) + 1);
        let cols = file
            .cols
            .unwrap_or_else(|| keys.iter().map(|k| k.col).max().unwrap_or(0) + 1);
        if keys.iter().any(|k| k.row >= rows || k.col >= cols) {
            bail!("a key lies outside the {rows}x{cols} matrix");
        }
        let max_layer_inclusive = file.max_layer.unwrap_or(match file.layers.len() {
            0 => 7,
            n => (n - 1) as u8,
        });
        Ok(Self {
            name: file.name,
            source: BoardSource::RawHid,
            vid: file.vid,
            pid: file.pid,
            keys,
            rows,
            cols,
            max_layer_inclusive,
            layer_names: file.layers,
            layer_short_names: Vec::new(),
            custom_keycodes: file.custom_keycodes,
            game_layer: file.game_layer.unwrap_or(0),
            one_hand_layer: file.one_hand_layer.unwrap_or(0),
            evdev_match: None,
        })
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("parse {}", path.display()))
    }
}

pub fn boards_dir() -> PathBuf {
    config_dir().join("boards")
}

fn evdev_profile(name: &str, evdev_match: Option<&'static str>) -> BoardProfile {
    let layout = evdev_layout(name);
    BoardProfile {
        name: name.into(),
        source: BoardSource::Evdev,
        vid: 0,
        pid: 0,
        keys: layout.keys,
        rows: layout.rows,
        cols: layout.cols,
        max_layer_inclusive: 0,
        layer_names: vec!["KEYBOARD".into()],
        layer_short_names: Vec::new(),
        custom_keycodes: false,
        game_layer: 0,
        one_hand_layer: 0,
        evdev_match,
    }
}

/// Any keyboard: laptop, Bluetooth or USB, drawn as ANSI, no layers.
pub fn generic() -> BoardProfile {
    let mut p = evdev_profile(GENERIC, None);
    // Fn is drawn (MO(1) keycode) so the legend reads "Fn"; the overlay never
    // switches to that layer on a plain keyboard.
    p.layer_names = vec!["KEYBOARD".into(), "Fn".into()];
    p.layer_short_names = vec!["KEYBOARD".into(), "Fn".into()];
    p
}

/// Samsung Smart Keyboard Trio 500.
pub fn samsung_trio_500() -> BoardProfile {
    let mut p = evdev_profile(TRIO_500, Some("Trio 500"));
    p.max_layer_inclusive = 1;
    p.layer_names = vec!["KEYBOARD".into(), "FN".into()];
    p.layer_short_names = vec!["KEYBOARD".into(), "Fn".into()];
    p
}

/// Profile to run with. `board` is a profile name, or `auto`: the profile of a
/// plugged-in QMK board (`connected` holds the vid:pid of every Raw HID
/// device), else the generic any-keyboard profile.
pub fn select_profile(
    mut profiles: Vec<BoardProfile>,
    board: &str,
    connected: &[(u16, u16)],
) -> BoardProfile {
    let pick = if board == "auto" {
        profiles
            .iter()
            .position(|p| p.source == BoardSource::RawHid && connected.contains(&(p.vid, p.pid)))
    } else {
        let found = profiles.iter().position(|p| p.name == board);
        if found.is_none() {
            eprintln!("omakeys: no board profile named {board:?}, using the default");
        }
        found
    };
    let fallback = profiles.iter().position(|p| p.name == GENERIC).unwrap_or(0);
    profiles.swap_remove(pick.unwrap_or(fallback))
}

/// Built-in board plus every valid file in the boards dir. Bad files are
/// reported on stderr and skipped. A user file with the same name replaces
/// the built-in one.
pub fn load_profiles() -> Vec<BoardProfile> {
    let mut profiles = vec![generic(), loremipsum36(), samsung_trio_500()];
    let Ok(entries) = fs::read_dir(boards_dir()) else {
        return profiles;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        match BoardProfile::from_file(&path) {
            Ok(p) => {
                profiles.retain(|q| q.name != p.name);
                profiles.push(p);
            }
            Err(e) => eprintln!("omakeys: skipping board file: {e:#}"),
        }
    }
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loremipsum36_has_36_keys_and_split_gap() {
        let p = loremipsum36();
        assert_eq!(p.keys.len(), 36);
        assert_eq!(p.max_layer_inclusive, 13);
        let left = p.keys.iter().find(|k| k.row == 0 && k.col == 4).unwrap();
        let right = p.keys.iter().find(|k| k.row == 0 && k.col == 5).unwrap();
        assert!((right.x - left.x - left.w) >= 1.4);
    }

    fn named(name: &str, vid: u16) -> BoardProfile {
        BoardProfile {
            name: name.into(),
            vid,
            ..loremipsum36()
        }
    }

    #[test]
    fn auto_picks_the_connected_board() {
        let list = vec![named("a", 1), named("b", 2), generic()];
        assert_eq!(select_profile(list.clone(), "auto", &[(2, 8)]).name, "b");
        // No QMK board connected: any-keyboard mode.
        assert_eq!(select_profile(list.clone(), "auto", &[]).name, GENERIC);
        assert_eq!(select_profile(list.clone(), "a", &[(2, 8)]).name, "a");
        assert_eq!(select_profile(list, "nope", &[]).name, GENERIC);
    }

    #[test]
    fn generic_profile_is_one_layer_of_evdev_keys() {
        let p = generic();
        assert_eq!(p.source, BoardSource::Evdev);
        assert_eq!(p.max_layer_inclusive, 0);
        assert_eq!(p.layer_label(0), "KEYBOARD");
        assert!(p.keys.len() > 50);
    }

    #[test]
    fn shipped_example_board_parses() {
        let p =
            BoardProfile::from_toml(include_str!("../../examples/boards/example.toml")).unwrap();
        assert_eq!(p.keys.len(), 20);
        assert_eq!(p.max_layer_inclusive, 3);
        assert_eq!((p.rows, p.cols), (2, 10));
    }

    #[test]
    fn minimal_board_file_fills_defaults() {
        let p = BoardProfile::from_toml(
            "name = \"tiny\"\nvid = 0x1234\npid = 0xabcd\nsplit_col = 2\n\
             keys = [{row = 0, col = 0}, {row = 0, col = 2}, {row = 1, col = 1}]\n",
        )
        .unwrap();
        assert_eq!((p.vid, p.pid), (0x1234, 0xabcd));
        assert_eq!((p.rows, p.cols), (2, 3));
        assert_eq!(p.max_layer_inclusive, 7);
        assert_eq!(p.keys[1].x, 3.5);
        assert!(!p.custom_keycodes);
        assert_eq!(p.layer_label(1), "L1");
    }

    #[test]
    fn board_file_rejects_key_outside_matrix() {
        let e = BoardProfile::from_toml(
            "name = \"x\"\nvid = 1\npid = 2\nrows = 1\nkeys = [{row = 1, col = 0}]\n",
        );
        assert!(e.is_err());
    }

    #[test]
    fn layer_names_drive_the_layer_count() {
        let p = BoardProfile::from_toml(
            "name = \"x\"\nvid = 1\npid = 2\nlayers = [\"BASE\", \"NAV\"]\n\
             keys = [{row = 0, col = 0}]\n",
        )
        .unwrap();
        assert_eq!(p.max_layer_inclusive, 1);
        assert_eq!(p.layer_label(1), "NAV");
        assert_eq!(p.switch_layer_label(1), "NAV");
    }
}
