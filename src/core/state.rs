//! Pure overlay state: visibility, active layer, held keys, paint model.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::board::client::Keymap;
use crate::board::labels::{legend_for_keycode, LabelStyle};
use crate::board::profile::BoardProfile;
use crate::board::protocol::HostEvent;

use super::binds::{
    binds_for_mask, fallback_glyph, glyph_for, header_for, hypr_name, modmask_from_keycodes,
    modmask_from_qmk_bits, shift_label, Bind, MASK_SHIFT,
};
use super::config::{Anchor, AppConfig, HoverMode, VisibilityMode};
use super::output::{firmware_macro_output, tap_output, KeyOut};
use super::overrides::Overrides;

#[derive(Debug, Clone, PartialEq)]
pub struct KeyView {
    pub row: u8,
    pub col: u8,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Main (tap) legend.
    pub label: String,
    /// Hold legend for mod-tap / layer-tap (corner).
    pub hold_label: Option<String>,
    pub held: bool,
    /// Modifier / layer / tap-hold key — idle fill uses accent so keys that
    /// can change mods or layers stand out. Plain typing keys stay muted.
    pub special: bool,
    /// Host binding shown while a modifier is held.
    pub bind: Option<BindView>,
    /// Full text for the hover tooltip when the key text cannot speak for
    /// itself: bind description, readable name of an icon/emoji legend, or a
    /// hold role. Empty means nothing to show.
    pub tooltip: String,
    /// Keycode this cell would send if tapped right now: the frozen one while
    /// a key is physically down, otherwise the active layer's (a transparent
    /// cell resolves to the layer that provides the key). A type-mode click
    /// feeds this to `click_key`.
    pub keycode: u16,
    /// True when the legend is context rather than the point: the cell falls
    /// through to the layer below, or (type mode) a bind layer is shown and
    /// this key has no bind. Drawn dimmed to say so.
    pub dimmed: bool,
    /// True when a double click locked this mod or layer key; drawn with a
    /// bar so it reads differently from a one-shot latch.
    pub locked: bool,
}

/// One key's host binding on a modifier bind layer.
#[derive(Debug, Clone, PartialEq)]
pub struct BindView {
    pub glyph: Option<String>,
    pub description: String,
}

/// The modifier combo whose binds are being shown.
#[derive(Debug, Clone, PartialEq)]
pub struct BindLayer {
    pub mask: u8,
    pub header: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FrameKind {
    Layer,
    Binds(BindLayer),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub visible: bool,
    pub opacity: f64,
    pub scale: f64,
    pub anchor: Anchor,
    pub layer: u8,
    /// Name of the drawn layer, for the header.
    pub layer_name: String,
    pub kind: FrameKind,
    pub keys: Vec<KeyView>,
}

/// Key held since press: origin layer + keycode frozen at press time.
/// Freezing matters for layer-tap: QMK activates the hold layer on press, so
/// the live map would otherwise show the hold-layer keycap instead of the
/// action from the layer the press started on. While the key is down its
/// hold role is drawn centred; the frozen tap legend returns after release.
#[derive(Debug, Clone, Copy)]
struct HeldKey {
    from_layer: u8,
    keycode: u16,
    /// After release, keep the key lit until this instant so quick taps
    /// (press+release in one poll) still paint at least one frame.
    flash_until: Option<Instant>,
}

/// How long a released key stays highlighted (covers same-batch tap events).
const TAP_FLASH: Duration = Duration::from_millis(90);

pub struct OverlayCore {
    profile: BoardProfile,
    config: AppConfig,
    overrides: Overrides,
    keymap: Keymap,
    /// QMK `default_layer_state` bitmask (usually bit 0 = base).
    default_layer: u32,
    /// QMK `layer_state` overlay bitmask (excludes default layer).
    layer_state: u32,
    held: HashMap<(u8, u8), HeldKey>,
    /// Mod keys latched by type-mode clicks, by keycode. One-shot unless
    /// locked: the next typed click consumes them. Tracked per key, not per
    /// mod bit, so a key that merely sends the same mod (the one-hand "Cmd"
    /// is Ctrl off Mac) does not light up its twin. Physical board use clears
    /// them.
    mod_keys: HashSet<u16>,
    /// Subset of the latched mod keys and layers that survive a typed key:
    /// locked by a double click until clicked again.
    locked_mod_keys: HashSet<u16>,
    /// Layers latched by type-mode clicks (QMK layer bitmask), stacked on top
    /// of the board's own layers. One-shot like mods. Physical board use
    /// clears them.
    virtual_layers: u32,
    locked_layers: u32,
    /// Board layers a click switched off (QMK layer bitmask). The board stays
    /// in them, but the overlay and its clicks act as if it had left them.
    /// Cleared by physical board use, and per layer once the board itself
    /// leaves the layer.
    masked_layers: u32,
    /// Key cell (row, col) whose click latched each virtual layer. That cell
    /// is usually dead on the layer it opened (the board has you hold the key,
    /// the mouse cannot), so it shows a way back instead.
    layer_origins: HashMap<u8, (u8, u8)>,
    last_activity: Option<Instant>,
    /// Host bindings from the keybinding menu; empty until fetched.
    binds: Vec<Bind>,
    /// Shared with IPC so `ping` can report Live vs Hidden.
    force_hidden: Arc<AtomicBool>,
}

impl OverlayCore {
    pub fn new(profile: BoardProfile, config: AppConfig, overrides: Overrides) -> Self {
        let layers = (profile.max_layer_inclusive as usize) + 1;
        let keymap = Keymap::empty(layers, profile.rows as usize, profile.cols as usize);
        Self {
            profile,
            config: config.normalized(),
            overrides,
            keymap,
            default_layer: 0b1,
            layer_state: 0,
            held: HashMap::new(),
            mod_keys: HashSet::new(),
            locked_mod_keys: HashSet::new(),
            virtual_layers: 0,
            locked_layers: 0,
            masked_layers: 0,
            layer_origins: HashMap::new(),
            last_activity: None,
            binds: Vec::new(),
            force_hidden: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn force_hidden_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.force_hidden)
    }

    pub fn is_force_hidden(&self) -> bool {
        self.force_hidden.load(Ordering::Relaxed)
    }

    pub fn set_keymap(&mut self, map: Keymap) {
        self.keymap = map;
    }

    /// Show another board: its geometry and keymap, with no state carried
    /// over from the board shown before.
    pub fn set_board(&mut self, profile: BoardProfile, keymap: Keymap) {
        self.profile = profile;
        self.keymap = keymap;
        self.default_layer = 0b1;
        self.layer_state = 0;
        self.held.clear();
        self.clear_latches();
        self.layer_origins.clear();
    }

    pub fn board_name(&self) -> &str {
        &self.profile.name
    }

    pub fn set_config(&mut self, config: AppConfig) {
        self.config = config.normalized();
    }

    pub fn set_overrides(&mut self, o: Overrides) {
        self.overrides = o;
    }

    pub fn set_binds(&mut self, binds: Vec<Bind>) {
        self.binds = binds;
    }

    /// Pointer hover counts as activity so Auto mode does not hide the overlay
    /// out from under the cursor while a tooltip is being read.
    pub fn note_activity(&mut self, now: Instant) {
        self.last_activity = Some(now);
    }

    /// Modmask of the modifiers physically held right now, plus mods latched
    /// by type-mode clicks. Release flashes do not count: a tapped mod-tap key
    /// must not keep its bind layer up.
    pub fn held_modmask(&self) -> u8 {
        let codes: Vec<u16> = self
            .held
            .values()
            .filter(|h| h.flash_until.is_none())
            .map(|h| h.keycode)
            .collect();
        modmask_from_keycodes(&codes) | modmask_from_qmk_bits(self.virtual_mods())
    }

    /// Bind layer for the exact held modmask, if it has anything to show.
    ///
    /// Binds are resolved per cell against the keycode that cell would send
    /// right now, so a key only advertises what it would actually trigger.
    fn resolve_bind_layer(&self, mask: u8) -> Option<(BindLayer, HashMap<String, String>)> {
        if !self.config.binds.enabled || mask == 0 {
            return None;
        }
        let entries = binds_for_mask(&self.binds, mask, self.config.binds.standard);
        if entries.is_empty() && mask != MASK_SHIFT {
            return None;
        }
        Some((
            BindLayer {
                mask,
                header: header_for(mask),
            },
            entries,
        ))
    }

    /// Glyph and description for a bind description, honouring glyph overrides.
    fn bind_desc_view(&self, description: &str) -> BindView {
        let glyph = match self.overrides.bind_glyph(description) {
            Some(glyph) => glyph.map(str::to_string),
            None => Some(glyph_for(description).unwrap_or_else(|| fallback_glyph(description))),
        };
        BindView {
            glyph,
            description: description.to_string(),
        }
    }

    /// Bind content follows the label style: icons show the glyph only, text
    /// shows the description only. A suppressed glyph (override) falls back
    /// to the description so the key never goes blank.
    fn bind_view(&self, view: BindView) -> BindView {
        match self.config.label_style {
            LabelStyle::Text => BindView {
                glyph: None,
                description: view.description,
            },
            LabelStyle::Icons => {
                if view.glyph.is_some() {
                    BindView {
                        glyph: view.glyph,
                        description: String::new(),
                    }
                } else {
                    view
                }
            }
        }
    }

    pub fn toggle_force_hidden(&mut self) {
        let next = !self.is_force_hidden();
        self.force_hidden.store(next, Ordering::Relaxed);
    }

    pub fn handle(&mut self, ev: HostEvent, now: Instant) {
        match ev {
            HostEvent::Layer {
                default_layer,
                layer_state,
            } => {
                // Layer polls (keepalive / LayerLens) must not count as
                // activity — they fire every ~1.5s and would pin Auto mode
                // visible forever when timeout is only a few seconds.
                if default_layer != 0 {
                    self.default_layer = default_layer;
                }
                self.layer_state = layer_state;
                self.masked_layers &= layer_state;
            }
            HostEvent::Key { row, col, pressed } => {
                if pressed {
                    // Only real presses (and releases of keys we were tracking)
                    // count as activity. Spurious F releases forged from
                    // LayerLens state=0 keepalive must not reset Auto timeout.
                    self.last_activity = Some(now);
                    // Back on real hardware: overlay latches would desync the
                    // mirror, so they end here.
                    self.clear_latches();
                    let (from_layer, keycode) = resolve_press_keycode(
                        &self.profile,
                        &self.keymap,
                        self.default_layer,
                        self.layer_state,
                        row,
                        col,
                    );
                    self.held.insert(
                        (row, col),
                        HeldKey {
                            from_layer,
                            keycode,
                            flash_until: None,
                        },
                    );
                } else if let Some(h) = self.held.get_mut(&(row, col)) {
                    self.last_activity = Some(now);
                    // Fields, not clear_latches(): `h` borrows self.held.
                    self.mod_keys.clear();
                    self.locked_mod_keys.clear();
                    self.virtual_layers = 0;
                    self.locked_layers = 0;
                    self.masked_layers = 0;
                    // Arm flash once. Re-arming on repeat releases (e.g. phantom
                    // LayerLens/F collisions) would keep the key lit forever.
                    if h.flash_until.is_none() {
                        h.flash_until = Some(now + TAP_FLASH);
                    }
                }
            }
        }
    }

    pub fn tick(&mut self, now: Instant) {
        self.held
            .retain(|_, h| h.flash_until.map(|t| now < t).unwrap_or(true));
    }

    /// A type-mode click on the overlay. Dual-role keys (mod-tap, layer-tap)
    /// type their tap key: their hold role comes from `hold_key`. A mod or
    /// momentary layer key latches
    /// one-shot (cleared by the next typed key) or, if already latched,
    /// switches off. Toggle, exclusive and firmware layer keys switch a layer
    /// for good, like on the board. Anything else types.
    pub fn click_key(&mut self, cell: (u8, u8), kc: u16) -> Option<KeyOut> {
        if mod_bit(kc, &self.profile).is_some() {
            self.toggle_mod_key(kc);
            return None;
        }
        if let Some(key) = layer_key(kc, &self.profile).filter(|_| !is_dual_role(kc)) {
            let bit = 1u32 << key.target();
            match key {
                LayerKey::Momentary(_) => self.toggle_layer(key.target(), cell),
                LayerKey::Toggle(_) if self.effective_layer_state() & bit != 0 => {
                    self.layer_off(bit)
                }
                LayerKey::Toggle(_) | LayerKey::On(_) => self.lock_layer(bit),
                LayerKey::Exclusive(_) => {
                    self.virtual_layers = 0;
                    self.locked_layers = 0;
                    self.masked_layers = self.layer_state;
                    if key.target() != 0 {
                        self.lock_layer(bit);
                    }
                }
                LayerKey::Off(_) => self.layer_off(bit),
            }
            // A key that switches a layer for good is an action like a typed
            // key: the one-shot layer or mod used to reach it (QWERTY and GAME
            // sit on the ADJ layer) is spent, so the new layer shows.
            if !matches!(key, LayerKey::Momentary(_)) {
                self.spend_one_shots();
            }
            return None;
        }
        let out = firmware_macro_output(kc)
            .filter(|_| self.profile.custom_keycodes)
            .or_else(|| tap_output(kc, self.virtual_mods()));
        if out.is_some() {
            self.spend_one_shots();
        }
        out
    }

    /// One-shot latches are used up; locks stay.
    fn spend_one_shots(&mut self) {
        let locked = &self.locked_mod_keys;
        self.mod_keys.retain(|kc| locked.contains(kc));
        self.virtual_layers &= self.locked_layers;
    }

    /// A double click on a mod or layer key: lock it until clicked again.
    /// Returns false for keys that cannot latch. Layer keys that already
    /// stay put (toggle, exclusive, firmware) simply stay on.
    pub fn lock_key(&mut self, kc: u16) -> bool {
        if mod_bit(kc, &self.profile).is_some() {
            self.mod_keys.insert(kc);
            self.locked_mod_keys.insert(kc);
        } else if let Some(key) = layer_key(kc, &self.profile).filter(|_| !is_dual_role(kc)) {
            if !matches!(key, LayerKey::Off(_)) {
                self.lock_layer(1 << key.target());
            }
        } else {
            return false;
        }
        true
    }

    /// A type-mode long-press on a dual-role key: latch its hold role one-shot.
    /// A mod-tap key latches its mod like a clicked mod key, a layer-tap key
    /// latches its layer like a clicked layer key.
    pub fn hold_key(&mut self, cell: (u8, u8), kc: u16) {
        if (0x2000..=0x3FFF).contains(&kc) {
            self.toggle_mod_key(kc);
        } else if let Some(target) = layer_switch_target(kc, &self.profile) {
            self.toggle_layer(target, cell);
        }
    }

    /// Drop every click latch (mods and layers, locks too), back to
    /// mirroring the board.
    pub fn clear_latches(&mut self) {
        self.mod_keys.clear();
        self.locked_mod_keys.clear();
        self.virtual_layers = 0;
        self.locked_layers = 0;
        self.masked_layers = 0;
    }

    /// Latch a mod key one-shot, or switch it off if already latched.
    fn toggle_mod_key(&mut self, kc: u16) {
        if self.mod_keys.remove(&kc) {
            self.locked_mod_keys.remove(&kc);
        } else {
            self.mod_keys.insert(kc);
        }
    }

    /// QMK mods byte of everything latched by clicks.
    fn virtual_mods(&self) -> u8 {
        self.mod_keys
            .iter()
            .fold(0, |bits, kc| bits | latch_mods(*kc, &self.profile))
    }

    fn toggle_layer(&mut self, target: u8, cell: (u8, u8)) {
        let bit = 1u32 << target;
        if self.virtual_layers & bit != 0 {
            self.unlock_layer(bit);
        } else {
            self.virtual_layers |= bit;
            self.layer_origins.insert(target, cell);
        }
    }

    fn lock_layer(&mut self, bit: u32) {
        self.virtual_layers |= bit;
        self.locked_layers |= bit;
        self.masked_layers &= !bit;
    }

    /// Switch a layer off for good, even one the board itself holds on (the
    /// one-hand and game off keys): the board stays put, the overlay leaves.
    fn layer_off(&mut self, bit: u32) {
        self.unlock_layer(bit);
        self.masked_layers |= bit & self.layer_state;
    }

    fn unlock_layer(&mut self, bit: u32) {
        self.virtual_layers &= !bit;
        self.locked_layers &= !bit;
    }

    /// Board layers (minus ones a click switched off) plus click-latched ones
    /// (QMK overlay bitmask).
    fn effective_layer_state(&self) -> u32 {
        (self.layer_state & !self.masked_layers) | self.virtual_layers
    }

    pub fn active_layer(&self) -> Option<u8> {
        let state = self.effective_layer_state();
        let layer = if state == 0 {
            default_layer_index(self.default_layer)
        } else {
            highest_layer_bit(state)
        };
        if layer > self.profile.max_layer_inclusive {
            return None;
        }
        Some(layer)
    }

    /// True while this key's mod bit or layer target is latched by a click.
    fn virtual_held(&self, kc: u16) -> bool {
        self.virtual_lit(kc, &self.mod_keys, self.virtual_layers)
    }

    /// True while this key's mod bit or layer target is locked by a click.
    fn virtual_locked(&self, kc: u16) -> bool {
        self.virtual_lit(kc, &self.locked_mod_keys, self.locked_layers)
    }

    fn virtual_lit(&self, kc: u16, mods: &HashSet<u16>, layers: u32) -> bool {
        if mods.contains(&kc) {
            return true;
        }
        match layer_key(kc, &self.profile) {
            Some(LayerKey::Off(_)) | None => false,
            Some(key) => layers & (1 << key.target()) != 0,
        }
    }

    /// MO key for the highest click-latched layer that this cell latched.
    fn way_back_key(&self, row: u8, col: u8) -> Option<u16> {
        (0..32u8)
            .rev()
            .find(|l| {
                self.virtual_layers & (1 << l) != 0
                    && self.layer_origins.get(l) == Some(&(row, col))
            })
            .map(|l| 0x5220 + u16::from(l))
    }

    /// Walk fall-through (`KC_TRNS`) cells down the active layers, then the
    /// default layer, like QMK does, so the drawn label, tooltip and click
    /// all match what the key really does. `KC_NO` stops the walk. Returns
    /// the providing layer and keycode.
    fn resolve_fallthrough(&self, layer: u8, row: u8, col: u8) -> (u8, u16) {
        let kc = self.keymap.get(layer, row, col);
        if kc != 0x0001 {
            return (layer, kc);
        }
        let active = self.effective_layer_state();
        let default = default_layer_index(self.default_layer);
        let below = (0..layer)
            .rev()
            .filter(|l| active & (1 << l) != 0 || *l == default);
        for l in below {
            let kc = self.keymap.get(l, row, col);
            if kc != 0x0001 {
                return (l, kc);
            }
        }
        (layer, kc)
    }

    pub fn frame(&self) -> Frame {
        let layer = self.active_layer().unwrap_or(0);
        let skip = self.active_layer().is_none();
        let visible = if self.is_force_hidden() || skip {
            false
        } else {
            match self.config.mode {
                VisibilityMode::Hidden => false,
                VisibilityMode::Always => true,
                VisibilityMode::Auto => match self.last_activity {
                    Some(t) => t.elapsed() < Duration::from_millis(self.config.timeout_ms),
                    None => false,
                },
            }
        };

        let mask = self.held_modmask();
        let resolved = self.resolve_bind_layer(mask);
        // A bind layer only takes over when it would actually show a bind on
        // some board key (Shift always counts: it transforms typed output).
        let bind_mode = resolved.as_ref().is_some_and(|(_, entries)| {
            mask == MASK_SHIFT
                || self.profile.keys.iter().any(|k| {
                    let held = self.held.get(&(k.row, k.col));
                    let pressed = held.is_some_and(|h| h.flash_until.is_none());
                    if pressed {
                        return false;
                    }
                    let kc = match held {
                        Some(h) if h.keycode != 0x0001 => h.keycode,
                        _ => self.resolve_fallthrough(layer, k.row, k.col).1,
                    };
                    hypr_name(kc).is_some_and(|name| entries.contains_key(&name))
                })
        });
        let kind = match &resolved {
            Some((layer_meta, _)) if bind_mode => FrameKind::Binds(layer_meta.clone()),
            _ => FrameKind::Layer,
        };
        let keys = if skip {
            Vec::new()
        } else {
            self.profile
                .keys
                .iter()
                .map(|k| {
                    let held = self.held.get(&(k.row, k.col)).copied();
                    let (label_layer, kc, fallthrough) = match held {
                        Some(h) if h.keycode == 0x0001 => {
                            let (l, kc) = self.resolve_fallthrough(h.from_layer, k.row, k.col);
                            (l, kc, l != h.from_layer)
                        }
                        Some(h) => (h.from_layer, h.keycode, false),
                        None => {
                            let (l, kc) = self.resolve_fallthrough(layer, k.row, k.col);
                            (l, kc, l != layer)
                        }
                    };
                    // A dead cell that latched a layer becomes its way back.
                    let kc = if kc == 0x0000 && held.is_none() {
                        self.way_back_key(k.row, k.col).unwrap_or(kc)
                    } else {
                        kc
                    };
                    let (mut label, hold_label, overridden) = if let Some(over) =
                        self.overrides.get(label_layer, k.row, k.col)
                    {
                        (over.to_string(), None, true)
                    } else {
                        let leg = legend_for_keycode(kc, self.config.label_style, &self.profile);
                        (leg.primary, leg.hold, false)
                    };
                    // While physically down, a dual-role key shows what it is
                    // doing: the hold action centred, like a pure MO key.
                    // The frozen tap legend comes back for the release flash.
                    let pressed = held.map(|h| h.flash_until.is_none()).unwrap_or(false);
                    // Only idle keys advertise a host bind: a held key keeps
                    // its own presentation (centred hold role, held accent).
                    let bind_desc = if pressed {
                        None
                    } else {
                        resolved.as_ref().and_then(|(_, entries)| {
                            hypr_name(kc).and_then(|name| entries.get(&name).cloned())
                        })
                    };
                    let bind = bind_desc
                        .clone()
                        .map(|description| self.bind_view(self.bind_desc_view(&description)));
                    // On a bind layer an unbound key stays blank: it only ever
                    // shows what it would trigger. Shift keeps its typed-output
                    // transform; overrides win over the transform.
                    let mut shifted_applied = false;
                    let mut dim = false;
                    if bind.is_none() && bind_mode && !pressed {
                        let shifted = if mask == MASK_SHIFT && !overridden {
                            shift_label(kc)
                        } else {
                            None
                        };
                        if let Some(shifted) = shifted {
                            label = shifted;
                            shifted_applied = true;
                        } else if self.config.hover.mode == HoverMode::Type {
                            // The on-screen keyboard keeps keys readable
                            // (dimmed) so a click is never blind.
                            dim = true;
                        } else {
                            label.clear();
                        }
                    }
                    let hold_swapped = pressed && hold_label.is_some();
                    let (label, hold_label) = match (pressed, hold_label) {
                        (true, Some(hold)) => (hold, None),
                        (_, hold) => (label, hold),
                    };
                    // Tooltips only earn their keep when the drawn text cannot
                    // speak for itself: bind actions, icon/emoji legends (the
                    // readable Text legend becomes the tooltip) and hold roles.
                    // Plain letters, digits and shifted symbols stay quiet.
                    let text_leg = legend_for_keycode(kc, LabelStyle::Text, &self.profile);
                    let mut tooltip = if let Some(description) = bind_desc.filter(|d| !d.is_empty())
                    {
                        description
                    } else if hold_swapped {
                        text_leg.hold.clone().unwrap_or_default()
                    } else if label.is_empty() || overridden || shifted_applied {
                        String::new()
                    } else if label != text_leg.primary {
                        text_leg.primary.clone()
                    } else {
                        String::new()
                    };
                    if let Some(hold) = hold_label.as_deref().filter(|h| !h.is_empty()) {
                        let name = text_leg.hold.as_deref().unwrap_or(hold);
                        if tooltip.is_empty() {
                            tooltip = label.clone();
                        }
                        tooltip = if tooltip.is_empty() {
                            format!("hold {name}")
                        } else {
                            format!("{tooltip} \u{b7} hold {name}")
                        };
                    }
                    let special = is_modifier_or_layer_key(kc, &self.profile);
                    KeyView {
                        row: k.row,
                        col: k.col,
                        x: k.x,
                        y: k.y,
                        w: k.w,
                        h: k.h,
                        label,
                        hold_label,
                        held: held.is_some() || self.virtual_held(kc),
                        special,
                        bind,
                        tooltip,
                        keycode: kc,
                        dimmed: fallthrough || dim,
                        locked: self.virtual_locked(kc),
                    }
                })
                .collect()
        };

        Frame {
            visible,
            opacity: self.config.opacity,
            scale: self.config.scale,
            anchor: self.config.anchor,
            layer,
            layer_name: self.profile.layer_label(layer),
            kind,
            keys,
        }
    }
}

fn highest_layer_bit(state: u32) -> u8 {
    if state == 0 {
        return 0;
    }
    31 - state.leading_zeros() as u8
}

fn default_layer_index(default_layer: u32) -> u8 {
    if default_layer == 0 {
        0
    } else {
        highest_layer_bit(default_layer)
    }
}

/// Pick the keycode to freeze on press.
///
/// QMK `layer_state` is overlay-only. When Layer arrives before Key (LT/MO/
/// OSL already active), the top layer's key at this cell is the *destination*
/// key — not the dual/MO that was pressed. Prefer the keycode whose switch
/// target equals the active layer, scanning the active overlay layers from
/// highest to lowest, then the default layer.
fn resolve_press_keycode(
    profile: &BoardProfile,
    map: &Keymap,
    default_layer: u32,
    layer_state: u32,
    row: u8,
    col: u8,
) -> (u8, u16) {
    let def = default_layer_index(default_layer);
    let active = if layer_state == 0 {
        def
    } else {
        highest_layer_bit(layer_state)
    };
    let mut candidates: Vec<u8> = Vec::new();
    for layer in (0..32).rev() {
        if layer_state & (1 << layer) != 0 {
            candidates.push(layer as u8);
        }
    }
    if !candidates.contains(&def) {
        candidates.push(def);
    }
    for layer in candidates {
        let kc = map.get(layer, row, col);
        if let Some(target) = layer_switch_target(kc, profile) {
            if target == active {
                return (layer, kc);
            }
        }
    }
    (active, map.get(active, row, col))
}

/// QMK mod bit latched by a click on a standalone mod key. Left and right
/// hand keys share one bit: the latches feed the virtual keyboard mods byte, which only
/// knows the four canonical modifiers.
fn mod_bit(kc: u16, profile: &BoardProfile) -> Option<u8> {
    match kc {
        0x00E0..=0x00E7 => Some(1 << ((kc - 0x00E0) & 3) as u8),
        // One-hand mode mods. The one-hand "Cmd" is Super here, like the base
        // layer's GUI key and its own Super icon (the firmware itself sends
        // Ctrl off Mac), and its one-shot shift behaves like a clicked Shift.
        0x7E07 | 0x7E0B if profile.custom_keycodes => Some(0x02),
        0x7E08 if profile.custom_keycodes => Some(0x01),
        0x7E09 if profile.custom_keycodes => Some(0x08),
        0x7E0A if profile.custom_keycodes => Some(0x04),
        _ => None,
    }
}

/// QMK mods byte a latched key contributes: a mod key's bit, or the hold mods
/// of a mod-tap key.
fn latch_mods(kc: u16, profile: &BoardProfile) -> u8 {
    match kc {
        0x2000..=0x3FFF => ((kc >> 8) & 0x0F) as u8,
        _ => mod_bit(kc, profile).unwrap_or(0),
    }
}

/// Dual-role keycodes: mod-tap (hold a mod, tap a key) and layer-tap (hold a
/// layer, tap a key). A held click latches their hold role.
pub fn is_dual_role(kc: u16) -> bool {
    (0x2000..=0x4FFF).contains(&kc)
}

/// How a layer key switches layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerKey {
    /// LT / MO / OSL: on while held (one-shot for a click).
    Momentary(u8),
    /// TG: flips on and off, staying put.
    Toggle(u8),
    /// TO / DF: this layer only, staying put.
    Exclusive(u8),
    /// Firmware custom key that turns a layer on (GAME, one-hand).
    On(u8),
    /// Firmware custom key that turns a layer off.
    Off(u8),
}

impl LayerKey {
    fn target(self) -> u8 {
        match self {
            Self::Momentary(t)
            | Self::Toggle(t)
            | Self::Exclusive(t)
            | Self::On(t)
            | Self::Off(t) => t,
        }
    }
}

/// Layer-switching role of a keycode, if it has one. The firmware's own
/// custom keys (rsta: `QK_KB_3..6`) switch the game and one-hand layers.
fn layer_key(kc: u16, profile: &BoardProfile) -> Option<LayerKey> {
    match kc {
        0x4000..=0x4FFF => Some(LayerKey::Momentary(((kc >> 8) & 0x0F) as u8)), // LT
        0x5200..=0x521F => Some(LayerKey::Exclusive((kc - 0x5200) as u8)),      // TO
        0x5220..=0x523F => Some(LayerKey::Momentary((kc - 0x5220) as u8)),      // MO
        0x5240..=0x525F => Some(LayerKey::Exclusive((kc - 0x5240) as u8)),      // DF
        0x5260..=0x527F => Some(LayerKey::Toggle((kc - 0x5260) as u8)),         // TG
        0x5280..=0x529F => Some(LayerKey::Momentary((kc - 0x5280) as u8)),      // OSL
        0x7E03 if profile.custom_keycodes => Some(LayerKey::On(profile.game_layer)),
        0x7E04 if profile.custom_keycodes => Some(LayerKey::Off(profile.game_layer)),
        0x7E05 if profile.custom_keycodes => Some(LayerKey::On(profile.one_hand_layer)),
        0x7E06 if profile.custom_keycodes => Some(LayerKey::Off(profile.one_hand_layer)),
        _ => None,
    }
}

/// Layer index activated by LT / MO / DF / TG / OSL, if this keycode is one
/// of those.
fn layer_switch_target(kc: u16, profile: &BoardProfile) -> Option<u8> {
    layer_key(kc, profile).map(LayerKey::target)
}

/// Accent idle fill for keys that can change mods or layers: standalone
/// mods, mod-tap, layer switches (TO / MO / DF / TG / OSL), and rsta
/// custom keys that switch modes (Game / 1H toggles) or act as mods.
fn is_modifier_or_layer_key(kc: u16, profile: &BoardProfile) -> bool {
    match kc {
        0x00E0..=0x00E7 => true,                    // KC_LCTL … KC_RGUI
        0x2000..=0x3FFF => true,                    // MT / mod-tap
        0x4000..=0x4FFF => true,                    // LT
        0x5200..=0x529F => true,                    // TO / MO / DF / TG / OSL (incl. Trio Fn)
        0x7E03..=0x7E0B => profile.custom_keycodes, // GAM_ON/OFF, FV_ON/OFF, 1H mods (QK_KB_3..11)
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::profile::loremipsum36;
    use crate::core::binds::{parse_binds, MASK_CTRL};
    use crate::core::config::BindsConfig;
    use crate::core::output::KEY_Q;

    /// Cell the tests click from when the position does not matter.
    const C: (u8, u8) = (0, 0);

    fn always_core() -> OverlayCore {
        OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                hover: crate::core::config::HoverConfig {
                    mode: HoverMode::Dodge,
                },
                ..AppConfig::default()
            },
            Overrides::default(),
        )
    }

    /// Letters on the base layer, one digit, and the thumbs used by the bind
    /// layer tests.
    fn letter_map() -> Keymap {
        let mut map = Keymap::empty(14, 4, 10);
        map.set(0, 0, 0, 0x14); // Q
        map.set(0, 0, 1, 0x06); // C
        map.set(0, 0, 2, 0x09); // F
        map.set(0, 0, 3, 0x0A); // G
        map.set(0, 0, 4, 0x1B); // X
        map.set(0, 1, 0, 0x1E); // 1
        map.set(0, 3, 2, 0x00E3); // LGUI thumb
        map.set(0, 3, 4, 0x00E1); // LSFT thumb
        map.set(0, 3, 6, 0x5223); // MO(NAV) thumb
        map
    }

    #[test]
    fn combo_with_no_board_keys_falls_back_to_layer() {
        let mut core = always_core();
        let mut map = Keymap::empty(14, 4, 10);
        map.set(0, 0, 0, 0x04); // A only
        map.set(0, 3, 2, 0x00E3); // LGUI thumb
        core.set_keymap(map);
        core.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        assert!(matches!(frame.kind, FrameKind::Layer));
    }

    #[test]
    fn held_mod_tap_key_keeps_hold_legend_on_ctrl_layer() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(0, 0, 1, 0x2106); // LCTL_T(KC_C)
        core.set_keymap(map);
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 1,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        assert!(matches!(frame.kind, FrameKind::Binds(ref b) if b.header == "CTRL"));
        let c = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 1)
            .unwrap();
        assert!(c.held);
        assert!(c.bind.is_none(), "held mod-tap key keeps its hold legend");
        assert_eq!(c.label, "\u{2303}", "hold role drawn centred while held");
        assert!(
            c.hold_label.is_none(),
            "no separate corner label while held"
        );
        let f = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 2)
            .unwrap();
        assert_eq!(
            f.bind.as_ref().and_then(|b| b.glyph.as_deref()),
            Some("\u{f002}")
        );
        let x = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 4)
            .unwrap();
        assert_eq!(
            x.bind.as_ref().and_then(|b| b.glyph.as_deref()),
            Some("\u{f0c4}"),
            "uncovered Ctrl hints still show"
        );
    }

    #[test]
    fn key_view_carries_the_drawn_keycode() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(0, 0, 1, 0x2106); // LCTL_T(KC_C)
        core.set_keymap(map);
        let frame = core.frame();
        let q = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap();
        assert_eq!(q.keycode, 0x14);
        let c = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 1)
            .unwrap();
        assert_eq!(c.keycode, 0x2106, "dual-role key sends its tap");
        let gui = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 2)
            .unwrap();
        assert_eq!(gui.keycode, 0x00E3, "mod keys still carry their keycode");
    }

    #[test]
    fn transparent_cells_show_and_click_the_key_below() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(2, 0, 0, 0x0001); // SYM and the layer between fall through to Q
        map.set(1, 0, 0, 0x0001);
        map.set(2, 0, 1, 0x0000); // KC_NO blocks C
        core.set_keymap(map);
        let _ = core.click_key(C, 0x5222); // MO(2), latched for the test
        let frame = core.frame();
        let q = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap();
        assert_eq!(q.keycode, 0x14);
        assert!(q.dimmed);
        assert_eq!(
            q.label,
            legend_for_keycode(0x14, LabelStyle::Icons, &loremipsum36()).primary,
            "the OSK shows what the key types"
        );
        let c = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 1)
            .unwrap();
        assert_eq!(c.keycode, 0x0000);
        assert!(!c.dimmed, "KC_NO stops the fall-through");
        let out = core.click_key(C, q.keycode).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 0,
                key: KEY_Q
            }
        );
    }

    #[test]
    fn click_on_mod_key_latches_one_shot() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        assert!(
            core.click_key(C, 0x00E1).is_none(),
            "mod click sends nothing"
        );
        let shift = core
            .frame()
            .keys
            .into_iter()
            .find(|k| k.row == 3 && k.col == 4)
            .unwrap();
        assert!(shift.held, "latched mod shows as held");
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 2,
                key: KEY_Q
            }
        );
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 0,
                key: KEY_Q
            },
            "one-shot mod is consumed by the next typed click"
        );
        let _ = core.click_key(C, 0x00E0);
        let _ = core.click_key(C, 0x00E1);
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 3,
                key: KEY_Q
            },
            "clicked mods stack until a key is typed"
        );
        let _ = core.click_key(C, 0x00E1);
        let _ = core.click_key(C, 0x00E5);
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 2,
                key: KEY_Q
            },
            "two keys with the same mod latch on their own"
        );
    }

    #[test]
    fn click_on_mod_key_again_cancels_the_latch() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x00E1);
        let _ = core.click_key(C, 0x00E1);
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 0,
                key: KEY_Q
            }
        );
    }

    #[test]
    fn clicked_mods_show_their_own_bind_layer() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x00E0);
        assert!(
            matches!(core.frame().kind, FrameKind::Binds(ref b) if b.header == "CTRL"),
            "a latched ctrl must not read as shift"
        );
        let _ = core.click_key(C, 0x00E1);
        core.set_binds(vec![Bind {
            mask: MASK_CTRL | MASK_SHIFT,
            key: "X".to_string(),
            description: "Cut".to_string(),
        }]);
        assert!(matches!(core.frame().kind, FrameKind::Binds(ref b) if b.header == "CTRL + SHIFT"));
    }

    #[test]
    fn long_pressed_mod_tap_key_shows_its_bind_layer() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.hold_key(C, 0x2114); // LCTL_T(KC_Q)
        assert!(matches!(core.frame().kind, FrameKind::Binds(ref b) if b.header == "CTRL"));
    }

    #[test]
    fn click_on_layer_key_switches_and_switches_back() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        assert!(
            core.click_key(C, 0x5223).is_none(),
            "layer click sends nothing"
        );
        assert_eq!(core.frame().layer, 3, "MO(NAV) click shows NAV");
        let _ = core.click_key(C, 0x5223);
        assert_eq!(core.frame().layer, 0);
    }

    #[test]
    fn long_press_on_mod_tap_key_latches_its_mod() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(0, 0, 1, 0x2106); // LCTL_T(KC_C)
        core.set_keymap(map);
        core.hold_key(C, 0x2106);
        core.hold_key(C, 0x2106); // the same key again cancels
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 0,
                key: KEY_Q
            }
        );
        core.hold_key(C, 0x2106);
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 1,
                key: KEY_Q
            },
            "held mod-tap latches its mod one-shot"
        );
    }

    #[test]
    fn long_press_on_layer_tap_key_switches_layer_sticky() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(0, 0, 1, 0x4304); // LT(3, KC_A) = NAV
        core.set_keymap(map);
        core.hold_key(C, 0x4304);
        assert_eq!(core.frame().layer, 3, "held layer-tap shows its layer");
        core.hold_key(C, 0x4304);
        assert_eq!(core.frame().layer, 0, "again switches back");
    }

    #[test]
    fn long_press_on_a_plain_key_does_nothing() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.hold_key(C, 0x14);
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 0,
                key: KEY_Q
            }
        );
    }

    #[test]
    fn physical_board_use_clears_clicked_state() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x00E1); // shift latch
        let _ = core.click_key(C, 0x5223); // NAV override
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        assert_eq!(frame.layer, 0, "board use resumes the mirror");
        let shift = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 4)
            .unwrap();
        assert!(!shift.held, "board use clears the latch");
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(
            out,
            KeyOut {
                mods: 0,
                key: KEY_Q
            }
        );
    }

    #[test]
    fn type_mode_keeps_unbound_keys_readable_on_a_bind_layer() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                hover: crate::core::config::HoverConfig {
                    mode: HoverMode::Type,
                },
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        core.set_keymap(letter_map());
        core.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        let _ = core.click_key(C, 0x00E3);
        let frame = core.frame();
        let g = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 3)
            .unwrap();
        assert_eq!(
            g.label,
            legend_for_keycode(0x0A, LabelStyle::Icons, &loremipsum36()).primary
        );
        assert!(g.dimmed, "unbound keys fade so the binds stand out");
        let f = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 2)
            .unwrap();
        assert!(f.bind.is_some() && !f.dimmed);
    }

    #[test]
    fn type_mode_shift_layer_keeps_keys_without_a_shifted_form_readable() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                hover: crate::core::config::HoverConfig {
                    mode: HoverMode::Type,
                },
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        let mut map = letter_map();
        map.set(2, 0, 0, 0x0001); // SYM falls through to Q
        map.set(2, 0, 1, 0x0001); // SYM falls through to C ...
        map.set(0, 0, 1, 0x52); // ... which is an arrow key here
        core.set_keymap(map);
        let _ = core.click_key(C, 0x5222); // SYM
        let _ = core.click_key(C, 0x00E1); // Shift
        let frame = core.frame();
        assert!(matches!(frame.kind, FrameKind::Binds(_)));
        let q = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap();
        assert_eq!(q.label, "Q", "fallthrough key shows its shifted form");
        let up = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 1)
            .unwrap();
        assert_eq!(
            up.label,
            legend_for_keycode(0x52, LabelStyle::Icons, &loremipsum36()).primary
        );
        assert!(up.dimmed, "no shifted form: legend stays, dimmed");
    }

    #[test]
    fn fallthrough_skips_layers_that_are_not_active() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(3, 0, 0, 0x0001); // NAV falls through ...
        map.set(2, 0, 0, 0x2B); // ... but SYM (inactive) has Tab here
        core.set_keymap(map);
        let _ = core.click_key(C, 0x5223); // MO(NAV)
        let frame = core.frame();
        let q = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap();
        assert_eq!(q.keycode, 0x14, "falls to the base layer, not SYM");
        let _ = core.click_key(C, 0x5222); // MO(SYM) stacks on top of NAV
        let frame = core.frame();
        let q = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap();
        assert_eq!(q.keycode, 0x2B, "an active SYM now provides the key");
    }

    #[test]
    fn clicked_layers_stack_and_clear_together() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x5222);
        let _ = core.click_key(C, 0x5223);
        assert_eq!(core.frame().layer, 3, "highest latched layer shows");
        let _ = core.click_key(C, 0x5223);
        assert_eq!(core.frame().layer, 2, "unlatching one keeps the other");
        let _ = core.click_key(C, 0x00E1);
        core.clear_latches();
        assert_eq!(core.frame().layer, 0);
        assert!(core.click_key(C, 0x14).is_some_and(|o| o.mods == 0));
    }

    #[test]
    fn clicked_layer_is_one_shot_like_mods() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x00E3); // Super
        let _ = core.click_key(C, 0x5222); // SYM
        assert_eq!(core.frame().layer, 2);
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(out.mods, 0x08, "Super rode the typed key");
        assert_eq!(core.frame().layer, 0, "SYM cleared with it");
        let out = core.click_key(C, 0x14).unwrap();
        assert_eq!(out.mods, 0, "Super is spent too");
    }

    #[test]
    fn clicking_a_mod_or_layer_key_does_not_spend_latches() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x5222);
        let _ = core.click_key(C, 0x00E1);
        assert_eq!(core.frame().layer, 2, "latching a mod keeps the layer");
    }

    #[test]
    fn double_click_locks_until_clicked_again() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(3, 3, 6, 0x0001); // NAV key falls through to MO(NAV)
        core.set_keymap(map);
        let _ = core.click_key(C, 0x5223); // first click of the double click
        assert!(core.lock_key(0x5223));
        let _ = core.click_key(C, 0x14);
        let _ = core.click_key(C, 0x14);
        assert_eq!(core.frame().layer, 3, "typing does not spend a lock");
        let nav = core
            .frame()
            .keys
            .into_iter()
            .find(|k| k.row == 3 && k.col == 6)
            .unwrap();
        assert!(nav.held && nav.locked);
        let _ = core.click_key(C, 0x5223);
        assert_eq!(core.frame().layer, 0, "one click unlocks");

        assert!(core.lock_key(0x00E1));
        assert_eq!(core.click_key(C, 0x14).unwrap().mods, 0x02);
        assert_eq!(
            core.click_key(C, 0x14).unwrap().mods,
            0x02,
            "locked shift stays"
        );
        let _ = core.click_key(C, 0x00E1);
        assert_eq!(
            core.click_key(C, 0x14).unwrap().mods,
            0,
            "one click unlocks"
        );
    }

    #[test]
    fn locking_a_plain_key_does_nothing() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        assert!(!core.lock_key(0x14));
    }

    #[test]
    fn one_click_cancels_a_one_shot() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x5223);
        let _ = core.click_key(C, 0x5223);
        assert_eq!(core.frame().layer, 0);
    }

    #[test]
    fn physical_board_use_clears_locks_too() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.lock_key(0x5223);
        core.lock_key(0x00E1);
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: true,
            },
            Instant::now(),
        );
        assert_eq!(core.frame().layer, 0);
        assert_eq!(core.click_key(C, 0x14).unwrap().mods, 0);
    }

    #[test]
    fn toggle_layer_key_stays_until_clicked_again() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(0, 3, 4, 0x5261); // TG(QWERTY) on the base layer
        map.set(1, 3, 4, 0x0001); // and falling through on QWERTY
        core.set_keymap(map);
        let _ = core.click_key(C, 0x5261); // TG(1)
        assert_eq!(core.frame().layer, 1);
        let _ = core.click_key(C, 0x14);
        assert_eq!(core.frame().layer, 1, "typing does not spend a toggle");
        let key = core
            .frame()
            .keys
            .into_iter()
            .find(|k| k.row == 3 && k.col == 4)
            .unwrap();
        assert!(key.held && key.locked);
        let _ = core.click_key(C, 0x5261);
        assert_eq!(core.frame().layer, 0);
    }

    #[test]
    fn switching_a_layer_for_good_spends_the_one_shot_that_reached_it() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(5, 0, 4, 0x5261); // TG(QWERTY) lives on ADJ (layer 5)
        core.set_keymap(map);
        let _ = core.click_key(C, 0x5225); // MO(ADJ)
        assert_eq!(core.frame().layer, 5);
        let _ = core.click_key(C, 0x5261);
        assert_eq!(core.frame().layer, 1, "QWERTY shows, ADJ is spent");
        let _ = core.click_key(C, 0x5225);
        let _ = core.click_key(C, 0x7E03); // GAME from ADJ
        assert_eq!(core.frame().layer, 6);
        let _ = core.click_key(C, 0x5225);
        let _ = core.click_key(C, 0x7E04); // GAME OFF
        assert_eq!(core.frame().layer, 1, "QWERTY is still on underneath");
    }

    #[test]
    fn clicking_a_layer_tap_key_types_its_tap_key() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        // LT(NAV, KC_S): a click types s, it does not switch layers.
        let out = core.click_key(C, 0x4B16).unwrap();
        assert_eq!(out.key, 31);
        assert_eq!(core.frame().layer, 0);
        assert!(!core.lock_key(0x4B16), "a layer-tap cannot lock by click");
        core.hold_key(C, 0x4B16);
        assert_eq!(core.frame().layer, 11, "hold switches the layer");
    }

    #[test]
    fn the_key_that_latched_a_layer_is_the_way_back() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(9, 2, 0, 0x4D09); // one-hand base: LT(SYM, KC_F)
        map.set(13, 2, 0, 0x0000); // SYM: that cell is dead (KC_NO)
        map.set(13, 2, 1, 0x38); // SYM has a slash somewhere else
        core.set_keymap(map);
        let _ = core.click_key(C, 0x7E05); // one-hand mode on
        assert_eq!(core.frame().layer, 9);
        core.hold_key((2, 0), 0x4D09); // long-press F
        assert_eq!(core.frame().layer, 13);
        let back = core
            .frame()
            .keys
            .into_iter()
            .find(|k| k.row == 2 && k.col == 0)
            .unwrap();
        assert_eq!(back.keycode, 0x522D, "dead cell offers MO(SYM)");
        assert!(back.held, "and shows the layer is latched");
        let other = core
            .frame()
            .keys
            .into_iter()
            .find(|k| k.row == 2 && k.col == 1)
            .unwrap();
        assert_eq!(other.keycode, 0x38, "other cells are untouched");
        let _ = core.click_key((2, 0), back.keycode);
        assert_eq!(core.frame().layer, 9, "back on one-hand base");
        let f = core
            .frame()
            .keys
            .into_iter()
            .find(|k| k.row == 2 && k.col == 0)
            .unwrap();
        assert_eq!(f.keycode, 0x4D09, "F is F again");
    }

    #[test]
    fn one_hand_cmd_sends_super_and_lights_only_itself() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(0, 3, 4, 0x7E09); // 1H Cmd
        map.set(0, 3, 2, 0x00E3); // Super
        core.set_keymap(map);
        let _ = core.click_key(C, 0x7E09);
        let frame = core.frame();
        let cmd = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 4)
            .unwrap();
        let gui = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 2)
            .unwrap();
        assert!(cmd.held, "the clicked key lights");
        assert!(!gui.held, "a key that only sends the same mod does not");
        assert_eq!(core.click_key(C, 0x14).unwrap().mods, 0x08, "Cmd is Super");
    }

    #[test]
    fn off_key_leaves_a_layer_the_board_switched_on() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        // One-hand mode was switched on at the board.
        core.handle(
            HostEvent::Layer {
                default_layer: 1,
                layer_state: 1 << 9,
            },
            Instant::now(),
        );
        assert_eq!(core.frame().layer, 9);
        let _ = core.click_key(C, 0x7E06); // 1H OFF
        assert_eq!(core.frame().layer, 0, "the overlay leaves one-hand mode");
        core.handle(
            HostEvent::Layer {
                default_layer: 1,
                layer_state: 1 << 9,
            },
            Instant::now(),
        );
        assert_eq!(core.frame().layer, 0, "a board poll does not undo it");
        core.handle(
            HostEvent::Layer {
                default_layer: 1,
                layer_state: 0,
            },
            Instant::now(),
        );
        core.handle(
            HostEvent::Layer {
                default_layer: 1,
                layer_state: 1 << 9,
            },
            Instant::now(),
        );
        assert_eq!(core.frame().layer, 9, "the board entering it again shows");
    }

    #[test]
    fn turning_a_board_layer_back_on_unmasks_it() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.handle(
            HostEvent::Layer {
                default_layer: 1,
                layer_state: 1 << 6,
            },
            Instant::now(),
        );
        let _ = core.click_key(C, 0x7E04); // GAME OFF
        assert_eq!(core.frame().layer, 0);
        let _ = core.click_key(C, 0x7E03); // GAME ON
        assert_eq!(core.frame().layer, 6);
    }

    #[test]
    fn exclusive_layer_key_replaces_other_latches() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x5262); // TG(2)
        let _ = core.click_key(C, 0x5201); // TO(1)
        assert_eq!(core.frame().layer, 1);
        let _ = core.click_key(C, 0x5203); // TO(3)
        let _ = core.click_key(C, 0x5200); // TO(0)
        assert_eq!(core.frame().layer, 0, "TO(0) is back to base");
    }

    #[test]
    fn game_and_one_hand_keys_switch_their_layers_for_good() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        assert!(core.click_key(C, 0x7E03).is_none(), "GAME sends nothing");
        assert_eq!(core.frame().layer, 6);
        let _ = core.click_key(C, 0x14);
        assert_eq!(core.frame().layer, 6, "game mode survives typing");
        let _ = core.click_key(C, 0x7E03);
        assert_eq!(core.frame().layer, 6, "GAME again stays on");
        let _ = core.click_key(C, 0x7E04); // GAME OFF
        assert_eq!(core.frame().layer, 0);
        let _ = core.click_key(C, 0x7E05); // 1H
        assert_eq!(core.frame().layer, 9);
        let _ = core.click_key(C, 0x7E06); // 1H OFF
        assert_eq!(core.frame().layer, 0);
    }

    #[test]
    fn one_hand_mod_keys_latch_like_mods() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let _ = core.click_key(C, 0x7E08); // 1H Ctrl
        assert_eq!(core.click_key(C, 0x14).unwrap().mods, 0x01);
        let _ = core.click_key(C, 0x7E0B); // one-shot shift
        assert_eq!(core.click_key(C, 0x14).unwrap().mods, 0x02);
        let _ = core.click_key(C, 0x7E09); // 1H Cmd is Super
        assert_eq!(core.click_key(C, 0x14).unwrap().mods, 0x08);
    }

    #[test]
    fn clicked_layer_stacks_on_the_boards_layer() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.handle(
            HostEvent::Layer {
                default_layer: 1,
                layer_state: 1 << 2,
            },
            Instant::now(),
        );
        let _ = core.click_key(C, 0x5223);
        assert_eq!(core.frame().layer, 3);
        let _ = core.click_key(C, 0x5223);
        assert_eq!(core.frame().layer, 2, "board layer is untouched");
    }

    #[test]
    fn bind_layer_shows_on_held_modifier() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        assert!(matches!(frame.kind, FrameKind::Binds(ref b) if b.header == "SUPER"));
        let f = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 2)
            .unwrap();
        let bind = f.bind.as_ref().unwrap();
        assert_eq!(bind.glyph.as_deref(), Some("\u{f065}"));
        assert!(
            bind.description.is_empty(),
            "icons style shows the glyph only"
        );
        let g = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 3)
            .unwrap();
        assert!(g.bind.is_none(), "unbound key keeps muted normal legend");
        assert_eq!(g.label, "", "unbound keys are blank on a bind layer");
        assert_eq!(frame.layer, 0, "QMK layer stays accurate");
    }

    #[test]
    fn bind_view_follows_label_style() {
        let mut icons = always_core();
        icons.set_keymap(letter_map());
        icons.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        icons.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let f = icons
            .frame()
            .keys
            .into_iter()
            .find(|k| k.row == 0 && k.col == 2)
            .unwrap();
        let b = f.bind.unwrap();
        assert_eq!(b.glyph.as_deref(), Some("\u{f065}"));
        assert!(b.description.is_empty(), "icons style shows the glyph only");

        let mut text = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                label_style: LabelStyle::Text,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        text.set_keymap(letter_map());
        text.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        text.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let f = text
            .frame()
            .keys
            .into_iter()
            .find(|k| k.row == 0 && k.col == 2)
            .unwrap();
        let b = f.bind.unwrap();
        assert!(b.glyph.is_none(), "text style shows the description only");
        assert_eq!(b.description, "Full screen");
    }

    #[test]
    fn tooltip_shows_full_bind_description_in_icons_mode() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        let f = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 2)
            .unwrap();
        assert!(
            f.bind.as_ref().is_some_and(|b| b.description.is_empty()),
            "on-key legend stays glyph-only"
        );
        assert_eq!(f.tooltip, "Full screen");

        let g = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 3)
            .unwrap();
        assert_eq!(g.tooltip, "", "dimmed bind-layer key stays quiet");
    }

    #[test]
    fn tooltip_appends_hold_role() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                label_style: LabelStyle::Text,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        let mut map = Keymap::empty(14, 4, 10);
        map.set(0, 0, 1, 0x2106); // LCTL_T(KC_C)
        core.set_keymap(map);
        let frame = core.frame();
        let c = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 1)
            .unwrap();
        assert_eq!(c.label, "c");
        assert_eq!(c.hold_label.as_deref(), Some("Ctrl"));
        assert_eq!(c.tooltip, "c \u{b7} hold Ctrl");
    }

    #[test]
    fn tooltip_bind_key_appends_hold_role() {
        let mut core = always_core();
        let mut map = Keymap::empty(14, 4, 10);
        map.set(0, 0, 1, 0x2128); // LCTL_T(KC_ENT)
        map.set(0, 3, 2, 0x00E3); // LGUI thumb
        core.set_keymap(map);
        core.set_binds(parse_binds("SUPER + RETURN \u{2192} Terminal\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        let ent = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 1)
            .unwrap();
        assert!(ent.bind.is_some(), "Super+Enter carries the Terminal bind");
        assert_eq!(ent.hold_label.as_deref(), Some("\u{2303}"));
        assert_eq!(ent.tooltip, "Terminal \u{b7} hold Ctrl");
    }

    #[test]
    fn tooltip_shifted_symbol_stays_quiet() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 4,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        let one = frame
            .keys
            .iter()
            .find(|k| k.row == 1 && k.col == 0)
            .unwrap();
        assert_eq!(one.label, "!");
        assert_eq!(one.tooltip, "", "shifted output is self-explanatory");
    }

    #[test]
    fn tooltip_plain_letters_stay_quiet() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let frame = core.frame();
        let q = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap();
        assert_eq!(q.tooltip, "", "letters need no tooltip");
    }

    #[test]
    fn tooltip_icon_key_shows_readable_name() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        let frame = core.frame();
        let super_key = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 2)
            .unwrap();
        assert_eq!(super_key.label, "\u{E900}");
        assert_eq!(super_key.tooltip, "Super");
    }

    #[test]
    fn tooltip_space_names_the_key() {
        let mut core = always_core();
        let mut map = Keymap::empty(14, 4, 10);
        map.set(0, 3, 5, 0x2C); // Space on the base thumb row
        core.set_keymap(map);
        let frame = core.frame();
        let space = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 5)
            .unwrap();
        assert_eq!(space.label, "\u{2423}");
        assert_eq!(space.tooltip, "Space");
    }

    #[test]
    fn tooltip_pressed_mod_tap_names_hold_role() {
        let mut core = always_core();
        let mut map = Keymap::empty(14, 4, 10);
        map.set(0, 0, 1, 0x2106); // LCTL_T(KC_C)
        core.set_keymap(map);
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 1,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        let c = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 1)
            .unwrap();
        assert_eq!(c.label, "\u{2303}", "centred hold icon");
        assert_eq!(c.tooltip, "Ctrl");
    }

    #[test]
    fn pointer_activity_keeps_auto_visible() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Auto,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        assert!(!core.frame().visible, "no activity yet");
        core.note_activity(Instant::now());
        assert!(core.frame().visible);
    }

    #[test]
    fn bind_layer_wins_over_qmk_layer_and_release_restores() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(3, 3, 2, 0x00E3); // NAV keeps Super on the thumb
        map.set(3, 0, 2, 0x1E); // NAV sends 1 on the F cell
        core.set_keymap(map);
        core.set_binds(parse_binds("SUPER + 1 \u{2192} Switch to workspace 1\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 6,
                pressed: true,
            },
            Instant::now(),
        );
        core.handle(
            HostEvent::Layer {
                default_layer: 0b1,
                layer_state: 0b1000,
            },
            Instant::now(),
        );
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        assert_eq!(frame.layer, 3);
        let f = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 2)
            .unwrap();
        assert_eq!(
            f.bind.as_ref().and_then(|b| b.glyph.as_deref()),
            Some("\u{f0a9} 1")
        );
        assert!(matches!(frame.kind, FrameKind::Binds(_)));

        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: false,
            },
            Instant::now(),
        );
        core.tick(Instant::now() + Duration::from_millis(200));
        let frame = core.frame();
        assert!(matches!(frame.kind, FrameKind::Layer));
        assert_eq!(frame.layer, 3);
    }

    #[test]
    fn bind_layer_follows_active_layer_keycode() {
        let mut core = always_core();
        let mut map = Keymap::empty(14, 4, 10);
        map.set(0, 1, 0, 0x04); // A on base
        map.set(3, 1, 0, 0x1E); // 1 on NAV, same cell
        map.set(0, 3, 2, 0x00E3); // Super thumb
        map.set(0, 3, 6, 0x5223); // MO(NAV) thumb
        core.set_keymap(map);
        core.set_binds(parse_binds(
            "SUPER + A \u{2192} Select all\nSUPER + 1 \u{2192} Switch to workspace 1\n",
        ));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        for _ in 0..30 {
            let frame = core.frame();
            let k = frame
                .keys
                .iter()
                .find(|k| k.row == 1 && k.col == 0)
                .unwrap();
            assert_eq!(
                k.bind.as_ref().and_then(|b| b.glyph.as_deref()),
                Some("\u{f14a}"),
                "base cell shows the A bind"
            );
        }
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 6,
                pressed: true,
            },
            Instant::now(),
        );
        core.handle(
            HostEvent::Layer {
                default_layer: 0b1,
                layer_state: 0b1000,
            },
            Instant::now(),
        );
        for _ in 0..30 {
            let frame = core.frame();
            let k = frame
                .keys
                .iter()
                .find(|k| k.row == 1 && k.col == 0)
                .unwrap();
            assert_eq!(
                k.bind.as_ref().and_then(|b| b.glyph.as_deref()),
                Some("\u{f0a9} 1"),
                "NAV cell shows the digit bind"
            );
        }
    }

    #[test]
    fn exact_mask_only() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.set_binds(parse_binds(
            "SUPER + X \u{2192} Close window\nSUPER SHIFT + X \u{2192} Swap window left\n",
        ));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        let x = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 4)
            .unwrap();
        assert_eq!(
            x.bind.as_ref().and_then(|b| b.glyph.as_deref()),
            Some("\u{f2d3}")
        );

        core.handle(
            HostEvent::Key {
                row: 3,
                col: 4,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        let x = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 4)
            .unwrap();
        assert_eq!(
            x.bind.as_ref().and_then(|b| b.glyph.as_deref()),
            Some("\u{f0ec}")
        );
    }

    #[test]
    fn unknown_combo_falls_back_to_layer() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 4,
                pressed: true,
            },
            Instant::now(),
        );
        assert!(matches!(core.frame().kind, FrameKind::Layer));
    }

    #[test]
    fn shift_layer_transforms_typed_output() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.set_binds(parse_binds("SHIFT + F \u{2192} Magic\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 4,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        assert!(matches!(frame.kind, FrameKind::Binds(ref b) if b.header == "SHIFT"));
        let q = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap();
        assert_eq!(q.label, "Q");
        assert!(q.bind.is_none());
        let one = frame
            .keys
            .iter()
            .find(|k| k.row == 1 && k.col == 0)
            .unwrap();
        assert_eq!(one.label, "!");
        let f = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 2)
            .unwrap();
        assert_eq!(
            f.bind.as_ref().and_then(|b| b.glyph.as_deref()),
            Some("M"),
            "no glyph table hit falls back to the first letter"
        );
        assert_eq!(f.label, "f", "bind key keeps its normal legend underneath");
    }

    #[test]
    fn quick_tap_mod_tap_does_not_show_bind_layer() {
        let mut core = always_core();
        let mut map = letter_map();
        map.set(0, 0, 0, 0x2114); // LCTL_T(Q)
        core.set_keymap(map);
        let t0 = Instant::now();
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: true,
            },
            t0,
        );
        assert!(matches!(core.frame().kind, FrameKind::Binds(_)));
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: false,
            },
            t0,
        );
        assert!(matches!(core.frame().kind, FrameKind::Layer));
    }

    #[test]
    fn bind_layer_keeps_held_mod_key() {
        let mut core = always_core();
        core.set_keymap(letter_map());
        core.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        assert!(matches!(frame.kind, FrameKind::Binds(_)));
        let sup = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 2)
            .unwrap();
        assert!(sup.held);
        assert!(sup.bind.is_none(), "the held mod key itself has no bind");
    }

    #[test]
    fn no_keymap_keeps_layer_kind() {
        let mut core = always_core();
        core.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        assert!(matches!(core.frame().kind, FrameKind::Layer));
    }

    #[test]
    fn disabled_binds_stay_on_layer() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                binds: BindsConfig {
                    enabled: false,
                    standard: true,
                },
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        core.set_keymap(letter_map());
        core.set_binds(parse_binds("SUPER + F \u{2192} Full screen\n"));
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );
        assert!(matches!(core.frame().kind, FrameKind::Layer));
    }

    #[test]
    fn standard_tables_can_be_disabled() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                binds: BindsConfig {
                    enabled: true,
                    standard: false,
                },
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        let mut map = letter_map();
        map.set(0, 3, 3, 0x00E0); // LCTL on a spare thumb cell
        core.set_keymap(map);
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 3,
                pressed: true,
            },
            Instant::now(),
        );
        assert!(matches!(core.frame().kind, FrameKind::Layer));
    }

    #[test]
    fn auto_hides_after_timeout() {
        let mut core = OverlayCore::new(loremipsum36(), AppConfig::default(), Overrides::default());
        let t0 = Instant::now();
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: true,
            },
            t0,
        );
        // Simulate aged activity by rewriting last_activity.
        core.last_activity = Instant::now().checked_sub(Duration::from_millis(2500));
        assert!(!core.frame().visible);
    }

    #[test]
    fn auto_shows_on_key_not_layer_poll() {
        let mut core = OverlayCore::new(loremipsum36(), AppConfig::default(), Overrides::default());
        let t0 = Instant::now();
        core.handle(
            HostEvent::Layer {
                default_layer: 0,
                layer_state: 0b100,
            },
            t0,
        );
        assert!(
            !core.frame().visible,
            "layer poll alone must not show overlay"
        );
        assert_eq!(core.frame().layer, 2);
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: true,
            },
            t0,
        );
        assert!(core.frame().visible);
        // Keepalive-style layer refresh must not extend the idle timer.
        core.last_activity = Instant::now().checked_sub(Duration::from_millis(2500));
        core.handle(
            HostEvent::Layer {
                default_layer: 0,
                layer_state: 0b100,
            },
            Instant::now(),
        );
        assert!(
            !core.frame().visible,
            "layer poll must not reset auto timeout"
        );
    }

    #[test]
    fn auto_ignores_spurious_key_release() {
        let mut core = OverlayCore::new(loremipsum36(), AppConfig::default(), Overrides::default());
        let t0 = Instant::now();
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: true,
            },
            t0,
        );
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: false,
            },
            t0,
        );
        core.last_activity = Instant::now().checked_sub(Duration::from_millis(2500));
        // Phantom F release (LayerLens state=0) — not in held map.
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 2,
                pressed: false,
            },
            Instant::now(),
        );
        assert!(
            !core.frame().visible,
            "spurious release must not reset auto timeout"
        );
    }

    #[test]
    fn frame_carries_normalized_scale() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                scale: 0.48,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        assert!((core.frame().scale - 0.50).abs() < f64::EPSILON);
        core.set_config(AppConfig {
            scale: 0.22,
            ..AppConfig::default()
        });
        assert!((core.frame().scale - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn highest_layer_wins() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        core.handle(
            HostEvent::Layer {
                default_layer: 0,
                layer_state: 0b1100,
            },
            Instant::now(),
        );
        assert_eq!(core.frame().layer, 3);
    }

    #[test]
    fn held_key_marks_view() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 1,
                pressed: true,
            },
            Instant::now(),
        );
        let frame = core.frame();
        let k = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 1)
            .unwrap();
        assert!(k.held);
    }

    #[test]
    fn quick_tap_flash_survives_same_batch_release() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        let mut map = Keymap::empty(6, 4, 10);
        map.set(0, 0, 0, 0x2114); // LCTL_T(Q)
        core.set_keymap(map);
        let t0 = Instant::now();
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: true,
            },
            t0,
        );
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 0,
                pressed: false,
            },
            t0, // same poll batch as press
        );
        let k = core
            .frame()
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap()
            .clone();
        assert!(k.held, "quick tap must stay lit after same-batch release");
        assert_eq!(k.label, "q");
        assert_eq!(k.hold_label.as_deref(), Some("⌃"));
        core.tick(t0 + Duration::from_millis(50));
        let mid = core.frame();
        assert!(
            mid.keys.iter().any(|k| k.row == 0 && k.col == 0 && k.held),
            "flash should still be lit mid-duration"
        );
        core.tick(t0 + Duration::from_millis(100));
        let frame = core.frame();
        let k = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 0)
            .unwrap();
        assert!(!k.held, "flash should end after TAP_FLASH");
    }

    #[test]
    fn layer_tap_hold_shows_hold_role_while_pressed() {
        // LT(2, Esc) on base at (3,2); hold layer same pos is KC_LEFT.
        // While pressed the key shows its hold role, not the stale tap letter
        // and not the live hold-layer keycap; the frozen tap legend returns
        // for the release flash so quick taps still read Esc.
        const LT_ESC_L2: u16 = 0x4200 | 0x29; // layer 2 + KC_ESC
        const KC_LEFT: u16 = 0x0050;
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        let mut map = Keymap::empty(6, 4, 10);
        map.set(0, 3, 2, LT_ESC_L2);
        map.set(2, 3, 2, KC_LEFT);
        core.set_keymap(map);

        core.handle(
            HostEvent::Layer {
                default_layer: 0b1,
                layer_state: 0b100, // overlay only — matches KeyPeek/QMK
            },
            Instant::now(),
        );
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: true,
            },
            Instant::now(),
        );

        let frame = core.frame();
        assert_eq!(frame.layer, 2);
        let k = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 2)
            .unwrap();
        assert!(k.held);
        assert_eq!(k.label, "SYM");
        assert_eq!(k.hold_label, None);

        core.handle(
            HostEvent::Key {
                row: 3,
                col: 2,
                pressed: false,
            },
            Instant::now(),
        );
        let frame = core.frame();
        let k = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 2)
            .unwrap();
        assert!(k.held, "release flash keeps the key lit");
        assert_eq!(k.label, "⎋");
        assert_eq!(k.hold_label.as_deref(), Some("SYM"));
    }

    #[test]
    fn mo_hold_keeps_layer_legend() {
        // MO(SYM) on base thumb; SYM layer same pos is something else.
        const MO_SYM: u16 = 0x5222;
        const KC_LEFT: u16 = 0x0050;
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        let mut map = Keymap::empty(6, 4, 10);
        map.set(0, 3, 3, MO_SYM);
        map.set(2, 3, 3, KC_LEFT);
        core.set_keymap(map);

        core.handle(
            HostEvent::Layer {
                default_layer: 0b1,
                layer_state: 0b100,
            },
            Instant::now(),
        );
        core.handle(
            HostEvent::Key {
                row: 3,
                col: 3,
                pressed: true,
            },
            Instant::now(),
        );

        let frame = core.frame();
        let k = frame
            .keys
            .iter()
            .find(|k| k.row == 3 && k.col == 3)
            .unwrap();
        assert!(k.held);
        assert_eq!(k.label, "SYM");
    }

    #[test]
    fn special_flags_mods_layers_and_tap_hold() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        let mut map = Keymap::empty(6, 4, 10);
        map.set(0, 0, 0, 0x0014); // KC_Q
        map.set(0, 0, 1, 0x2114); // LCTL_T(KC_Q) — mod-tap, accented
        map.set(0, 0, 2, 0x00E0); // KC_LCTL
        map.set(0, 0, 3, 0x0036); // KC_COMM
        map.set(0, 0, 4, 0x0028); // KC_ENT
        map.set(0, 0, 5, 0x5222); // MO(SYM)
        map.set(0, 0, 6, 0x4200 | 0x29); // LT(2, Esc)
        map.set(0, 0, 7, 0x5261); // TG(QWERTY)
        map.set(0, 0, 8, 0x528A); // OSL(1H Flip)
        map.set(0, 1, 0, 0x7E00); // MC_MUTE — shortcut, muted
        map.set(0, 1, 1, 0x7E03); // GAM_ON — game mode toggle
        map.set(0, 1, 2, 0x7E05); // FV_ON — one-hand toggle
        map.set(0, 1, 3, 0x7E0B); // FV_OSSF — one-shot shift
        core.set_keymap(map);
        let frame = core.frame();
        let find = |r, c| {
            frame
                .keys
                .iter()
                .find(|k| k.row == r && k.col == c)
                .unwrap()
        };
        assert!(!find(0, 0).special, "letter muted");
        assert!(find(0, 1).special, "mod-tap accented");
        assert!(find(0, 2).special, "modifier accented");
        assert!(!find(0, 3).special, "comma muted");
        assert!(!find(0, 4).special, "enter muted");
        assert!(find(0, 5).special, "MO accented");
        assert!(find(0, 6).special, "LT accented");
        assert!(find(0, 7).special, "TG accented");
        assert!(find(0, 8).special, "OSL accented");
        assert!(!find(1, 0).special, "mute muted");
        assert!(find(1, 1).special, "GAM_ON accented");
        assert!(find(1, 2).special, "FV_ON accented");
        assert!(find(1, 3).special, "FV_OSSF accented");
    }

    #[test]
    fn one_hand_layer_shows() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        core.handle(
            HostEvent::Layer {
                default_layer: 0,
                layer_state: 1 << 9,
            },
            Instant::now(),
        );
        let frame = core.frame();
        assert!(frame.visible);
        assert_eq!(frame.layer, 9);
        assert!(!frame.keys.is_empty());
    }

    #[test]
    fn layer_above_profile_range_hides() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        core.handle(
            HostEvent::Layer {
                default_layer: 0,
                layer_state: 1 << 14,
            },
            Instant::now(),
        );
        assert!(!core.frame().visible);
    }

    #[test]
    fn osl_hold_keeps_layer_legend() {
        // OSL(FLIP) on the one-hand base layer; pressing it activates FLIP
        // (layer 10) whose same cell is a plain letter.
        const OSL_FLIP: u16 = 0x528A;
        const KC_Q: u16 = 0x0014;
        const KC_A: u16 = 0x0004;
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        let mut map = Keymap::empty(14, 4, 10);
        map.set(0, 0, 3, KC_A);
        map.set(9, 0, 3, OSL_FLIP);
        map.set(10, 0, 3, KC_Q);
        core.set_keymap(map);

        core.handle(
            HostEvent::Layer {
                default_layer: 0b1,
                layer_state: (1 << 9) | (1 << 10),
            },
            Instant::now(),
        );
        core.handle(
            HostEvent::Key {
                row: 0,
                col: 3,
                pressed: true,
            },
            Instant::now(),
        );

        let frame = core.frame();
        assert_eq!(frame.layer, 10);
        let k = frame
            .keys
            .iter()
            .find(|k| k.row == 0 && k.col == 3)
            .unwrap();
        assert!(k.held);
        assert_eq!(k.label, "FLIP");
        assert!(k.special);
    }

    #[test]
    fn default_layer_shows_when_layer_state_zero() {
        let mut core = OverlayCore::new(
            loremipsum36(),
            AppConfig {
                mode: VisibilityMode::Always,
                ..AppConfig::default()
            },
            Overrides::default(),
        );
        core.handle(
            HostEvent::Layer {
                default_layer: 0b10, // layer 1 (QWERTY)
                layer_state: 0,
            },
            Instant::now(),
        );
        assert_eq!(core.frame().layer, 1);
    }

    #[test]
    fn plain_boards_ignore_author_keycodes() {
        let plain = BoardProfile {
            custom_keycodes: false,
            ..loremipsum36()
        };
        let mut core = OverlayCore::new(plain.clone(), AppConfig::default(), Overrides::default());
        assert!(layer_key(0x7E03, &plain).is_none());
        assert!(mod_bit(0x7E09, &plain).is_none());
        assert!(core.click_key((0, 0), 0x7E00).is_none(), "no Zoom macro");
        assert!(!is_modifier_or_layer_key(0x7E05, &plain));
        assert!(layer_key(0x7E03, &loremipsum36()).is_some());
    }

    #[test]

    fn generic_super_hold_shows_bind_layer() {
        use crate::board::evdev::evdev_layout;
        use crate::board::profile::generic;
        use crate::core::binds::{parse_binds, MASK_SUPER};
        let layout = evdev_layout("generic");
        let (r, c) = layout.cells[&125];
        assert_eq!(layout.keymap.get(0, r, c), 0xE3, "ANSI Super cell is LGUI");
        let mut core = OverlayCore::new(generic(), AppConfig::default(), Overrides::default());
        core.set_keymap(layout.keymap);
        core.set_binds(parse_binds("SUPER + RETURN \u{2192} Terminal\n"));
        core.handle(
            HostEvent::Key { row: r, col: c, pressed: true },
            Instant::now(),
        );
        assert_eq!(core.held_modmask(), MASK_SUPER, "held Super is SUPER mask");
        assert!(
            matches!(core.frame().kind, FrameKind::Binds(ref b) if b.header == "SUPER"),
            "frame kind={:?}",
            core.frame().kind
        );
    }

    #[test]
    fn generic_keyboard_draws_its_keys_and_lights_the_pressed_one() {
        use crate::board::evdev::evdev_layout;
        use crate::board::profile::generic;
        let layout = evdev_layout("generic");
        let (row, col) = layout.cells[&30]; // KEY_A
        let mut core = OverlayCore::new(generic(), AppConfig::default(), Overrides::default());
        core.set_keymap(layout.keymap);
        let t = Instant::now();
        core.handle(HostEvent::Key { row, col, pressed: true }, t);
        let frame = core.frame();
        assert_eq!(frame.layer_name, "KEYBOARD");
        assert!(frame.visible, "typing shows the overlay");
        let a = frame.keys.iter().find(|k| (k.row, k.col) == (row, col)).unwrap();
        assert_eq!(a.label, "a");
        assert!(a.held, "pressed key is lit");
    }
}
