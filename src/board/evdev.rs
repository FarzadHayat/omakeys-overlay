//! Any keyboard, without firmware help: laptop, Bluetooth or plain USB boards.
//! Key presses come from the kernel's evdev nodes, drawn on a fixed ANSI
//! layout. Such a board cannot report layers, so the overlay shows one layer
//! and lights up the keys you press.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::mem::size_of;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::client::{BoardClient, Keymap};
use super::profile::KeyPos;
use super::protocol::HostEvent;

const RESCAN: Duration = Duration::from_secs(2);
const EV_KEY: u16 = 1;
const SYS_INPUT: &str = "/sys/class/input";

/// One drawn key: its Linux key code, the QMK/HID keycode it is labelled
/// with, and its size in key units. Code 0 is blank space, not a key.
struct Cell {
    evdev: u16,
    hid: u8,
    w: f64,
    h: f64,
    /// Sits under the previous key instead of beside it (split arrow keys).
    below: bool,
}

const fn c(evdev: u16, hid: u8, w: f64) -> Cell {
    Cell { evdev, hid, w, h: 1.0, below: false }
}

/// Invisible spacer (advances x, draws nothing).
const fn gap(w: f64) -> Cell {
    c(0, 0, w)
}

/// Half-height key; `below` stacks it under the one before.
const fn half(evdev: u16, hid: u8, w: f64, below: bool) -> Cell {
    Cell { evdev, hid, w, h: 0.5, below }
}

/// A row of keys and its height in key units.
struct Row {
    h: f64,
    cells: Vec<Cell>,
}

/// Names of the built-in layouts.
pub const ANSI: &str = "generic";
pub const TRIO_500: &str = "samsung-trio-500";

/// F1–F12 Linux codes and HID keycodes.
const F_KEYS: [(u16, u8); 12] = [
    (59, 0x3A),
    (60, 0x3B),
    (61, 0x3C),
    (62, 0x3D),
    (63, 0x3E),
    (64, 0x3F),
    (65, 0x40),
    (66, 0x41),
    (67, 0x42),
    (68, 0x43),
    (87, 0x44),
    (88, 0x45),
];

/// Gap between main block and the nav / sys cluster.
const ANSI_NAV_GAP: f64 = 0.5;
/// One key in the 3-wide nav / sys cluster.
const ANSI_NAV: f64 = 1.0;
/// Main alphanumeric block width (ANSI units).
#[cfg(test)]
const ANSI_MAIN: f64 = 15.0;
/// Full board width: main + gap + Ins/Home/PgUp (or arrows).
#[cfg(test)]
const ANSI_WIDTH: f64 = ANSI_MAIN + ANSI_NAV_GAP + 3.0 * ANSI_NAV;

/// Append the standard gap + three-key cluster to the right of a 15-unit row.
fn with_nav_cluster(mut row: Row, keys: [Cell; 3]) -> Row {
    row.cells.push(gap(ANSI_NAV_GAP));
    row.cells.extend(keys);
    row
}

/// ANSI TKL F-row: Esc, grouped F1–F12, then PrtSc / ScrLk / Pause over the nav cluster.
fn ansi_function_row() -> Row {
    let mut cells = vec![c(1, 0x29, 1.0), gap(1.0)];
    for &(e, k) in &F_KEYS[0..4] {
        cells.push(c(e, k, 1.0));
    }
    cells.push(gap(0.5));
    for &(e, k) in &F_KEYS[4..8] {
        cells.push(c(e, k, 1.0));
    }
    cells.push(gap(0.5));
    for &(e, k) in &F_KEYS[8..12] {
        cells.push(c(e, k, 1.0));
    }
    // 1+1+4+0.5+4+0.5+4 = 15; sys cluster sits over Ins/Home/PgUp.
    cells.push(gap(ANSI_NAV_GAP));
    cells.push(c(99, 0x46, ANSI_NAV)); // Print Screen
    cells.push(c(70, 0x47, ANSI_NAV)); // Scroll Lock
    cells.push(c(119, 0x48, ANSI_NAV)); // Pause
    Row { h: 1.0, cells }
}

/// Number row up to Backspace.
fn number_row(backspace: f64) -> Row {
    const N: [(u16, u8); 12] = [
        (2, 0x1E),
        (3, 0x1F),
        (4, 0x20),
        (5, 0x21),
        (6, 0x22),
        (7, 0x23),
        (8, 0x24),
        (9, 0x25),
        (10, 0x26),
        (11, 0x27),
        (12, 0x2D),
        (13, 0x2E),
    ];
    let mut cells = vec![c(41, 0x35, 1.0)];
    cells.extend(N.iter().map(|&(e, k)| c(e, k, 1.0)));
    cells.push(c(14, 0x2A, backspace));
    Row { h: 1.0, cells }
}

fn letter_rows(tab: f64, backslash: f64, caps: f64, enter: f64, shift_l: f64, shift_r: f64) -> [Row; 3] {
    let keys = |list: &[(u16, u8)]| -> Vec<Cell> { list.iter().map(|&(e, k)| c(e, k, 1.0)).collect() };
    let mut top = vec![c(15, 0x2B, tab)];
    // Q W E R T Y U I O P [ ]
    top.extend(keys(&[
        (16, 0x14),
        (17, 0x1A),
        (18, 0x08),
        (19, 0x15),
        (20, 0x17),
        (21, 0x1C),
        (22, 0x18),
        (23, 0x0C),
        (24, 0x12),
        (25, 0x13),
        (26, 0x2F),
        (27, 0x30),
    ]));
    top.push(c(43, 0x31, backslash));
    let mut home = vec![c(58, 0x39, caps)];
    // A S D F G H J K L ; '
    home.extend(keys(&[
        (30, 0x04),
        (31, 0x16),
        (32, 0x07),
        (33, 0x09),
        (34, 0x0A),
        (35, 0x0B),
        (36, 0x0D),
        (37, 0x0E),
        (38, 0x0F),
        (39, 0x33),
        (40, 0x34),
    ]));
    home.push(c(28, 0x28, enter));
    let mut bottom = vec![c(42, 0xE1, shift_l)];
    // Z X C V B N M , . /
    bottom.extend(keys(&[
        (44, 0x1D),
        (45, 0x1B),
        (46, 0x06),
        (47, 0x19),
        (48, 0x05),
        (49, 0x11),
        (50, 0x10),
        (51, 0x36),
        (52, 0x37),
        (53, 0x38),
    ]));
    bottom.push(c(54, 0xE5, shift_r));
    [
        Row { h: 1.0, cells: top },
        Row { h: 1.0, cells: home },
        Row { h: 1.0, cells: bottom },
    ]
}

/// ANSI TKL (no numpad): main 15-unit block, sys/nav cluster on the right,
/// inverted-T arrows under the nav cluster, Fn between Ctrl and Super.
fn ansi_rows() -> Vec<Row> {
    let [top, home, bottom] = letter_rows(1.5, 1.5, 1.75, 2.25, 2.25, 2.75);
    let blank_nav = [gap(ANSI_NAV), gap(ANSI_NAV), gap(ANSI_NAV)];
    vec![
        ansi_function_row(),
        with_nav_cluster(
            number_row(2.0),
            [
                c(110, 0x49, ANSI_NAV), // Insert
                c(102, 0x4A, ANSI_NAV), // Home
                c(104, 0x4B, ANSI_NAV), // PgUp
            ],
        ),
        with_nav_cluster(
            top,
            [
                c(111, 0x4C, ANSI_NAV), // Delete
                c(107, 0x4D, ANSI_NAV), // End
                c(109, 0x4E, ANSI_NAV), // PgDn
            ],
        ),
        with_nav_cluster(home, blank_nav),
        // Up sits in the middle of the nav column (over Down).
        with_nav_cluster(
            bottom,
            [gap(ANSI_NAV), c(103, 0x52, ANSI_NAV), gap(ANSI_NAV)],
        ),
        with_nav_cluster(
            Row {
                h: 1.0,
                cells: vec![
                    c(29, 0xE0, 1.25),    // Left Ctrl
                    c(KEY_FN, 0x00, 1.0), // Fn
                    c(125, 0xE3, 1.25),   // Left Super
                    c(56, 0xE2, 1.25),    // Left Alt
                    c(57, 0x2C, 6.5),     // Space
                    c(100, 0xE6, 1.25),   // Right Alt
                    c(127, 0x65, 1.25),   // Menu
                    c(97, 0xE4, 1.25),    // Right Ctrl
                ],
            },
            [
                c(105, 0x50, ANSI_NAV), // Left
                c(108, 0x51, ANSI_NAV), // Down (Up is on the row above)
                c(106, 0x4F, ANSI_NAV), // Right
            ],
        ),
    ]
}

/// Linux KEY_FN. Trio does not advertise it; we still draw Fn and treat
/// Fn-mode keycodes as that layer being held.
const KEY_FN: u16 = 464;
/// QMK MO(1): the Fn key switches the Trio overlay to the FN layer.
const MO_FN: u16 = 0x5221;

/// Codes Trio only sends while Fn is held.
const TRIO_FN_HINTS: &[u16] = &[
    59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, // F1–F12
    102, 104, 109, 107, // Home / PgUp / PgDn / End on arrows
    165, 164, 163, 113, 114, 115, // media on 1–6
];
/// Codes Trio only sends with Fn up (F-row media / app keys).
const TRIO_BASE_HINTS: &[u16] = &[757, 758, 759, 705, 704, 172, 224, 225, 701];

/// Trio 500 F-row: Esc, F1–F12, Del — stretched to the full board width.
fn trio_function_row() -> Row {
    // Layer-0 HIDs are placeholders; `apply_trio_fn_layers` sets APP/media
    // on layer 0 and real F-keys on layer 1.
    // 14 keys across 14.7 units (same as the number row): each ~1.05.
    let w = 14.7 / 14.0;
    let mut cells = vec![c(1, 0x29, w)];
    cells.extend(F_KEYS.iter().map(|&(e, k)| c(e, k, w)));
    cells.push(c(111, 0x4C, w));
    Row { h: 0.7, cells }
}

/// Samsung Smart Keyboard Trio 500: short function row filling the width,
/// drawn Fn (MO to FN layer), small Backspace/Enter, stacked arrows.
fn trio_500_rows() -> Vec<Row> {
    let [top, home, bottom] = letter_rows(1.5, 1.2, 1.75, 1.95, 2.35, 2.35);
    vec![
        trio_function_row(),
        number_row(1.7),
        top,
        home,
        bottom,
        Row {
            h: 1.0,
            cells: vec![
                c(29, 0xE0, 1.1),   // Left Ctrl
                c(KEY_FN, 0x00, 1.1), // Fn: MO(1), filled in apply_trio_fn_layers
                c(125, 0xE3, 1.0),  // Cmd, a Super key
                c(56, 0xE2, 1.2),   // Left Alt
                c(57, 0x2C, 5.2),   // Space
                c(100, 0xE6, 1.0),  // Right Alt
                c(97, 0xE4, 1.0),   // Right Ctrl
                c(105, 0x50, 1.0),  // Left
                half(103, 0x52, 1.0, false), // Up
                half(108, 0x51, 1.0, true),  // Down
                c(106, 0x4F, 1.0),  // Right
            ],
        },
    ]
}

/// Layer 0 = no-Fn legends (APP / media / arrows). Layer 1 = Fn legends.
fn apply_trio_fn_layers(keymap: &mut Keymap, cells: &HashMap<u16, (u8, u8)>) {
    let mut put = |code: u16, layer: u8, kc: u16| {
        if let Some(&(r, c)) = cells.get(&code) {
            keymap.set(layer, r, c, kc);
        }
    };
    // F-row: Samsung defaults on layer 0, real F-keys on layer 1.
    const FROW: [(u16, u16, u16); 12] = [
        (59, 0xF1, 0x3A), // APP1 / F1
        (60, 0xF2, 0x3B),
        (61, 0xF3, 0x3C),
        (62, 0xF4, 0x3D), // Task / F4
        (63, 0xF5, 0x3E), // Recent / F5
        (64, 0xF6, 0x3F), // Home / F6
        (65, 0xF7, 0x40), // BT1 / F7
        (66, 0xF8, 0x41),
        (67, 0xF9, 0x42),
        (68, 0xBE, 0x43), // Brt- / F10
        (87, 0xBD, 0x44), // Brt+ / F11
        (88, 0xFA, 0x45), // DeX / F12
    ];
    for &(code, base, fn_kc) in &FROW {
        put(code, 0, base);
        put(code, 1, fn_kc);
    }
    // Number row with Fn: media controls on 1–6.
    for &(code, kc) in &[
        (2, 0xACu16),
        (3, 0xAE),
        (4, 0xAB),
        (5, 0xA8),
        (6, 0xAA),
        (7, 0xA9),
    ] {
        put(code, 1, kc);
    }
    // Arrows with Fn: Home / PgUp / PgDn / End.
    put(105, 1, 0x4A);
    put(103, 1, 0x4B);
    put(108, 1, 0x4E);
    put(106, 1, 0x4D);
    put(KEY_FN, 0, MO_FN);
    put(KEY_FN, 1, MO_FN);
}

/// Extra codes that light an already-drawn ANSI key (alternate names).
const ANSI_ALT: &[(u16, u16)] = &[
    (139, 127), // KEY_MENU → Menu
    (210, 99),  // KEY_PRINT → Print Screen
];

/// Codes a Trio 500 sends for a key in its other mode, as (code, the key it
/// lights). Top row in media mode, and Fn arrows and number keys. Read off
/// a real keyboard (`omakeys keys`).
const TRIO_500_ALT: &[(u16, u16)] = &[
    (757, 59), // F1, app 1
    (758, 60),
    (759, 61),
    (705, 62),
    (704, 63),
    (172, 64),

    (224, 68), // F10, brightness down
    (225, 87),
    (701, 88), // F12, Dex
    (102, 105), // Home on Left
    (104, 103), // PgUp on Up
    (109, 108), // PgDn on Down
    (107, 106), // End on Right
    (165, 2),   // previous track on 1
    (164, 3),   // play/pause on 2
    (163, 4),   // next track on 3
    (113, 5),   // mute on 4
    (114, 6),   // volume down on 5
    (115, 7),   // volume up on 6
];

/// Geometry, one layer of HID keycodes, and the evdev code → cell table.
pub struct EvdevLayout {
    pub keys: Vec<KeyPos>,
    pub keymap: Keymap,
    pub cells: HashMap<u16, (u8, u8)>,
    pub rows: u8,
    pub cols: u8,
}

/// Layout by profile name; unknown names get plain ANSI.
pub fn evdev_layout(name: &str) -> EvdevLayout {
    let (rows, alt, n_layers) = match name {
        TRIO_500 => (trio_500_rows(), TRIO_500_ALT, 2),
        _ => (ansi_rows(), ANSI_ALT, 1),
    };
    let cols = rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
    let mut keymap = Keymap::empty(n_layers, rows.len(), cols);
    let mut keys = Vec::new();
    let mut cells = HashMap::new();
    let mut y = 0.0;
    for (r, row) in rows.iter().enumerate() {
        let (mut x, mut prev_y) = (0.0, y);
        for (col, cell) in row.cells.iter().enumerate() {
            let at_y = if cell.below { prev_y + 0.5 } else { y };
            if cell.evdev != 0 {
                keys.push(KeyPos {
                    row: r as u8,
                    col: col as u8,
                    x: if cell.below { x - cell.w } else { x },
                    y: at_y,
                    w: cell.w,
                    h: if cell.h < 1.0 { cell.h } else { row.h },
                });
                let kc = u16::from(cell.hid);
                keymap.set(0, r as u8, col as u8, kc);
                if n_layers > 1 {
                    keymap.set(1, r as u8, col as u8, kc);
                }
                cells.insert(cell.evdev, (r as u8, col as u8));
            }
            prev_y = at_y;
            if !cell.below {
                x += cell.w;
            }
        }
        y += row.h;
    }
    for &(code, primary) in alt {
        if let Some(&cell) = cells.get(&primary) {
            cells.insert(code, cell);
        }
    }
    if name == TRIO_500 {
        apply_trio_fn_layers(&mut keymap, &cells);
    } else if let Some(&(r, c)) = cells.get(&KEY_FN) {
        // Drawn Fn for laptops; label via MO(1) / profile "Fn" short name.
        // Generic stays one layer — Fn does not switch the overlay.
        keymap.set(0, r, c, MO_FN);
    }
    EvdevLayout {
        keys,
        keymap,
        cells,
        rows: rows.len() as u8,
        cols: cols as u8,
    }
}

/// A keyboard-like input node found under `/sys/class/input`.
#[derive(Debug, Clone, PartialEq)]
pub struct InputKeyboard {
    pub path: PathBuf,
    pub name: String,
    pub bus: u16,
    pub vid: u16,
    pub pid: u16,
}

impl InputKeyboard {
    /// "usb", "bluetooth", "built-in" for the doctor and logs.
    pub fn bus_name(&self) -> &'static str {
        match self.bus {
            0x03 => "usb",
            0x05 => "bluetooth",
            0x11 | 0x19 => "built-in",
            _ => "other",
        }
    }
}

/// Parse the sysfs `capabilities/key` bitmap (hex words, most significant
/// first) and say whether it describes a typing keyboard: letters and space.
pub fn caps_have_typing_keys(caps: &str) -> bool {
    let words: Vec<u64> = caps
        .split_whitespace()
        .rev()
        .map(|w| u64::from_str_radix(w, 16).unwrap_or(0))
        .collect();
    let has = |code: usize| {
        words
            .get(code / 64)
            .is_some_and(|w| w & (1 << (code % 64)) != 0)
    };
    (16..=25).all(has) && (30..=38).all(has) && has(57) && has(28)
}

fn read_trim(path: PathBuf) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Every typing keyboard the kernel knows, except virtual ones (including our
/// own on-screen keyboard).
pub fn list_keyboards() -> Vec<InputKeyboard> {
    list_key_nodes(true)
}

/// Input nodes that have keys. `typing_only` keeps real keyboards; otherwise
/// media-key and system-control nodes come too.
pub fn list_key_nodes(typing_only: bool) -> Vec<InputKeyboard> {
    let Ok(entries) = fs::read_dir(SYS_INPUT) else {
        return Vec::new();
    };
    let mut found: Vec<InputKeyboard> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let file = e.file_name().into_string().ok()?;
            if !file.starts_with("event") {
                return None;
            }
            let dev = e.path().join("device");
            let caps = read_trim(dev.join("capabilities/key"))?;
            let has_keys = caps.split_whitespace().any(|w| w.chars().any(|c| c != '0'));
            if typing_only && !caps_have_typing_keys(&caps) || !has_keys {
                return None;
            }
            let bus = u16::from_str_radix(&read_trim(dev.join("id/bustype"))?, 16).ok()?;
            let name = read_trim(dev.join("name")).unwrap_or_default();
            // Virtual keyboards (ours, text expanders) are not someone typing.
            if bus == 0x06 || name.to_lowercase().contains("virtual") {
                return None;
            }
            let id = |f: &str| {
                read_trim(dev.join("id").join(f)).and_then(|v| u16::from_str_radix(&v, 16).ok())
            };
            Some(InputKeyboard {
                path: PathBuf::from("/dev/input").join(file),
                name,
                bus,
                vid: id("vendor").unwrap_or(0),
                pid: id("product").unwrap_or(0),
            })
        })
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

struct Open {
    file: File,
}

/// Which keyboards an evdev board reads. A board named after a keyboard model
/// reads only that model; the generic board reads the rest.
#[derive(Debug, Clone, Default)]
pub struct Claim {
    /// Device-name part this board reads. None: every keyboard nobody else claims.
    pub name: Option<String>,
    /// Name parts claimed by other boards.
    pub others: Vec<String>,
    /// USB ids of QMK boards: they report through their own client.
    pub skip_ids: Vec<(u16, u16)>,
}

impl Claim {
    fn takes(&self, kb: &InputKeyboard) -> bool {
        if self.skip_ids.contains(&(kb.vid, kb.pid)) {
            return false;
        }
        let name = kb.name.to_lowercase();
        let has = |part: &String| name.contains(&part.to_lowercase());
        match &self.name {
            Some(mine) => has(mine),
            None => !self.others.iter().any(has),
        }
    }
}

/// Reads every keyboard the board claims at once, so a laptop and an external board both show.
pub struct EvdevBoardClient {
    layout: EvdevLayout,
    /// Which keyboards this board reads.
    claim: Claim,
    open: HashMap<PathBuf, Open>,
    next_scan: Instant,
    /// Fn layer drawn on the overlay (Trio). Bare Fn is silent; Fn-mode
    /// keycodes and a real KEY_FN (if ever) turn this on.
    fn_layer: bool,
    fn_hint_depth: u16,
    fn_cell: Option<(u8, u8)>,
}

impl EvdevBoardClient {
    pub fn new(layout_name: &str, claim: Claim) -> Self {
        let layout = evdev_layout(layout_name);
        // Only the Trio board infers a silent Fn layer from Fn-only codes.
        // Generic draws Fn but treats it as a normal key.
        let fn_cell = if layout_name == TRIO_500 {
            layout.cells.get(&KEY_FN).copied()
        } else {
            None
        };
        let mut client = Self {
            layout,
            claim,
            open: HashMap::new(),
            next_scan: Instant::now(),
            fn_layer: false,
            fn_hint_depth: 0,
            fn_cell,
        };
        client.rescan();
        client
    }

    fn set_fn_layer(&mut self, want: bool, events: &mut Vec<HostEvent>) {
        if want == self.fn_layer {
            return;
        }
        self.fn_layer = want;
        events.push(HostEvent::Layer {
            default_layer: 1,
            layer_state: if want { 1 << 1 } else { 0 },
        });
        if let Some((row, col)) = self.fn_cell {
            events.push(HostEvent::Key {
                row,
                col,
                pressed: want,
            });
        }
    }

    fn rescan(&mut self) {
        self.next_scan = Instant::now() + RESCAN;
        for kb in list_keyboards() {
            if self.open.contains_key(&kb.path) || !self.claim.takes(&kb) {
                continue;
            }
            // Never grab: the keys must still reach the focused window.
            match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&kb.path)
            {
                Ok(file) => {
                    eprintln!("omakeys: reading {} ({})", kb.name, kb.bus_name());
                    self.open.insert(kb.path, Open { file });
                }
                Err(e) => eprintln!(
                    "omakeys: cannot read {} ({e}); join the input group, then log in again",
                    kb.path.display()
                ),
            }
        }
    }
}

impl BoardClient for EvdevBoardClient {
    fn poll_events(&mut self) -> Vec<HostEvent> {
        if Instant::now() >= self.next_scan {
            self.rescan();
        }
        let mut raw: Vec<(u16, bool)> = Vec::new();
        let mut gone = Vec::new();
        let size = size_of::<libc::input_event>();
        let mut buf = vec![0u8; size * 32];
        for (path, dev) in &mut self.open {
            loop {
                match dev.file.read(&mut buf) {
                    Ok(0) => {
                        gone.push(path.clone());
                        break;
                    }
                    Ok(n) => {
                        for chunk in buf[..n].chunks_exact(size) {
                            // SAFETY: chunk is exactly one input_event and the
                            // struct has no invalid bit patterns.
                            let ev: libc::input_event =
                                unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                            // value 2 is autorepeat: not a new press.
                            if ev.type_ != EV_KEY || ev.value > 1 {
                                continue;
                            }
                            raw.push((ev.code, ev.value == 1));
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        // Unplugged or asleep (a Bluetooth board dropping off).
                        gone.push(path.clone());
                        break;
                    }
                }
            }
        }
        let dropped = !gone.is_empty();
        for path in gone {
            self.open.remove(&path);
        }
        // Bluetooth drop / sleep can miss Fn key releases; clear the latch.
        if dropped && self.fn_cell.is_some() && self.fn_layer {
            self.fn_hint_depth = 0;
            let mut clear = Vec::new();
            self.set_fn_layer(false, &mut clear);
            // Apply clears before new presses from this poll.
            let mut events = clear;
            self.collect_key_events(raw, &mut events);
            return events;
        }

        let mut events = Vec::new();
        self.collect_key_events(raw, &mut events);
        events
    }

    fn keymap(&self) -> &Keymap {
        &self.layout.keymap
    }

    fn ok(&self) -> bool {
        // The layout is always drawn: a sleeping Bluetooth board wakes on a keypress.
        true
    }
}

impl EvdevBoardClient {
    fn collect_key_events(&mut self, raw: Vec<(u16, bool)>, events: &mut Vec<HostEvent>) {
        for (code, pressed) in raw {
            // Trio: infer the silent Fn key from Fn-only codes.
            if self.fn_cell.is_some() {
                if code == KEY_FN {
                    self.fn_hint_depth = 0;
                    self.set_fn_layer(pressed, events);
                } else if TRIO_FN_HINTS.contains(&code) {
                    if pressed {
                        self.fn_hint_depth = self.fn_hint_depth.saturating_add(1);
                    } else {
                        self.fn_hint_depth = self.fn_hint_depth.saturating_sub(1);
                    }
                    self.set_fn_layer(self.fn_hint_depth > 0, events);
                } else if pressed && TRIO_BASE_HINTS.contains(&code) {
                    self.fn_hint_depth = 0;
                    self.set_fn_layer(false, events);
                }
            }
            if let Some(&(row, col)) = self.layout.cells.get(&code) {
                // Trio Fn is lit via set_fn_layer; generic Fn is a normal key.
                if code != KEY_FN || self.fn_cell.is_none() {
                    events.push(HostEvent::Key {
                        row,
                        col,
                        pressed,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn right_edge(l: &EvdevLayout, row: u8) -> f64 {
        l.keys
            .iter()
            .filter(|k| k.row == row)
            .map(|k| k.x + k.w)
            .fold(0.0, f64::max)
    }

    #[test]
    fn ansi_tkl_width_and_clusters() {
        let l = evdev_layout(ANSI);
        let board_w = l.keys.iter().map(|k| k.x + k.w).fold(0.0, f64::max);
        assert!(
            (board_w - ANSI_WIDTH).abs() < 1e-9,
            "board width {board_w}, want {ANSI_WIDTH}"
        );
        // F / number / Q / mods carry the right-hand cluster to the edge.
        for r in [0u8, 1, 2, 5] {
            let right = right_edge(&l, r);
            assert!(
                (right - ANSI_WIDTH).abs() < 1e-9,
                "row {r} ends at {right}, want {ANSI_WIDTH}"
            );
        }
        // A-row has no nav keys — main block only.
        assert!((right_edge(&l, 3) - ANSI_MAIN).abs() < 1e-9);
        // Z-row only places Up in the middle nav column.
        let z_right = ANSI_MAIN + ANSI_NAV_GAP + 2.0 * ANSI_NAV;
        assert!((right_edge(&l, 4) - z_right).abs() < 1e-9);
    }

    #[test]
    fn ansi_tkl_has_sys_nav_fn_menu_and_inverted_t_arrows() {
        let l = evdev_layout(ANSI);
        assert!(l.cells.contains_key(&KEY_FN), "Fn");
        assert!(l.cells.contains_key(&110), "Insert");
        assert!(l.cells.contains_key(&99), "Print Screen");
        assert!(l.cells.contains_key(&70), "Scroll Lock");
        assert!(l.cells.contains_key(&119), "Pause");
        assert!(l.cells.contains_key(&127), "Menu");
        assert!(l.cells.contains_key(&102), "Home");
        assert!(l.cells.contains_key(&107), "End");
        assert!(l.cells.contains_key(&104), "PgUp");
        assert!(l.cells.contains_key(&109), "PgDn");
        let (r, c) = l.cells[&KEY_FN];
        assert_eq!(l.keymap.get(0, r, c), MO_FN);
        // Inverted-T: Up above Down, same x; Left/Right flank Down.
        let up = l.keys.iter().find(|k| l.cells[&103] == (k.row, k.col)).unwrap();
        let down = l.keys.iter().find(|k| l.cells[&108] == (k.row, k.col)).unwrap();
        let left = l.keys.iter().find(|k| l.cells[&105] == (k.row, k.col)).unwrap();
        let right = l.keys.iter().find(|k| l.cells[&106] == (k.row, k.col)).unwrap();
        assert!((up.x - down.x).abs() < 1e-9, "Up over Down");
        assert!(down.y > up.y, "Down below Up");
        assert!((left.y - down.y).abs() < 1e-9 && (right.y - down.y).abs() < 1e-9);
        assert!(left.x < down.x && right.x > down.x);
    }

    #[test]
    fn trio_rows_line_up_to_the_same_edge() {
        let l = evdev_layout(TRIO_500);
        let edges: Vec<f64> = (0..l.rows).map(|r| right_edge(&l, r)).collect();
        for e in &edges {
            assert!((e - edges[1]).abs() < 0.6, "ragged edge: {edges:?}");
        }
    }

    #[test]
    fn evdev_codes_land_on_their_hid_cell() {
        let l = evdev_layout(ANSI);
        let (r, c) = l.cells[&30]; // KEY_A
        assert_eq!(l.keymap.get(0, r, c), 0x04);
        let (r, c) = l.cells[&125]; // KEY_LEFTMETA
        assert_eq!(l.keymap.get(0, r, c), 0xE3);
        let unique: std::collections::HashSet<_> = l.cells.values().collect();
        assert_eq!(unique.len(), l.keys.len(), "every key is reachable");
        assert!(l.cells.len() >= l.keys.len(), "alts may add extra codes");
    }

    #[test]
    fn trio_other_mode_codes_light_the_key_they_share() {
        let l = evdev_layout(TRIO_500);
        assert_eq!(l.cells[&224], l.cells[&68], "brightness lights F10");
        assert_eq!(l.cells[&102], l.cells[&105], "Home lights Left");
        assert_eq!(l.cells[&115], l.cells[&7], "volume up lights 6");
        let cells: std::collections::HashSet<_> = l.cells.values().collect();
        assert_eq!(cells.len(), l.keys.len(), "every key is reachable");
    }

    #[test]
    fn trio_up_and_down_stack_in_one_column() {
        let l = evdev_layout(TRIO_500);
        let at = |code: u16| {
            let (r, c) = l.cells[&code];
            *l.keys.iter().find(|k| (k.row, k.col) == (r, c)).unwrap()
        };
        let (up, down) = (at(103), at(108));
        assert_eq!(up.x, down.x);
        assert_eq!(down.y, up.y + 0.5);
        assert_eq!((up.h, down.h), (0.5, 0.5));
    }

    #[test]
    fn trio_draws_fn_and_f_row_fills_the_width() {
        let l = evdev_layout(TRIO_500);
        assert!(l.cells.contains_key(&KEY_FN));
        let (r, c) = l.cells[&KEY_FN];
        assert_eq!(l.keymap.get(0, r, c), MO_FN);
        assert_eq!(l.keymap.get(1, r, c), MO_FN);
        assert!((right_edge(&l, 0) - right_edge(&l, 1)).abs() < 1e-9);
        let f_keys = l.keys.iter().filter(|k| k.row == 0).count();
        assert_eq!(f_keys, 14, "Esc + F1–F12 + Del, no gap cell");
    }

    #[test]
    fn trio_fn_layer_shows_f_keys_and_base_shows_apps() {
        let l = evdev_layout(TRIO_500);
        let (r, c) = l.cells[&59]; // F1 position
        assert_eq!(l.keymap.get(0, r, c), 0xF1, "APP1 without Fn");
        assert_eq!(l.keymap.get(1, r, c), 0x3A, "F1 with Fn");
        let (r, c) = l.cells[&2]; // digit 1
        assert_eq!(l.keymap.get(0, r, c), 0x1E);
        assert_eq!(l.keymap.get(1, r, c), 0xAC, "prev track with Fn");
        let (r, c) = l.cells[&105]; // Left
        assert_eq!(l.keymap.get(1, r, c), 0x4A, "Home with Fn");
    }

    #[test]
    fn claims_split_keyboards_between_boards() {
        let kb = |name: &str| InputKeyboard {
            path: PathBuf::new(),
            name: name.into(),
            bus: 5,
            vid: 1,
            pid: 2,
        };
        let trio = Claim { name: Some("trio 500".into()), ..Claim::default() };
        let generic = Claim { others: vec!["Trio 500".into()], ..Claim::default() };
        assert!(trio.takes(&kb("Smart KBD Trio 500")));
        assert!(!trio.takes(&kb("Laptop keyboard")));
        assert!(!generic.takes(&kb("Smart KBD Trio 500")));
        assert!(generic.takes(&kb("Laptop keyboard")));
        let qmk = Claim { skip_ids: vec![(1, 2)], ..Claim::default() };
        assert!(!qmk.takes(&kb("Anything")));
    }

    #[test]
    fn typing_keyboards_are_told_from_other_input() {
        // Real capability dump of a full keyboard: keys 1..=248 set.
        let kbd = "fffffffffffffffe ffffffffffffffff ffffffffffffffff ffffffffffffffff";
        assert!(caps_have_typing_keys(kbd));
        // A power button only has KEY_POWER (116).
        assert!(!caps_have_typing_keys("100000 0 0 0"));
        assert!(!caps_have_typing_keys(""));
    }
}

/// `omakeys keys`: print the Linux code of every key press on every keyboard,
/// for building a layout. Runs until interrupted.
pub fn watch_codes() -> anyhow::Result<()> {
    let size = size_of::<libc::input_event>();
    let mut buf = vec![0u8; size * 32];
    let mut open: Vec<(String, File)> = Vec::new();
    for node in list_key_nodes(false) {
        match OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&node.path) {
            Ok(file) => {
                println!("watching {} [{}]", node.name, node.path.display());
                open.push((node.name, file));
            }
            Err(e) => eprintln!("cannot read {}: {e}", node.path.display()),
        }
    }
    if open.is_empty() {
        anyhow::bail!("no readable keyboard (join the input group, then log in again)");
    }
    println!("press keys, Ctrl+C to stop");
    loop {
        for (name, file) in &mut open {
            while let Ok(n) = file.read(&mut buf) {
                for chunk in buf[..n].chunks_exact(size) {
                    // SAFETY: chunk is exactly one input_event.
                    let ev: libc::input_event =
                        unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                    if ev.type_ == EV_KEY && ev.value == 1 {
                        println!("{name}: code {}", ev.code);
                    }
                }
                if n == 0 {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
