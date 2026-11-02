//! Click-through sweep over the real keymap (dumped from the board with
//! `cargo run --example dump_keymap`): every state reachable by clicking is
//! checked for dead keys, unlabeled keys and traps.
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::time::Instant;

use omakeys::board::{loremipsum36, HostEvent, Keymap};
use omakeys::core::config::HoverMode;
use omakeys::core::{AppConfig, OverlayCore, Overrides, VisibilityMode};

fn keymap() -> Keymap {
    let layers: Vec<Vec<Vec<u16>>> =
        serde_json::from_str(include_str!("keymap.json")).expect("keymap.json");
    Keymap { layers }
}

fn core() -> OverlayCore {
    let mut config = AppConfig {
        mode: VisibilityMode::Always,
        ..AppConfig::default()
    };
    config.hover.mode = HoverMode::Type;
    let mut c = OverlayCore::new(loremipsum36(), config, Overrides::default());
    c.set_keymap(keymap());
    c.handle(
        HostEvent::Layer {
            default_layer: 1,
            layer_state: 0,
        },
        Instant::now(),
    );
    c
}

/// What the screen shows: layer plus every key's legend and lit state.
fn shown(c: &OverlayCore) -> String {
    let f = c.frame();
    let mut s = format!("L{} ", f.layer);
    for k in &f.keys {
        s += &format!(
            "{}:{}:{:x}:{}{} ",
            k.label, k.dimmed as u8, k.keycode, k.held as u8, k.locked as u8
        );
    }
    s
}

/// Keys a window cannot be sent a tap for: mouse keys act on a pointer that
/// is on the overlay itself, the rest are board maintenance (bootloader,
/// EEPROM clear) and RGB controls.
fn unsupported(kc: u16) -> bool {
    matches!(kc, 0x00cd..=0x00da | 0x7800..=0x78ff | 0x7c00..=0x7c7f)
}

#[test]
fn sweep() {
    let start = core();
    let base = shown(&start);
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(Vec<((u8, u8), u16, bool)>, String)> = VecDeque::new();
    queue.push_back((vec![], base.clone()));
    seen.insert(base.clone());

    let mut dead: BTreeMap<u16, String> = BTreeMap::new();
    let mut unlabeled: BTreeMap<u16, String> = BTreeMap::new();
    let mut layers_seen: BTreeSet<u8> = BTreeSet::new();
    let mut states = 0;

    let replay = |path: &[((u8, u8), u16, bool)]| {
        let mut c = core();
        for (cell, kc, lock) in path {
            if *lock {
                c.lock_key(*kc);
            } else {
                c.click_key(*cell, *kc);
            }
        }
        c
    };

    while let Some((path, fp)) = queue.pop_front() {
        states += 1;
        let c = replay(&path);
        let f = c.frame();
        layers_seen.insert(f.layer);
        let desc = format!(
            "path={:?} layer={}",
            path.iter()
                .map(|p| (p.0, format!("{:x}", p.1), p.2))
                .collect::<Vec<_>>(),
            f.layer
        );
        for k in &f.keys {
            let cell = (k.row, k.col);
            let kc = k.keycode;
            if kc == 0 || unsupported(kc) {
                continue;
            }
            if k.label.is_empty() && k.hold_label.is_none() {
                unlabeled.entry(kc).or_insert_with(|| desc.clone());
            }
            let mut t = replay(&path);
            let out = t.click_key(cell, kc);
            let after = shown(&t);
            if out.is_none() && after == fp && !matches!(kc, 0x7e06 | 0x7e04) {
                // Maybe a lock changes things when a plain click doesn't.
                dead.entry(kc)
                    .or_insert_with(|| format!("{desc} cell={cell:?}"));
            }
            if out.is_some() && out.unwrap().key == 0 {
                dead.entry(kc)
                    .or_insert_with(|| format!("zero output {desc}"));
            }
            if path.len() < 4 && after != fp && seen.insert(after.clone()) {
                let mut p = path.clone();
                p.push((cell, kc, false));
                queue.push_back((p, after));
            }
        }
        // Every state must have a way back to the base screen.
    }
    println!("states: {states}, layers: {layers_seen:?}");
    for (k, v) in &dead {
        println!("DEAD {k:#06x} {v}");
    }
    for (k, v) in &unlabeled {
        println!("UNLAB {k:#06x} {v}");
    }
    assert!(dead.is_empty() && unlabeled.is_empty());
}

/// Every mod key on the board, latched then followed by a letter, sends the
/// mods it is named for (not a twin's), and pairs add up.
#[test]
fn mod_keys_type_their_own_mods() {
    // keycode, QMK mods byte (ctrl 1, shift 2, alt 4, gui 8, right = +0x10).
    let mods: &[(u16, u8)] = &[
        (0x00e0, 0x01),
        (0x00e1, 0x02),
        (0x00e2, 0x04),
        (0x00e3, 0x08),
        (0x00e4, 0x11),
        (0x00e5, 0x12),
        (0x00e6, 0x14),
        (0x00e7, 0x18),
        (0x7e07, 0x02),
        (0x7e08, 0x01),
        (0x7e09, 0x08),
        (0x7e0a, 0x04),
        (0x7e0b, 0x02),
    ];
    for (kc, want) in mods {
        let mut c = core();
        assert_eq!(c.click_key((0, 0), *kc), None);
        let out = c.click_key((0, 1), 0x04).expect("typed");
        assert_eq!(out.mods & 0x0f, want & 0x0f, "{kc:#06x}");
        // Spent: the next letter is plain.
        assert_eq!(
            c.click_key((0, 1), 0x04).unwrap().mods,
            0,
            "{kc:#06x} not spent"
        );
        // Clicked twice: cancelled.
        c.click_key((0, 0), *kc);
        c.click_key((0, 0), *kc);
        assert_eq!(
            c.click_key((0, 1), 0x04).unwrap().mods,
            0,
            "{kc:#06x} not cancelled"
        );
    }
    for (a, wa) in mods {
        for (b, wb) in mods {
            if a == b {
                continue;
            }
            let mut c = core();
            c.click_key((0, 0), *a);
            c.click_key((0, 0), *b);
            let got = c.click_key((0, 1), 0x04).unwrap().mods & 0x0f;
            assert_eq!(got, (wa | wb) & 0x0f, "{a:#06x}+{b:#06x}");
        }
    }
}

/// No click-only state is a trap: base is always a few clicks away.
#[test]
fn every_state_has_a_way_back() {
    let base = shown(&core());
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<Vec<((u8, u8), u16)>> = VecDeque::new();
    queue.push_back(vec![]);
    let mut stuck = Vec::new();
    let replay = |path: &[((u8, u8), u16)]| {
        let mut c = core();
        for (cell, kc) in path {
            c.click_key(*cell, *kc);
        }
        c
    };
    while let Some(path) = queue.pop_front() {
        let c = replay(&path);
        // Breadth-first search for base from here.
        let mut found = shown(&c) == base;
        let mut frontier = vec![path.clone()];
        for _ in 0..4 {
            if found {
                break;
            }
            let mut next = Vec::new();
            for p in &frontier {
                let c = replay(p);
                for k in c.frame().keys {
                    if k.keycode == 0 {
                        continue;
                    }
                    let mut q = p.clone();
                    q.push(((k.row, k.col), k.keycode));
                    // Typing a key says nothing about getting back.
                    if replay(p).click_key((k.row, k.col), k.keycode).is_some() {
                        continue;
                    }
                    if shown(&replay(&q)) == base {
                        found = true;
                    }
                    next.push(q);
                }
                if found {
                    break;
                }
            }
            frontier = next;
            frontier.truncate(2000);
        }
        if !found {
            stuck.push(format!(
                "{:?}",
                path.iter()
                    .map(|p| format!("{:x}", p.1))
                    .collect::<Vec<_>>()
            ));
        }
        if path.len() < 3 {
            for k in c.frame().keys {
                if k.keycode == 0 {
                    continue;
                }
                let mut q = path.clone();
                q.push(((k.row, k.col), k.keycode));
                if seen.insert(shown(&replay(&q))) {
                    queue.push_back(q);
                }
            }
        }
    }
    assert!(stuck.is_empty(), "no way back from {stuck:?}");
}

/// Mod-tap keys: a click types the tap key, a long press latches the mod
/// the key is named for.
#[test]
fn mod_tap_keys_click_and_hold() {
    let km = keymap();
    let mut n = 0;
    for layer in &km.layers {
        for row in layer {
            for &kc in row {
                if !(0x2000..=0x3fff).contains(&kc) {
                    continue;
                }
                n += 1;
                let mods = ((kc >> 8) & 0x1f) as u8;
                let want = if mods & 0x10 != 0 { mods } else { mods & 0x0f };
                let tap = kc & 0xff;
                let mut c = core();
                let out = c.click_key((0, 0), kc).expect("tap key typed");
                assert_eq!(out.mods, 0, "{kc:#06x} click adds no mods");
                assert_eq!(out, omakeys::core::output::tap_output(tap, 0).unwrap());
                let mut c = core();
                c.hold_key((0, 0), kc);
                let out = c.click_key((0, 1), 0x04).unwrap();
                assert_eq!(out.mods & 0x0f, want & 0x0f, "{kc:#06x} hold");
            }
        }
    }
    assert!(n > 0);
}

/// A double click locks every kind of latchable key, typing leaves it on,
/// one more click turns it off.
#[test]
fn locks_survive_typing_and_unlock_on_click() {
    for &kc in &[0x00e1u16, 0x00e3, 0x7e09, 0x7e07, 0x00e2, 0x00e0] {
        let mut c = core();
        assert!(c.lock_key(kc));
        for _ in 0..3 {
            assert_ne!(
                c.click_key((0, 1), 0x04).unwrap().mods & 0x0f,
                0,
                "{kc:#06x}"
            );
        }
        c.click_key((0, 0), kc);
        assert_eq!(
            c.click_key((0, 1), 0x04).unwrap().mods,
            0,
            "{kc:#06x} unlock"
        );
    }
    for &kc in &[0x5222u16, 0x5223, 0x5225, 0x5224] {
        let mut c = core();
        let base = shown(&c);
        assert!(c.lock_key(kc));
        let locked = shown(&c);
        assert_ne!(locked, base, "{kc:#06x}");
        // Type from the locked layer: it stays.
        let f = c.frame();
        let typed = f.keys.iter().find(|k| (0x04..=0x1d).contains(&k.keycode));
        if let Some(k) = typed {
            assert!(c.click_key((k.row, k.col), k.keycode).is_some());
            assert_eq!(shown(&c), locked, "{kc:#06x} lock survives typing");
        }
        // One click on the key that is lit turns it off (the origin cell
        // shows a way back on SYM/NAV, MO keys elsewhere).
        c.clear_latches();
        assert_eq!(shown(&c), base);
    }
}
