//! GTK4 layer-shell overlay window.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

use cairo::{RectangleInt, Region};
use gtk::gdk::prelude::{DisplayExt, MonitorExt, SurfaceExt};
use gtk::gdk::RGBA;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, DrawingArea};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use pango::{Alignment, EllipsizeMode, FontDescription, WrapMode};
use pangocairo::functions::{create_layout, show_layout};

use crate::board::OMARCHY_SUPER;
use crate::core::config::{Anchor, HoverMode};
use crate::core::output::send;
use crate::core::state::{is_dual_role, Frame, FrameKind, KeyView, OverlayCore};
use crate::ui::theme::Theme;

const UNIT_PX: f64 = 44.0;
const PAD: f64 = 12.0;
/// Fallback shorter screen side (logical px) when no monitor is known.
const REFERENCE_SCREEN_MIN: f64 = 1080.0;
/// How often a dodged overlay checks whether the pointer left its home area.
const DODGE_POLL: Duration = Duration::from_millis(100);
/// Extra room around the home area before the overlay returns, so the pointer
/// grazing the edge does not make it flicker back and forth.
const DODGE_RETURN_PAD: f64 = 24.0;
/// Without a cursor source the overlay returns on its own after this long.
const DODGE_BLIND_RETURN: Duration = Duration::from_millis(2000);
/// Gap between the overlay and the screen edge (layer-shell margin).
const EDGE_MARGIN: i32 = 12;
/// A dual-role key held this long (type mode) latches its hold role instead
/// of tapping.
const TAP_HOLD: Duration = Duration::from_millis(250);
/// One family for letters + symbols so Pango does not mix Liberation / Nerd / Adwaita.
/// Super still switches to `omarchy` via markup. Matches Omarchy UI monospace.
const LEGEND_FONT: &str = "JetBrainsMono Nerd Font";

pub struct OverlayWindow {
    window: ApplicationWindow,
    area: DrawingArea,
    frame: Rc<RefCell<Frame>>,
    theme: Rc<RefCell<Theme>>,
    /// Paint scale relative to the 44 px design unit, shared with the draw callback.
    paint: Rc<Cell<f64>>,
    /// Tooltip and dodge both need pointer input, so clicks over the overlay
    /// stop passing through to windows below; Off stays click-through.
    mode: Rc<Cell<HoverMode>>,
    /// Anchor from config; the overlay returns here after a dodge.
    home: Rc<Cell<Anchor>>,
    /// Anchor the overlay hopped to, while the pointer is over its home area.
    dodged: Rc<Cell<Option<Anchor>>>,
    dodged_at: Rc<Cell<Option<Instant>>>,
    last_poll: Cell<Option<Instant>>,
    /// Last pointer position in drawing-area coordinates.
    pointer: Rc<Cell<Option<(f64, f64)>>>,
    pointer_inside: Rc<Cell<bool>>,
}

impl OverlayWindow {
    pub fn new(app: &Application, mode: HoverMode, core: Rc<RefCell<OverlayCore>>) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Omakeys")
            .decorated(false)
            .resizable(false)
            .build();

        window.init_layer_shell();
        window.set_namespace("omakeys");
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            window.set_margin(edge, EDGE_MARGIN);
        }

        let area = DrawingArea::new();
        let frame = Rc::new(RefCell::new(Frame {
            visible: false,
            opacity: 0.92,
            scale: 0.50,
            anchor: Anchor::BottomRight,
            layer: 0,
            layer_name: String::new(),
            kind: FrameKind::Layer,
            keys: Vec::new(),
        }));
        let theme = Rc::new(RefCell::new(Theme::default()));
        let paint = Rc::new(Cell::new(1.0));
        let mode = Rc::new(Cell::new(mode));
        let home = Rc::new(Cell::new(Anchor::BottomRight));
        let dodged = Rc::new(Cell::new(None::<Anchor>));
        let dodged_at = Rc::new(Cell::new(None::<Instant>));
        let pointer = Rc::new(Cell::new(None::<(f64, f64)>));
        let pointer_inside = Rc::new(Cell::new(false));
        let flash = Rc::new(Cell::new(None::<usize>));

        {
            let frame_c = Rc::clone(&frame);
            let theme_c = Rc::clone(&theme);
            let paint_c = Rc::clone(&paint);
            let mode_c = Rc::clone(&mode);
            let pointer_c = Rc::clone(&pointer);
            let flash_c = Rc::clone(&flash);
            area.set_draw_func(move |_area, cr, _w, _h| {
                let frame = frame_c.borrow();
                let theme = theme_c.borrow();
                let hover_pos = match mode_c.get() {
                    HoverMode::Tooltip | HoverMode::Type => pointer_c.get(),
                    HoverMode::Off | HoverMode::Dodge => None,
                };
                draw_keyboard(cr, &frame, &theme, paint_c.get(), hover_pos, flash_c.get());
            });
        }

        // Pointer tracking feeds the tooltip and the dodge. Events only arrive
        // while a non-empty input region is set, see apply_input_region.
        let motion = gtk::EventControllerMotion::new();
        {
            let pointer_c = Rc::clone(&pointer);
            let inside_c = Rc::clone(&pointer_inside);
            motion.connect_motion(move |_, x, y| {
                if inside_c.get() {
                    pointer_c.set(Some((x, y)));
                }
            });
        }
        {
            let pointer_c = Rc::clone(&pointer);
            let inside_c = Rc::clone(&pointer_inside);
            let mode_c = Rc::clone(&mode);
            let home_c = Rc::clone(&home);
            let dodged_c = Rc::clone(&dodged);
            let dodged_at_c = Rc::clone(&dodged_at);
            let window_c = window.clone();
            let area_c = area.clone();
            motion.connect_enter(move |_, x, y| {
                inside_c.set(true);
                pointer_c.set(Some((x, y)));
                if mode_c.get() == HoverMode::Dodge {
                    // Pushed away from where the pointer came in.
                    let push = (
                        f64::from(area_c.width()) / 2.0 - x,
                        f64::from(area_c.height()) / 2.0 - y,
                    );
                    let next = dodged_c.get().unwrap_or(home_c.get()).dodge(push);
                    dodged_c.set(Some(next));
                    dodged_at_c.set(Some(Instant::now()));
                    set_window_anchor(&window_c, next);
                }
            });
        }
        {
            let pointer_c = Rc::clone(&pointer);
            let inside_c = Rc::clone(&pointer_inside);
            motion.connect_leave(move |_| {
                inside_c.set(false);
                pointer_c.set(None);
            });
        }
        area.add_controller(motion);

        // Type mode: a click types the key under the pointer, latches a mod
        // or switches the shown layer. Holding a dual-role key past
        // TAP_HOLD latches its hold role instead. The overlay keeps
        // KeyboardMode::None, so keyboard focus never leaves the window
        // being typed into.
        let click = gtk::GestureClick::new();
        click.set_button(1);
        {
            let pending = Rc::new(Cell::new(None::<((u8, u8), u16)>));
            let press_seq = Rc::new(Cell::new(0u64));
            let frame_c = Rc::clone(&frame);
            let mode_c = Rc::clone(&mode);
            let mode_r = Rc::clone(&mode);
            let paint_c = Rc::clone(&paint);
            let flash_c = Rc::clone(&flash);
            let area_c = area.clone();
            let core_c = Rc::clone(&core);
            let core_r = Rc::clone(&core);
            let pending_c = Rc::clone(&pending);
            click.connect_pressed(move |_, n_press, x, y| {
                if mode_c.get() != HoverMode::Type {
                    return;
                }
                let (cell, kc) = {
                    let frame = frame_c.borrow();
                    let Some(idx) = key_at(&frame.keys, paint_c.get(), x, y) else {
                        return;
                    };
                    flash_c.set(Some(idx));
                    let key = &frame.keys[idx];
                    ((key.row, key.col), key.keycode)
                };
                let flash_c = Rc::clone(&flash_c);
                let area_redraw = area_c.clone();
                gtk::glib::timeout_add_local_once(Duration::from_millis(120), move || {
                    flash_c.set(None);
                    area_redraw.queue_draw();
                });
                area_c.queue_draw();
                if is_dual_role(kc) {
                    // Wait out the press: quick releases tap below, a long
                    // press latches the hold role.
                    let seq = press_seq.get() + 1;
                    press_seq.set(seq);
                    pending_c.set(Some((cell, kc)));
                    let pending_c = Rc::clone(&pending_c);
                    let press_seq_c = Rc::clone(&press_seq);
                    let mode_c = Rc::clone(&mode_c);
                    let core_c = Rc::clone(&core_c);
                    let area_redraw = area_c.clone();
                    gtk::glib::timeout_add_local_once(TAP_HOLD, move || {
                        if mode_c.get() == HoverMode::Type
                            && press_seq_c.get() == seq
                            && pending_c.get() == Some((cell, kc))
                        {
                            pending_c.set(None);
                            core_c.borrow_mut().hold_key(cell, kc);
                            area_redraw.queue_draw();
                        }
                    });
                } else if n_press >= 2 && core_c.borrow_mut().lock_key(kc) {
                    // Double click on a mod or layer key: locked.
                } else if let Some(out) = core_c.borrow_mut().click_key(cell, kc) {
                    send(&out);
                }
            });
            click.connect_released(move |_, _, _, _| {
                if mode_r.get() != HoverMode::Type {
                    pending.set(None);
                    return;
                }
                if let Some((cell, kc)) = pending.take() {
                    if let Some(out) = core_r.borrow_mut().click_key(cell, kc) {
                        send(&out);
                    }
                }
            });
        }
        area.add_controller(click);

        // Right click drops every latched mod and layer: the way back to the
        // board's own state when the latched layer has no key to leave it.
        let clear = gtk::GestureClick::new();
        clear.set_button(3);
        {
            let mode_c = Rc::clone(&mode);
            let core_c = Rc::clone(&core);
            clear.connect_pressed(move |_, _, _, _| {
                if mode_c.get() == HoverMode::Type {
                    core_c.borrow_mut().clear_latches();
                }
            });
        }
        area.add_controller(clear);

        window.set_child(Some(&area));

        // Empty input region → clicks pass through to windows below. Tooltip
        // and dodge need pointer events; re-applies on realize, show and resize.
        {
            let mode_c = Rc::clone(&mode);
            window.connect_realize(move |w| apply_input_region(w, mode_c.get()));
        }

        Self {
            window,
            area,
            frame,
            theme,
            paint,
            mode,
            home,
            dodged,
            dodged_at,
            last_poll: Cell::new(None),
            pointer,
            pointer_inside,
        }
    }

    /// Re-apply the input region after a live config change.
    pub fn set_hover_mode(&self, mode: HoverMode) {
        if self.mode.get() == mode {
            return;
        }
        self.mode.set(mode);
        self.dodged.set(None);
        self.pointer.set(None);
        self.pointer_inside.set(false);
        apply_input_region(&self.window, mode);
        self.area.queue_draw();
    }

    /// True while the pointer rests on the overlay; the daemon counts it as
    /// activity so Auto mode cannot hide the surface under the cursor.
    pub fn pointer_inside(&self) -> bool {
        self.pointer_inside.get()
    }

    pub fn set_theme(&self, theme: Theme) {
        *self.theme.borrow_mut() = theme;
        self.area.queue_draw();
    }

    pub fn apply_frame(&self, frame: &Frame) {
        *self.frame.borrow_mut() = frame.clone();
        if self.home.get() != frame.anchor || !frame.visible {
            self.dodged.set(None);
        }
        self.home.set(frame.anchor);
        set_window_anchor(&self.window, self.dodged.get().unwrap_or(frame.anchor));
        self.window.set_opacity(frame.opacity.clamp(0.05, 1.0));

        if frame.visible && !frame.keys.is_empty() {
            let paint = self.paint_scale(&frame.keys, frame.scale);
            self.paint.set(paint);
            let (w, h) = content_size(&frame.keys, paint);
            self.area.set_content_width(w as i32);
            self.area.set_content_height(h as i32);
            self.window.set_default_size(w as i32, h as i32);
            self.window.set_visible(true);
            apply_input_region(&self.window, self.mode.get());
            self.area.queue_draw();
        } else {
            self.window.set_visible(false);
        }
    }

    /// Call each tick. While dodged, return home once the pointer has left
    /// the home area. Wayland shows no pointer outside our own surface, so
    /// ask the compositor where the cursor is.
    pub fn tick_dodge(&self, now: Instant) {
        let Some(since) = self.dodged_at.get() else {
            return;
        };
        if self
            .last_poll
            .get()
            .is_some_and(|t| now.duration_since(t) < DODGE_POLL)
        {
            return;
        }
        self.last_poll.set(Some(now));
        let back = match (cursor_position(), self.home_rect()) {
            (Some((cx, cy)), Some((x, y, w, h))) => {
                cx < x - DODGE_RETURN_PAD
                    || cx > x + w + DODGE_RETURN_PAD
                    || cy < y - DODGE_RETURN_PAD
                    || cy > y + h + DODGE_RETURN_PAD
            }
            _ => now.duration_since(since) >= DODGE_BLIND_RETURN,
        };
        if back {
            self.dodged.set(None);
            self.dodged_at.set(None);
        }
    }

    /// Home area in layout coordinates: x, y, width, height.
    fn home_rect(&self) -> Option<(f64, f64, f64, f64)> {
        let m = self.monitor()?.geometry();
        let (w, h) = (f64::from(self.area.width()), f64::from(self.area.height()));
        let (gx, gy) = self.home.get().grid();
        let place = |origin: i32, size: i32, len: f64, g: i32| {
            let (origin, size, edge) = (f64::from(origin), f64::from(size), f64::from(EDGE_MARGIN));
            match g {
                -1 => origin + edge,
                1 => origin + size - edge - len,
                _ => origin + (size - len) / 2.0,
            }
        };
        Some((
            place(m.x(), m.width(), w, gx),
            place(m.y(), m.height(), h, gy),
            w,
            h,
        ))
    }

    /// Configured overlay-width fraction → paint scale relative to the 44 px
    /// design unit, so the painted keyboard spans that fraction of the screen's
    /// shorter side.
    fn paint_scale(&self, keys: &[KeyView], screen_fraction: f64) -> f64 {
        let full_width = content_size(keys, 1.0).0;
        self.screen_min_px().unwrap_or(REFERENCE_SCREEN_MIN) * screen_fraction / full_width
    }

    /// Shorter side of the monitor this surface is on, in logical pixels.
    /// Prefers the surface's actual monitor, then the layer-shell requested
    /// monitor, then the first display monitor (before the surface exists).
    fn screen_min_px(&self) -> Option<f64> {
        self.monitor().map(|m| {
            let geometry = m.geometry();
            f64::from(geometry.width().min(geometry.height()))
        })
    }

    fn monitor(&self) -> Option<gtk::gdk::Monitor> {
        let display = gtk::prelude::RootExt::display(&self.window);
        self.window
            .surface()
            .and_then(|surface| display.monitor_at_surface(&surface))
            .or_else(|| self.window.monitor())
            .or_else(|| {
                display
                    .monitors()
                    .item(0)
                    .and_then(|obj| obj.downcast::<gtk::gdk::Monitor>().ok())
            })
    }
}

/// Cursor position in layout coordinates, from the Hyprland socket.
fn cursor_position() -> Option<(f64, f64)> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let path = std::path::Path::new(&runtime)
        .join("hypr")
        .join(sig)
        .join(".socket.sock");
    let mut stream = UnixStream::connect(path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .ok()?;
    stream.write_all(b"cursorpos").ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    let (x, y) = reply.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn set_window_anchor(w: &ApplicationWindow, anchor: Anchor) {
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        w.set_anchor(edge, false);
    }
    match anchor {
        Anchor::TopLeft => {
            w.set_anchor(Edge::Top, true);
            w.set_anchor(Edge::Left, true);
        }
        Anchor::Top => {
            // Single edge only: opposite anchors stretch the surface.
            w.set_anchor(Edge::Top, true);
        }
        Anchor::TopRight => {
            w.set_anchor(Edge::Top, true);
            w.set_anchor(Edge::Right, true);
        }
        Anchor::Left => {
            w.set_anchor(Edge::Left, true);
        }
        Anchor::Right => {
            w.set_anchor(Edge::Right, true);
        }
        Anchor::BottomLeft => {
            w.set_anchor(Edge::Bottom, true);
            w.set_anchor(Edge::Left, true);
        }
        Anchor::Bottom => {
            w.set_anchor(Edge::Bottom, true);
        }
        Anchor::BottomRight => {
            w.set_anchor(Edge::Bottom, true);
            w.set_anchor(Edge::Right, true);
        }
    }
}

/// Empty GDK input region → compositor ignores pointer on this surface, so
/// clicks pass through to windows below. Tooltip and dodge need pointer
/// events, so they set a region far larger than the surface (clamped by the
/// compositor) and swallow clicks over the overlay area instead.
fn apply_input_region(window: &ApplicationWindow, mode: HoverMode) {
    let Some(surface) = window.surface() else {
        return;
    };
    if mode != HoverMode::Off {
        // GDK binding only exposes a non-null region; an oversized rectangle
        // effectively means the whole surface.
        let rect = RectangleInt::new(0, 0, i32::MAX / 2, i32::MAX / 2);
        surface.set_input_region(&Region::create_rectangle(&rect));
    } else {
        surface.set_input_region(&Region::create());
    }
}

fn content_size(keys: &[KeyView], scale: f64) -> (f64, f64) {
    let unit = UNIT_PX * scale;
    let pad = PAD * scale;
    let mut max_x: f64 = 0.0;
    let mut max_y: f64 = 0.0;
    for k in keys {
        max_x = max_x.max((k.x + k.w) * unit);
        max_y = max_y.max((k.y + k.h) * unit);
    }
    (max_x + pad * 2.0, max_y + pad * 2.0 + 18.0 * scale)
}

fn parse_rgba(hex: &str) -> RGBA {
    let h = hex.trim().trim_start_matches('#');
    if h.len() >= 6 {
        let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0) as f32 / 255.0;
        let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0) as f32 / 255.0;
        let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0) as f32 / 255.0;
        RGBA::new(r, g, b, 1.0)
    } else {
        RGBA::new(0.1, 0.1, 0.15, 1.0)
    }
}

fn escape_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Wrap Omarchy Super PUA glyph in the `omarchy` icon font; leave other text as Sans.
fn legend_markup(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch == OMARCHY_SUPER.chars().next().unwrap() {
            out.push_str("<span font_family=\"omarchy\">");
            out.push(ch);
            out.push_str("</span>");
        } else {
            out.push_str(&escape_markup(&ch.to_string()));
        }
    }
    out
}

/// Fixed-size tap legend: wrap inside the key; only shrink if height still overflows.
fn tap_legend_layout(
    cr: &gtk::cairo::Context,
    text: &str,
    max_w: f64,
    max_h: f64,
    scale: f64,
) -> (pango::Layout, i32) {
    const BASE_PT: f64 = 13.0;
    const MIN_PT: f64 = 8.0;
    let markup = legend_markup(text);
    let width = (max_w * f64::from(pango::SCALE)) as i32;
    let mut pt = (BASE_PT * scale).round().max(MIN_PT) as i32;
    let min_pt = MIN_PT.round().max(6.0) as i32;

    loop {
        let layout = create_layout(cr);
        layout.set_font_description(Some(&FontDescription::from_string(&format!(
            "{LEGEND_FONT} {pt}"
        ))));
        layout.set_width(width);
        layout.set_wrap(WrapMode::WordChar);
        layout.set_alignment(Alignment::Center);
        layout.set_markup(&markup);
        let (_tw, th) = layout.pixel_size();
        if f64::from(th) <= max_h || pt <= min_pt {
            return (layout, th);
        }
        pt -= 1;
    }
}

/// Draw a single glyph centred by its ink extents: Nerd Font icons often have
/// off-centre bearings or advances wider than the key, which look shifted.
fn draw_glyph_centered(
    cr: &gtk::cairo::Context,
    glyph: &str,
    x: f64,
    y: f64,
    kw: f64,
    kh: f64,
    scale: f64,
) {
    const BASE_PT: f64 = 13.0;
    const MIN_PT: f64 = 8.0;
    let markup = legend_markup(glyph);
    let mut pt = (BASE_PT * scale).round().max(MIN_PT) as i32;
    let min_pt = MIN_PT.round().max(6.0) as i32;
    let layout = loop {
        let layout = create_layout(cr);
        layout.set_font_description(Some(&FontDescription::from_string(&format!(
            "{LEGEND_FONT} {pt}"
        ))));
        layout.set_markup(&markup);
        let (tw, _th) = layout.pixel_size();
        if f64::from(tw) <= kw || pt <= min_pt {
            break layout;
        }
        pt -= 1;
    };
    let (ink, logical) = layout.pixel_extents();
    let dx = x + (kw - f64::from(ink.width())) / 2.0 - f64::from(ink.x());
    let dy = y + (kh - f64::from(logical.height())) / 2.0;
    cr.move_to(dx, dy);
    show_layout(cr, &layout);
}

/// Small hold legend in the key's top-right corner: shrinks to fit one line
/// so it never wraps into the centered tap label.
fn hold_legend_layout(
    cr: &gtk::cairo::Context,
    text: &str,
    max_w: f64,
    scale: f64,
) -> pango::Layout {
    const BASE_PT: f64 = 10.0;
    const MIN_PT: f64 = 5.0;
    let markup = legend_markup(text);
    let mut pt = (BASE_PT * scale).round().max(MIN_PT) as i32;
    let min_pt = MIN_PT.round().max(5.0) as i32;

    loop {
        let layout = create_layout(cr);
        layout.set_font_description(Some(&FontDescription::from_string(&format!(
            "{LEGEND_FONT} {pt}"
        ))));
        layout.set_markup(&markup);
        let (tw, _th) = layout.pixel_size();
        if f64::from(tw) <= max_w || pt <= min_pt {
            if f64::from(tw) > max_w {
                layout.set_width((max_w * f64::from(pango::SCALE)).round() as i32);
                layout.set_ellipsize(EllipsizeMode::End);
            }
            return layout;
        }
        pt -= 1;
    }
}

fn draw_keyboard(
    cr: &gtk::cairo::Context,
    frame: &Frame,
    theme: &Theme,
    paint_scale: f64,
    pointer: Option<(f64, f64)>,
    flash: Option<usize>,
) {
    let bg = parse_rgba(&theme.background);
    let fg = parse_rgba(&theme.foreground);
    let accent = parse_rgba(&theme.accent);
    let muted = parse_rgba(&theme.muted);
    let scale = paint_scale;
    let unit = UNIT_PX * scale;
    let pad = PAD * scale;
    let gap = 4.0 * scale;
    let header_h = 16.0 * scale;
    let radius = 6.0 * scale;

    let (w, h) = content_size(&frame.keys, scale);
    cr.set_source_rgba(
        f64::from(bg.red()),
        f64::from(bg.green()),
        f64::from(bg.blue()),
        0.92,
    );
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    cr.set_source_rgba(
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
        0.85,
    );
    let header_text = match &frame.kind {
        FrameKind::Layer => frame.layer_name.clone(),
        FrameKind::Binds(b) => b.header.clone(),
    };
    let header = hold_legend_layout(cr, &header_text, w - 2.0 * pad, scale);
    cr.move_to(pad, pad - 2.0 * scale);
    show_layout(cr, &header);

    let hovered = pointer.and_then(|(px, py)| key_at(&frame.keys, scale, px, py));

    for (idx, k) in frame.keys.iter().enumerate() {
        let x = pad + k.x * unit;
        let y = pad + header_h + k.y * unit;
        let kw = k.w * unit - gap;
        let kh = k.h * unit - gap;

        if k.held || flash == Some(idx) {
            // Held = accent fill; a type-mode click briefly lights the key the
            // same way so the click visibly lands.
            cr.set_source_rgba(
                f64::from(accent.red()),
                f64::from(accent.green()),
                f64::from(accent.blue()),
                0.95,
            );
        } else if k.special || k.bind.is_some() {
            // Accent-tinted idle fill so mods / layer keys stand out.
            cr.set_source_rgba(
                f64::from(accent.red()),
                f64::from(accent.green()),
                f64::from(accent.blue()),
                0.38,
            );
        } else {
            cr.set_source_rgba(
                f64::from(muted.red()),
                f64::from(muted.green()),
                f64::from(muted.blue()),
                0.55,
            );
        }
        rounded_rect(cr, x, y, kw, kh, radius);
        let _ = cr.fill();

        if k.locked {
            // Lock bar along the bottom edge: a double-clicked mod or layer
            // stays on until clicked again, unlike a one-shot latch.
            cr.set_source_rgba(
                f64::from(fg.red()),
                f64::from(fg.green()),
                f64::from(fg.blue()),
                0.9,
            );
            rounded_rect(
                cr,
                x + kw * 0.25,
                y + kh - 5.0 * scale,
                kw * 0.5,
                2.5 * scale,
                1.2 * scale,
            );
            let _ = cr.fill();
        }

        if hovered == Some(idx) {
            cr.set_source_rgba(
                f64::from(accent.red()),
                f64::from(accent.green()),
                f64::from(accent.blue()),
                0.9,
            );
            cr.set_line_width(1.5 * scale);
            rounded_rect(cr, x, y, kw, kh, radius);
            let _ = cr.stroke();
        }

        // Held = accent fill. Use bright fg for legends (dark-on-accent was
        // still hard to read). Idle uses normal fg.
        let (tr, tg, tb) = (
            f64::from(fg.red()),
            f64::from(fg.green()),
            f64::from(fg.blue()),
        );
        // Dimmed legends are context rather than the point: a key inherited
        // from the layer below, or a non-bind key while a bind layer shows.
        let ink = |alpha: f64| if k.dimmed { alpha * 0.45 } else { alpha };
        let has_hold = k.hold_label.as_deref().is_some_and(|h| !h.is_empty());

        // Centred text shrinks and drops below the top strip a corner hold
        // badge occupies.
        let centered = |text: &str, max_h: f64| {
            let max_h = if has_hold {
                max_h.min((kh - 6.0 * scale).max(8.0 * scale))
            } else {
                max_h
            };
            let (layout, th) = tap_legend_layout(cr, text, kw, max_h, scale);
            let y_off = if has_hold {
                (kh - f64::from(th)) / 2.0 + 3.0 * scale
            } else {
                (kh - f64::from(th)) / 2.0
            };
            (layout, y_off.max(0.0))
        };

        // Bind layer: one legend per key, glyph or description by label style.
        if let Some(bind) = &k.bind {
            let glyph = bind.glyph.as_deref().filter(|g| !g.is_empty());
            let description = bind.description.as_str();
            if !description.is_empty() {
                if let Some(glyph) = glyph {
                    cr.set_source_rgba(tr, tg, tb, ink(1.0));
                    draw_glyph_centered(cr, glyph, x, y, kw, kh * 0.62, scale);
                    cr.set_source_rgba(tr, tg, tb, ink(0.85));
                    let (layout, th) = tap_legend_layout(cr, description, kw, kh * 0.40, scale);
                    cr.move_to(x, y + kh - f64::from(th) - 1.5 * scale);
                    show_layout(cr, &layout);
                } else {
                    cr.set_source_rgba(tr, tg, tb, ink(0.9));
                    let (layout, y_off) = centered(description, kh * 0.80);
                    cr.move_to(x, y + y_off);
                    show_layout(cr, &layout);
                }
            } else if let Some(glyph) = glyph {
                cr.set_source_rgba(tr, tg, tb, ink(1.0));
                draw_glyph_centered(cr, glyph, x, y, kw, kh, scale);
            }
        } else if !k.label.is_empty() {
            // Tap first (center). One base size for all keys; long labels wrap
            // instead of shrinking. Shrink only if still taller than the keycap.
            cr.set_source_rgba(tr, tg, tb, ink(1.0));
            let (layout, y_off) = centered(&k.label, (kh - 2.0 * scale).max(8.0 * scale));
            cr.move_to(x, y + y_off);
            show_layout(cr, &layout);
        }

        if let Some(hold) = k.hold_label.as_deref() {
            if !hold.is_empty() {
                let hold_w = (kw * 0.5).max(14.0 * scale);
                let layout = hold_legend_layout(cr, hold, hold_w, scale);
                let (tw, _th) = layout.pixel_size();
                cr.set_source_rgba(tr, tg, tb, ink(if k.held { 1.0 } else { 0.75 }));
                cr.move_to(x + kw - f64::from(tw) - 2.0 * scale, y + 1.0 * scale);
                show_layout(cr, &layout);
            }
        }
    }

    if let Some(idx) = hovered {
        let k = &frame.keys[idx];
        if !k.tooltip.is_empty() {
            let x = pad + k.x * unit;
            let y = pad + header_h + k.y * unit;
            let kw = k.w * unit - gap;
            let kh = k.h * unit - gap;
            draw_tooltip(
                cr, &k.tooltip, x, y, kw, kh, w, h, pad, scale, &fg, &bg, radius,
            );
        }
    }
}

/// Index into `keys` of the key under a point in drawing-area coordinates.
fn key_at(keys: &[KeyView], scale: f64, x: f64, y: f64) -> Option<usize> {
    let unit = UNIT_PX * scale;
    let pad = PAD * scale;
    let header_h = 16.0 * scale;
    let gap = 4.0 * scale;
    keys.iter().position(|k| {
        let kx = pad + k.x * unit;
        let ky = pad + header_h + k.y * unit;
        x >= kx && x <= kx + k.w * unit - gap && y >= ky && y <= ky + k.h * unit - gap
    })
}

/// Hover tooltip: the key's full text in a themed box near the key, flipped
/// below when there is no room above, clamped to the overlay.
#[allow(clippy::too_many_arguments)]
fn draw_tooltip(
    cr: &gtk::cairo::Context,
    text: &str,
    key_x: f64,
    key_y: f64,
    key_w: f64,
    key_h: f64,
    content_w: f64,
    content_h: f64,
    pad: f64,
    scale: f64,
    fg: &RGBA,
    bg: &RGBA,
    radius: f64,
) {
    const BASE_PT: f64 = 13.0;
    const MIN_PT: f64 = 11.0;
    let markup = legend_markup(text);
    let max_w = (content_w - 2.0 * pad).max(40.0 * scale);
    let pt = (BASE_PT * scale).round().max(MIN_PT) as i32;

    let layout = create_layout(cr);
    layout.set_font_description(Some(&FontDescription::from_string(&format!(
        "{LEGEND_FONT} {pt}"
    ))));
    layout.set_width((max_w * f64::from(pango::SCALE)) as i32);
    layout.set_wrap(WrapMode::WordChar);
    layout.set_alignment(Alignment::Left);
    layout.set_markup(&markup);

    let (tw, th) = layout.pixel_size();
    let box_pad = (6.0 * scale).max(4.0);
    let bw = f64::from(tw) + 2.0 * box_pad;
    let bh = f64::from(th) + 2.0 * box_pad;
    let cx = key_x + key_w / 2.0;
    let max_x = (content_w - pad - bw).max(pad);
    let bx = (cx - bw / 2.0).clamp(pad, max_x);
    let mut by = key_y - bh - 4.0 * scale;
    if by < pad {
        by = key_y + key_h + 4.0 * scale;
    }
    let max_y = (content_h - pad - bh).max(pad);
    let by = by.clamp(pad, max_y);

    cr.set_source_rgba(
        f64::from(bg.red()),
        f64::from(bg.green()),
        f64::from(bg.blue()),
        0.97,
    );
    rounded_rect(cr, bx, by, bw, bh, radius);
    let _ = cr.fill();

    cr.set_source_rgba(
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
        1.0,
    );
    cr.move_to(bx + box_pad, by + box_pad);
    show_layout(cr, &layout);
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(row: u8, col: u8, x: f64, y: f64) -> KeyView {
        KeyView {
            row,
            col,
            x,
            y,
            w: 1.0,
            h: 1.0,
            label: String::new(),
            hold_label: None,
            held: false,
            special: false,
            bind: None,
            tooltip: String::new(),
            keycode: 0,
            dimmed: false,
            locked: false,
        }
    }

    #[test]
    fn key_at_hits_key_rects_only() {
        let keys = vec![key(0, 0, 0.0, 0.0), key(0, 1, 1.0, 0.0)];
        let unit = UNIT_PX;
        let kx = PAD + unit;
        let ky = PAD + 16.0 + 0.0;
        assert_eq!(key_at(&keys, 1.0, PAD + 2.0, ky + 2.0), Some(0));
        assert_eq!(key_at(&keys, 1.0, kx + 2.0, ky + 2.0), Some(1));
        assert_eq!(key_at(&keys, 1.0, kx - 6.0, ky + 2.0), Some(0));
        assert_eq!(key_at(&keys, 1.0, PAD - 5.0, PAD), None);
        assert_eq!(key_at(&keys, 1.0, kx + unit, ky + 2.0), None);
    }

    fn render(keys: Vec<KeyView>, scale: f64) -> cairo::ImageSurface {
        let frame = Frame {
            visible: true,
            opacity: 0.92,
            scale,
            anchor: Anchor::BottomRight,
            layer: 0,
            layer_name: String::new(),
            kind: FrameKind::Layer,
            keys,
        };
        let (w, h) = content_size(&frame.keys, scale);
        let surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, w.ceil() as i32, h.ceil() as i32)
                .unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        draw_keyboard(&cr, &frame, &Theme::default(), scale, None, None);
        surface.flush();
        surface
    }

    fn diff_pixels(
        a: &mut cairo::ImageSurface,
        b: &mut cairo::ImageSurface,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
    ) -> usize {
        let (a_stride, b_stride) = (a.stride() as usize, b.stride() as usize);
        let a_data = a.data().unwrap();
        let b_data = b.data().unwrap();
        let mut diff = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                let ao = y as usize * a_stride + x as usize * 4;
                let bo = y as usize * b_stride + x as usize * 4;
                if a_data[ao..ao + 4] != b_data[bo..bo + 4] {
                    diff += 1;
                }
            }
        }
        diff
    }

    #[test]
    fn bind_key_draws_hold_badge_in_top_right() {
        let mut shown = key(0, 0, 0.0, 0.0);
        shown.bind = Some(crate::core::state::BindView {
            glyph: Some("\u{f120}".into()),
            description: "Terminal".into(),
        });
        shown.hold_label = Some("Ctrl".into());
        let mut quiet = shown.clone();
        quiet.hold_label = None;

        let scale = 1.0;
        let mut with_badge = render(vec![shown], scale);
        let mut without_badge = render(vec![quiet], scale);

        let unit = UNIT_PX * scale;
        let pad = PAD * scale;
        let kx = pad as i32;
        let ky = (pad + 16.0 * scale) as i32;
        let kw = (unit - 4.0 * scale) as i32;
        let kh = (unit - 4.0 * scale) as i32;
        let (w, h) = (with_badge.width(), with_badge.height());
        let region = diff_pixels(
            &mut with_badge,
            &mut without_badge,
            kx + kw * 2 / 5,
            ky,
            kx + kw,
            ky + kh * 2 / 5,
        );
        let total = diff_pixels(&mut with_badge, &mut without_badge, 0, 0, w, h);
        assert!(region > 0, "bind key with a hold role draws a corner badge");
        assert_eq!(
            total, region,
            "badge is the only new ink and sits top right"
        );
    }
}
