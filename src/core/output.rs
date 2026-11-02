//! Type-mode output: turn a clicked keycode into key presses on a virtual
//! keyboard (uinput), so the focused window receives them like real typing.
//!
//! The keycode is the same one the legend was built from, so a click always
//! sends what the keycap shows. Keys that only change board state (pure mods,
//! layer switches, firmware custom keys) send nothing, exactly like tapping
//! them on the real board.
//!
//! A uinput device is an ordinary kernel keyboard: the compositor treats the
//! taps like the board's own, so shortcuts and input methods keep working.
//! (Wayland virtual-keyboard clients such as wtype swap the seat's keymap and
//! knock out the input method's keyboard grab, which breaks Super chords.)

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

/// Linux input codes (`linux/input-event-codes.h`) used by the HID table.
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTALT: u16 = 56;
const KEY_LEFTMETA: u16 = 125;
#[cfg(test)]
pub(crate) const KEY_Q: u16 = 16;
#[cfg(test)]
pub(crate) const KEY_C: u16 = 46;
#[cfg(test)]
pub(crate) const KEY_SPACE: u16 = 57;

/// QMK mod bit → Linux modifier key, in press order.
const MODS: [(u8, u16); 4] = [
    (0x01, KEY_LEFTCTRL),
    (0x02, KEY_LEFTSHIFT),
    (0x04, KEY_LEFTALT),
    (0x08, KEY_LEFTMETA),
];

/// A tap the overlay can deliver to the focused window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyOut {
    /// QMK canonical mod bits (ctrl 1, shift 2, alt 4, GUI 8) held for the tap.
    pub mods: u8,
    /// Linux input code of the key itself.
    pub key: u16,
}

/// Keycode → what tapping it sends, or `None` for keys with no host output.
/// `extra_mods` are modifiers latched by type-mode clicks; they ride along
/// with this tap like a hardware modifier held alongside it.
pub fn tap_output(kc: u16, extra_mods: u8) -> Option<KeyOut> {
    let (basic, mods) = match kc {
        0x0000..=0x0001 => return None, // blank / transparent
        0x0002..=0x00FF => (kc as u8, extra_mods),
        // Compound: modifier bits in the high byte, basic key in the low.
        0x0100..=0x1FFF => ((kc & 0xFF) as u8, ((kc >> 8) as u8) | extra_mods),
        // Mod-tap / layer-tap: the tap role is the low byte.
        0x2000..=0x4FFF => ((kc & 0xFF) as u8, extra_mods),
        // Pure mods, layer switches, RGB / bootloader, other firmware custom
        // keys: none with a host key output.
        _ => return None,
    };
    Some(KeyOut {
        // The 0x10 right-hand flag is dropped: one modifier per bit.
        mods: mods & 0x0F,
        key: evdev_key(basic)?,
    })
}

/// The author firmware's macro keys, as the board sends them to a Linux host:
/// Zoom mute and raise hand (Alt+A, Alt+Y) and screenshot (Print Screen).
/// Only boards whose profile has `custom_keycodes` use this.
pub fn firmware_macro_output(kc: u16) -> Option<KeyOut> {
    match kc {
        0x7E00 => Some(KeyOut {
            mods: 0x04,
            key: 30,
        }),
        0x7E01 => Some(KeyOut {
            mods: 0x04,
            key: 21,
        }),
        0x7E02 => Some(KeyOut { mods: 0, key: 99 }),
        _ => None,
    }
}

/// HID usage (QMK basic keycode) → Linux input code.
fn evdev_key(kc: u8) -> Option<u16> {
    Some(match kc {
        0x04..=0x1D => LETTERS[(kc - 0x04) as usize],
        0x1E..=0x27 => DIGITS[(kc - 0x1E) as usize],
        0x28 => 28,                             // Return
        0x29 => 1,                              // Escape
        0x2A => 14,                             // Backspace
        0x2B => 15,                             // Tab
        0x2C => 57,                             // Space
        0x2D => 12,                             // Minus
        0x2E => 13,                             // Equal
        0x2F => 26,                             // [
        0x30 => 27,                             // ]
        0x31 => 43,                             // Backslash
        0x32 => 43,                             // Non-US hash
        0x33 => 39,                             // Semicolon
        0x34 => 40,                             // Apostrophe
        0x35 => 41,                             // Grave
        0x36 => 51,                             // Comma
        0x37 => 52,                             // Period
        0x38 => 53,                             // Slash
        0x39 => 58,                             // Caps Lock
        0x3A..=0x43 => 59 + (kc - 0x3A) as u16, // F1-F10
        0x44 => 87,                             // F11
        0x45 => 88,                             // F12
        0x46 => 99,                             // Print Screen
        0x47 => 70,                             // Scroll Lock
        0x48 => 119,                            // Pause
        0x49 => 110,                            // Insert
        0x4A => 102,                            // Home
        0x4B => 104,                            // Page Up
        0x4C => 111,                            // Delete
        0x4D => 107,                            // End
        0x4E => 109,                            // Page Down
        0x4F => 106,                            // Right
        0x50 => 105,                            // Left
        0x51 => 108,                            // Down
        0x52 => 103,                            // Up
        0x65 => 127,                            // Menu
        0xA8 => 113,                            // Mute
        0xA9 => 115,                            // Volume up
        0xAA => 114,                            // Volume down
        0xAB => 163,                            // Next track
        0xAC => 165,                            // Previous track
        0xAD => 166,                            // Stop
        0xAE => 164,                            // Play / pause
        0xAF => 226,                            // Media
        0xBD => 225,                            // Brightness up
        0xBE => 224,                            // Brightness down
        _ => return None,
    })
}

/// Linux codes for A..Z (not contiguous: they follow the QWERTY rows).
const LETTERS: [u16; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];

/// Linux codes for 1..9, 0.
const DIGITS: [u16; 10] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

// uinput ioctl requests (`linux/uinput.h`): `_IOW('U', n, int)` and friends.
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_DEV_SETUP: libc::c_ulong = 0x405C_5503;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;

/// `struct uinput_setup`.
#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
/// Pause between the press and release steps of a tap, so apps and the
/// compositor see the modifiers settle before the key.
const STEP: Duration = Duration::from_millis(8);
/// The compositor needs a moment to notice a freshly created device.
const DEVICE_SETTLE: Duration = Duration::from_millis(150);

/// Virtual keyboard on `/dev/uinput`.
struct VirtualKeyboard {
    file: File,
}

impl VirtualKeyboard {
    fn create() -> std::io::Result<Self> {
        let file = OpenOptions::new().write(true).open("/dev/uinput")?;
        let fd = file.as_raw_fd();
        let ioctl = |req: libc::c_ulong, arg: libc::c_int| {
            // SAFETY: plain integer-argument uinput ioctls on a valid fd.
            if unsafe { libc::ioctl(fd, req, arg) } < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        ioctl(UI_SET_EVBIT, EV_KEY.into())?;
        for code in 1..=248 {
            ioctl(UI_SET_KEYBIT, code)?;
        }
        let mut setup = UinputSetup {
            bustype: 0x06, // BUS_VIRTUAL
            vendor: 0x5254,
            product: 0,
            version: 1,
            name: [0; 80],
            ff_effects_max: 0,
        };
        setup.name[..11].copy_from_slice(b"omakeys-osk");
        // SAFETY: setup outlives the call and matches UI_DEV_SETUP's layout.
        if unsafe { libc::ioctl(fd, UI_DEV_SETUP, &setup) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        ioctl(UI_DEV_CREATE, 0)?;
        thread::sleep(DEVICE_SETTLE);
        Ok(Self { file })
    }

    fn emit(&mut self, kind: u16, code: u16, value: i32) -> std::io::Result<()> {
        // SAFETY: zeroed input_event is valid; the kernel stamps the time.
        let mut ev: libc::input_event = unsafe { std::mem::zeroed() };
        ev.type_ = kind;
        ev.code = code;
        ev.value = value;
        // SAFETY: input_event is plain old data.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&ev as *const libc::input_event).cast::<u8>(),
                std::mem::size_of::<libc::input_event>(),
            )
        };
        self.file.write_all(bytes)
    }

    fn set(&mut self, code: u16, down: bool) -> std::io::Result<()> {
        self.emit(EV_KEY, code, i32::from(down))?;
        self.emit(EV_SYN, 0, 0)
    }

    fn tap(&mut self, out: KeyOut) -> std::io::Result<()> {
        let mods: Vec<u16> = MODS
            .iter()
            .filter(|(bit, _)| out.mods & bit != 0)
            .map(|(_, code)| *code)
            .collect();
        for code in &mods {
            self.set(*code, true)?;
        }
        thread::sleep(STEP);
        self.set(out.key, true)?;
        thread::sleep(STEP);
        self.set(out.key, false)?;
        for code in mods.iter().rev() {
            self.set(*code, false)?;
        }
        Ok(())
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        // SAFETY: destroying the device we created on this fd.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

/// Fire a tap at the focused window. The overlay never takes keyboard focus,
/// so the keys land in whatever window is active. A worker thread owns the
/// virtual keyboard (created on first use) so taps never stall the UI and
/// stay in click order. Failing to open `/dev/uinput` warns once, then clicks
/// go quiet.
pub fn send(out: &KeyOut) {
    static WORKER: OnceLock<Sender<KeyOut>> = OnceLock::new();
    let tx = WORKER.get_or_init(|| {
        let (tx, rx) = channel::<KeyOut>();
        thread::spawn(move || {
            let mut keyboard = None;
            let mut warned = false;
            while let Ok(out) = rx.recv() {
                if keyboard.is_none() {
                    match VirtualKeyboard::create() {
                        Ok(k) => keyboard = Some(k),
                        Err(err) => {
                            if !warned {
                                warned = true;
                                eprintln!(
                                    "omakeys: /dev/uinput unavailable ({err}); type mode cannot send keys"
                                );
                            }
                            continue;
                        }
                    }
                }
                if let Some(k) = keyboard.as_mut() {
                    if let Err(err) = k.tap(out) {
                        eprintln!("omakeys: type mode send failed: {err}");
                        keyboard = None;
                    }
                }
            }
        });
        tx
    });
    let _ = tx.send(*out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(mods: u8, key: u16) -> Option<KeyOut> {
        Some(KeyOut { mods, key })
    }

    #[test]
    fn typing_keys_map_to_linux_codes() {
        assert_eq!(tap_output(0x14, 0), out(0, KEY_Q));
        assert_eq!(tap_output(0x06, 0), out(0, KEY_C));
        assert_eq!(tap_output(0x2C, 0), out(0, KEY_SPACE));
        assert_eq!(tap_output(0x1E, 0), out(0, 2)); // 1
        assert_eq!(tap_output(0x27, 0), out(0, 11)); // 0
        assert_eq!(tap_output(0x28, 0), out(0, 28)); // Return
        assert_eq!(tap_output(0x3A, 0), out(0, 59)); // F1
        assert_eq!(tap_output(0x43, 0), out(0, 68)); // F10
        assert_eq!(tap_output(0x44, 0), out(0, 87)); // F11
        assert_eq!(tap_output(0x4B, 0), out(0, 104)); // Page Up
        assert_eq!(tap_output(0x52, 0), out(0, 103)); // Up
        assert_eq!(tap_output(0xA8, 0), out(0, 113)); // Mute
    }

    #[test]
    fn every_basic_key_the_board_labels_has_a_distinct_code() {
        let mut seen = std::collections::HashSet::new();
        for kc in (0x04..=0x52u8).filter(|k| !matches!(k, 0x32)) {
            let code = evdev_key(kc).unwrap_or_else(|| panic!("no code for {kc:#x}"));
            assert!(seen.insert(code), "duplicate code for {kc:#x}");
        }
    }

    #[test]
    fn compound_mods_ride_with_the_key() {
        assert_eq!(tap_output(0x0106, 0), out(0x01, KEY_C)); // C(KC_C)
        assert_eq!(tap_output(0x0214, 0), out(0x02, KEY_Q)); // S(KC_Q)
        assert_eq!(tap_output(0x0314, 0), out(0x03, KEY_Q)); // C+S
        assert_eq!(
            tap_output(0x1106, 0),
            out(0x01, KEY_C),
            "right ctrl is ctrl"
        );
        assert_eq!(tap_output(0x1806, 0), out(0x08, KEY_C), "right GUI is GUI");
    }

    #[test]
    fn tap_roles_of_dual_keys_output() {
        assert_eq!(tap_output(0x2106, 0), out(0, KEY_C)); // LCTL_T(KC_C)
        assert_eq!(tap_output(0x4233, 0), out(0, 39)); // LT(SYM, KC_SCLN)
    }

    #[test]
    fn latched_mods_ride_along_with_a_tap() {
        assert_eq!(tap_output(0x14, 0x02), out(0x02, KEY_Q));
        assert_eq!(tap_output(0x2106, 0x02), out(0x02, KEY_C));
        assert_eq!(tap_output(0x0106, 0x02), out(0x03, KEY_C));
        assert_eq!(tap_output(0x14, 0x11), out(0x01, KEY_Q), "right flag drops");
        assert_eq!(tap_output(0x14, 0x08), out(0x08, KEY_Q));
    }

    #[test]
    fn firmware_macros_send_their_linux_keys() {
        assert_eq!(firmware_macro_output(0x7E00), out(0x04, 30)); // Zoom mute: Alt+A
        assert_eq!(firmware_macro_output(0x7E01), out(0x04, 21)); // raise hand: Alt+Y
        assert_eq!(firmware_macro_output(0x7E02), out(0, 99)); // screenshot: Print
        assert_eq!(tap_output(0x7E00, 0), None, "plain QMK has no such macro");
    }

    #[test]
    fn state_only_keys_output_nothing() {
        assert_eq!(tap_output(0x0000, 0), None);
        assert_eq!(tap_output(0x0001, 0), None);
        assert_eq!(tap_output(0x00E0, 0), None); // LCTL
        assert_eq!(tap_output(0x00E7, 0), None); // RGUI
        assert_eq!(tap_output(0x5223, 0), None); // MO(NAV)
        assert_eq!(tap_output(0x7C00, 0), None); // bootloader
        assert_eq!(tap_output(0x7842, 0), None); // RGB
        assert_eq!(tap_output(0x7E03, 0), None); // GAME
    }
}
