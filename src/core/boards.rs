//! Several keyboards, one overlay. Each board reports on its own; the router
//! decides which one the overlay shows: the one you type on (`auto`), or the
//! one pinned in the config.

use std::collections::HashMap;
use std::time::Instant;

use crate::board::{BoardProfile, HostEvent, Keymap};

use super::state::OverlayCore;

pub struct BoardRouter {
    profiles: HashMap<String, BoardProfile>,
    keymaps: HashMap<String, Keymap>,
    /// Last layer report of each board, replayed when the overlay switches to it.
    layers: HashMap<String, HostEvent>,
    active: String,
}

impl BoardRouter {
    /// `active` is the board shown at start.
    pub fn new(profiles: Vec<BoardProfile>, active: &str) -> Self {
        Self {
            profiles: profiles.into_iter().map(|p| (p.name.clone(), p)).collect(),
            keymaps: HashMap::new(),
            layers: HashMap::new(),
            active: active.to_string(),
        }
    }

    pub fn active(&self) -> &str {
        &self.active
    }

    /// The board the config pins, if it names a known one.
    fn pinned<'a>(&self, setting: &'a str) -> Option<&'a str> {
        (setting != "auto" && self.profiles.contains_key(setting)).then_some(setting)
    }

    fn switch(&mut self, core: &mut OverlayCore, board: &str) {
        let Some(profile) = self.profiles.get(board).cloned() else {
            return;
        };
        let map = self.keymaps.get(board).cloned().unwrap_or_else(|| {
            Keymap::empty(
                usize::from(profile.max_layer_inclusive) + 1,
                usize::from(profile.rows),
                usize::from(profile.cols),
            )
        });
        self.active = board.to_string();
        core.set_board(profile, map);
        if let Some(layer) = self.layers.get(board) {
            core.handle(*layer, Instant::now());
        }
    }

    /// Merge freshly loaded profiles (e.g. a new `~/.config/omakeys/boards/*.toml`).
    /// Existing keymaps and layer cache stay; unknown names become selectable.
    pub fn merge_profiles(&mut self, profiles: Vec<BoardProfile>) {
        for p in profiles {
            self.profiles.insert(p.name.clone(), p);
        }
    }

    /// Apply the `board` setting now (startup, config reload).
    pub fn follow_setting(&mut self, core: &mut OverlayCore, setting: &str) {
        if let Some(board) = self.pinned(setting) {
            if board != self.active {
                self.switch(core, board);
            }
        }
    }

    pub fn set_keymap(&mut self, core: &mut OverlayCore, board: &str, map: Keymap) {
        if board == self.active {
            core.set_keymap(map.clone());
        }
        self.keymaps.insert(board.to_string(), map);
    }

    pub fn handle(
        &mut self,
        core: &mut OverlayCore,
        setting: &str,
        board: &str,
        events: Vec<HostEvent>,
        now: Instant,
    ) {
        for ev in events {
            if matches!(ev, HostEvent::Layer { .. }) {
                self.layers.insert(board.to_string(), ev);
            }
            if board != self.active {
                // Only a key press on another board takes the overlay over, and
                // never away from a pinned board.
                let takes_over = matches!(ev, HostEvent::Key { pressed: true, .. })
                    && self.pinned(setting).is_none_or(|p| p == board);
                if !takes_over {
                    continue;
                }
                self.switch(core, board);
            }
            core.handle(ev, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::evdev::evdev_layout;
    use crate::board::{generic, loremipsum36};
    use crate::core::config::AppConfig;
    use crate::core::overrides::Overrides;

    fn press() -> Vec<HostEvent> {
        vec![HostEvent::Key {
            row: 0,
            col: 0,
            pressed: true,
        }]
    }

    fn setup() -> (BoardRouter, OverlayCore) {
        let core = OverlayCore::new(generic(), AppConfig::default(), Overrides::default());
        let mut router = BoardRouter::new(vec![loremipsum36(), generic()], "generic");
        let mut core = core;
        router.set_keymap(&mut core, "generic", evdev_layout("generic").keymap);
        (router, core)
    }

    #[test]
    fn typing_on_another_board_switches_the_overlay() {
        let (mut router, mut core) = setup();
        let now = Instant::now();
        router.handle(&mut core, "auto", "loremipsum36", press(), now);
        assert_eq!(router.active(), "loremipsum36");
        assert_eq!(core.board_name(), "loremipsum36");
        router.handle(&mut core, "auto", "generic", press(), now);
        assert_eq!(core.board_name(), "generic");
    }

    #[test]
    fn releases_and_layer_reports_do_not_switch() {
        let (mut router, mut core) = setup();
        let now = Instant::now();
        let release = vec![HostEvent::Key { row: 0, col: 0, pressed: false }];
        let layer = vec![HostEvent::Layer { default_layer: 1, layer_state: 0b100 }];
        router.handle(&mut core, "auto", "loremipsum36", release, now);
        router.handle(&mut core, "auto", "loremipsum36", layer, now);
        assert_eq!(router.active(), "generic");
    }

    #[test]
    fn layer_seen_in_the_background_is_there_on_switch() {
        let (mut router, mut core) = setup();
        let now = Instant::now();
        let layer = vec![HostEvent::Layer { default_layer: 1, layer_state: 0b1000 }];
        router.handle(&mut core, "auto", "loremipsum36", layer, now);
        router.handle(&mut core, "auto", "loremipsum36", press(), now);
        assert_eq!(core.frame().layer, 3);
    }

    #[test]
    fn pinned_board_ignores_the_others() {
        let (mut router, mut core) = setup();
        let now = Instant::now();
        router.handle(&mut core, "generic", "loremipsum36", press(), now);
        assert_eq!(router.active(), "generic");
        // Pin the other board: the overlay moves at once, without a keypress.
        router.follow_setting(&mut core, "loremipsum36");
        assert_eq!(core.board_name(), "loremipsum36");
        router.handle(&mut core, "loremipsum36", "generic", press(), now);
        assert_eq!(core.board_name(), "loremipsum36");
    }

    #[test]
    fn unknown_pin_acts_like_auto() {
        let (mut router, mut core) = setup();
        router.handle(&mut core, "nope", "loremipsum36", press(), Instant::now());
        assert_eq!(router.active(), "loremipsum36");
    }

    #[test]
    fn merge_profiles_lets_a_new_pin_switch() {
        let (mut router, mut core) = setup();
        let extra = BoardProfile {
            name: "extra".into(),
            ..loremipsum36()
        };
        router.follow_setting(&mut core, "extra");
        assert_eq!(router.active(), "generic", "unknown pin is ignored");
        router.merge_profiles(vec![extra]);
        router.follow_setting(&mut core, "extra");
        assert_eq!(router.active(), "extra");
    }
}
