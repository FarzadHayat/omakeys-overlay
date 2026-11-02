# omakeys reference

Details behind the [README](../README.md).

## Requirements

- Omarchy / Hyprland (Wayland), GTK4 and gtk4-layer-shell
- Membership of the `input` group, so your user can read key events and open a QMK board's hidraw. The install script adds you and writes a udev rule, with sudo. Log out and in once after.
- Write access to `/dev/uinput` for the `type` hover mode. Without it, clicks in that mode do nothing.

## Boards

`board` in `config.toml` (or the panel's Board picker):

- `auto` (default): the overlay follows whichever keyboard you type on.
- `generic`: the built-in ANSI TKL catch-all. Any USB, Bluetooth or laptop keyboard the kernel reports. US geometry, no layers.
- `loremipsum36`, `samsung-trio-500`, or the name of your `boards/*.toml`: lock to that profile.

**Generic** reads every keyboard that no other profile claims and draws F-row groups, nav cluster, inverted-T arrows and Fn. Sleeping Bluetooth boards are picked up again when they wake. A QMK board with a Raw HID profile is never also read as Generic.

**Samsung Trio 500** is a built-in evdev profile (Fn layer inference, media and app legends).

**QMK boards, full layer map** need a board profile (geometry and layer names) and firmware that reports layers over Raw HID. Use the `omakeys_notify` module ([source](https://github.com/FarzadHayat/omakeys/tree/main/modules/omakeys/omakeys_notify)), or the older KeyPeek / LayerLens modules plus VIA. Stock QMK or VIA only gives you a static keymap at best.

Custom board profiles: copy `examples/boards/example.toml` to `~/.config/omakeys/boards/<name>.toml`. New files load within a couple of seconds. Rerun the install script after adding a board so the udev rule covers its USB ids. Boards running other people's firmware should leave `custom_keycodes` off (the default): those codes are the author's own macros.

With several keyboards connected, Auto switches on keypress. With several QMK boards plugged in at once, startup picks the first matching profile until you type on another. Pin a name to lock one layout.

`omakeys keys` prints Linux evdev codes as you press keys, for wiring a layout.

## Config

`~/.config/omakeys/config.toml`:

| Key | Default | Meaning |
| --- | --- | --- |
| `board` | `auto` | `auto` or a profile name |
| `mode` | `auto` | `auto` shows while you type, `always` stays up |
| `timeout_ms` | `2000` | how long it stays after the last key |
| `anchor` | `bottom_right` | screen corner or edge |
| `scale` | `0.5` | size, 0.2 to 1.0 of the shorter screen side |
| `opacity` | `0.8` | transparency, 0.05 to 1.0 |
| `tray_visible` | `true` | show the bar icon |
| `toggle_chord` | `SUPER+ALT+O` | label shown for the toggle key (it does not create the bind) |
| `label_style` | `icons` | `icons` or text legends |
| `[binds] enabled` | `true` | `false` never swaps to modifier bind layers |
| `[binds] standard` | `true` | adds built-in Ctrl hints and Alt F4, skipping any a real bind already covers. `false` shows only real host binds |
| `[hover] mode` | `type` | `type`, `dodge`, `tooltip`, `off` |

While a modifier is held the overlay shows that combo's host binds, read from `omarchy menu keybindings --print` and refreshed when your Hyprland config changes. Bound keys draw their legend, unbound keys go blank.

### Hover modes

- `type` (default): an on-screen keyboard. Tooltips as below, and a left click sends the key to the focused window through a virtual keyboard.
- `dodge`: the overlay hops to the next spot along the screen edge, away from the pointer, so you can read what is underneath. It returns when the pointer leaves its home area.
- `tooltip`: a tooltip while the pointer rests on a key, only where the key text cannot speak for itself (bind description, icon name, hold role).
- `off`: ignores the pointer and stays fully click-through.

`dodge`, `tooltip` and `type` make the overlay accept input, so clicks over its area do not reach windows below.

### Type mode

- A quick click taps a key. Hold the click about a quarter second to latch a dual-role key's hold role (modifier or layer).
- Clicked modifiers and layer keys latch one-shot and release after the next key. Latches stack: Super, then SYM, then a key sends that key from SYM with Super.
- Click a lit key again to cancel it. Double click to lock it (shown with a bar) until you click it again.
- Right click anywhere on the overlay drops every latch and lock.
- Any physical board use clears latches.
- TG, TO and DF layer keys switch a layer for good. Click again to leave.
- Zoom mute, raise hand and screenshot keys send their Linux shortcuts (Alt+A, Alt+Y, Print). RGB and bootloader keys click to nothing.
- The overlay never takes keyboard focus, so typing lands in the window you are working in.

## Per-key overrides

`~/.config/omakeys/overrides.toml`:

```toml
[[override]]
layer = 2
row = 0
col = 3
label = "‽"

[[binds]]
match = "Terminal"   # bind description, case-insensitive
glyph = "T"          # empty string hides the glyph
```

## CLI

```bash
omakeys run        # daemon (default)
omakeys toggle     # show/hide
omakeys settings   # open settings
omakeys boards     # list board picker entries as JSON
omakeys keys       # print evdev codes as you press keys
omakeys doctor     # check setup, board and permissions
omakeys udev-rules # print the udev rule for every known board
```
