//! omakeys protocol wire codec (magic `0x4F`, 32-byte Raw HID reports) and
//! `OmakeysBoardClient`, the native client that speaks it.
//!
//! Client handoff rules (chosen once per app run):
//! - Supported INFO at startup → the protocol client is used for the whole run.
//! - Supported INFO with no dynamic keymap (flags bit0 clear, zero layers, or
//!   zero advertised bytes) → still the protocol client, with the profile-shaped
//!   empty keymap: live layer/key events work, labels are unavailable.
//! - Supported INFO whose session start fails (keymap fetch, SUBSCRIBE, or
//!   GET_STATE) → still the protocol client: the device is dropped and `pump`
//!   re-probes on the reconnect backoff, refetching the keymap then.
//! - Board absent at startup → soft-connects as the protocol client, retrying
//!   in `pump`.
//! - Board present but probe unanswered or malformed INFO at startup → the
//!   factory falls back to the legacy client, which owns reconnect for that
//!   run.
//! - INFO seen but major version unsupported → no legacy fallback: the
//!   protocol client stays in an unsupported state (device closed, `ok` is
//!   false) and retries the probe on the reconnect backoff, keeping the last
//!   good state.
//! - Reflashing between protocol and legacy firmware families needs an app
//!   restart; the client choice is not renegotiated in-process.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use hidapi::{HidApi, HidDevice};

use crate::board::client::{BoardClient, Keymap, USAGE, USAGE_PAGE};
use crate::board::profile::BoardProfile;
use crate::board::protocol::{HostEvent, REPORT_LEN};

pub const MAGIC: u8 = 0x4F;

pub const OPC_HELLO: u8 = 0x01;
pub const OPC_GET_STATE: u8 = 0x02;
pub const OPC_SUBSCRIBE: u8 = 0x03;
pub const OPC_UNSUBSCRIBE: u8 = 0x04;
pub const OPC_GET_KEYMAP: u8 = 0x05;
pub const OPC_INFO: u8 = 0x81;
pub const OPC_STATE: u8 = 0x82;
pub const OPC_LAYER: u8 = 0x83;
pub const OPC_KEY: u8 = 0x84;
pub const OPC_KEYMAP: u8 = 0x85;

/// KEYMAP data bytes per frame: `REPORT_LEN` minus magic, op, offset (u16), len.
pub const MAX_CHUNK: u8 = 27;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub ver_major: u8,
    pub ver_minor: u8,
    pub rows: u8,
    pub cols: u8,
    pub layers: u8,
    pub flags: u8,
    pub keymap_bytes: u16,
}

impl Info {
    /// `flags` bit 0: dynamic keymap available on the firmware.
    pub fn keymap_available(&self) -> bool {
        self.flags & 1 != 0
    }

    /// True when the session start should fetch labels: the firmware
    /// advertises the dynamic keymap and reports a nonempty span. A keymap-less
    /// board still connects for live layer/key events, with an empty keymap.
    pub fn keymap_fetchable(&self) -> bool {
        self.keymap_available() && self.layers > 0 && self.keymap_bytes > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Info(Info),
    State { default_layer: u8, layer_state: u32 },
    Key { row: u8, col: u8, pressed: bool },
    Keymap { offset: u16, data: Vec<u8> },
    Other,
}

pub fn hello_req() -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = MAGIC;
    buf[1] = OPC_HELLO;
    buf
}

pub fn get_state_req() -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = MAGIC;
    buf[1] = OPC_GET_STATE;
    buf
}

pub fn subscribe_req() -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = MAGIC;
    buf[1] = OPC_SUBSCRIBE;
    buf
}

pub fn unsubscribe_req() -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = MAGIC;
    buf[1] = OPC_UNSUBSCRIBE;
    buf
}

pub fn get_keymap_req(offset: u16, len: u8) -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = MAGIC;
    buf[1] = OPC_GET_KEYMAP;
    buf[2] = (offset & 0xFF) as u8;
    buf[3] = (offset >> 8) as u8;
    buf[4] = len;
    buf
}

/// Parse an inbound report. LAYER shares STATE's payload shape.
pub fn classify_frame(buf: &[u8]) -> Frame {
    if buf.len() < 2 || buf[0] != MAGIC {
        return Frame::Other;
    }
    match buf[1] {
        OPC_INFO => {
            if buf.len() < 10 {
                return Frame::Other;
            }
            Frame::Info(Info {
                ver_major: buf[2],
                ver_minor: buf[3],
                rows: buf[4],
                cols: buf[5],
                layers: buf[6],
                flags: buf[7],
                keymap_bytes: u16::from_le_bytes([buf[8], buf[9]]),
            })
        }
        OPC_STATE | OPC_LAYER => {
            if buf.len() < 8 {
                return Frame::Other;
            }
            Frame::State {
                default_layer: buf[2],
                layer_state: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            }
        }
        OPC_KEY => {
            if buf.len() < 5 {
                return Frame::Other;
            }
            let (row, col, pressed) = (buf[2], buf[3], buf[4]);
            if row > 15 || col > 15 || pressed > 1 {
                return Frame::Other;
            }
            Frame::Key {
                row,
                col,
                pressed: pressed != 0,
            }
        }
        OPC_KEYMAP => {
            if buf.len() < 5 {
                return Frame::Other;
            }
            let offset = u16::from_le_bytes([buf[2], buf[3]]);
            let len = buf[4] as usize;
            if len > MAX_CHUNK as usize || buf.len() < 5 + len {
                return Frame::Other;
            }
            Frame::Keymap {
                offset,
                data: buf[5..5 + len].to_vec(),
            }
        }
        _ => Frame::Other,
    }
}

pub fn state_event(default_layer: u8, layer_state: u32) -> HostEvent {
    HostEvent::Layer {
        default_layer: default_layer as u32,
        layer_state,
    }
}

// --- OmakeysBoardClient ----------------------------------------------------

/// Keepalive cadence and reconnect backoff mirror the legacy client. Local so
/// `client.rs` keeps its constants private (Task 3 owns shared changes).
const KEEPALIVE: Duration = Duration::from_millis(1500);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// HELLO probe: 5 writes, each scanning up to 100 ms for INFO.
const PROBE_ATTEMPTS: u32 = 5;
const PROBE_WINDOW: Duration = Duration::from_millis(100);

/// Keymap chunks: up to 2 attempts per chunk.
const CHUNK_ATTEMPTS: u32 = 2;

/// Reply wait: up to 50 × 20 ms of reads per request.
const READ_RETRIES: u32 = 50;
const READ_TIMEOUT_MS: i32 = 20;

/// Chunk schedule for reading `total` keymap bytes: `(offset, len)` pairs with
/// `len <= MAX_CHUNK`, tiling `[0, total)` in order.
fn chunk_requests(total: u16) -> Vec<(u16, u8)> {
    let mut out = Vec::new();
    let mut offset = 0u16;
    while (offset as u32) < u32::from(total) {
        let len = (u32::from(total) - u32::from(offset)).min(u32::from(MAX_CHUNK)) as u8;
        out.push((offset, len));
        offset += u16::from(len);
    }
    out
}

/// Concatenate chunk replies, verifying they tile the stream from offset 0.
/// `None` on offset mismatch or a stream shorter than `total`; bytes past
/// `total` are ignored.
fn join_chunks(chunks: &[(u16, Vec<u8>)], total: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(total);
    let mut expected = 0u32;
    for (offset, data) in chunks {
        if u32::from(*offset) != expected {
            return None;
        }
        out.extend_from_slice(data);
        expected += data.len() as u32;
    }
    if out.len() < total {
        return None;
    }
    out.truncate(total);
    Some(out)
}

/// Layers to read and their byte length: clamped to the profile draw range,
/// the firmware layer count, and the advertised keymap buffer size.
fn keymap_span(max_layer_inclusive: u8, info: &Info) -> (u8, u16) {
    let layers = (u16::from(max_layer_inclusive) + 1).min(u16::from(info.layers)) as u8;
    let layer_bytes = u32::from(info.rows) * u32::from(info.cols) * 2;
    let total = (u32::from(layers) * layer_bytes).min(u32::from(info.keymap_bytes)) as u16;
    (layers, total)
}

/// Decode QMK's dynamic keymap buffer (layer-major, then row, col; 2 bytes BE
/// per cell, QMK stores keycodes big-endian) into a `Keymap`. Missing bytes
/// stay 0, leftover bytes ignored.
fn fill_keymap(bytes: &[u8], layers: u8, rows: u8, cols: u8) -> Keymap {
    let mut map = Keymap::empty(layers as usize, rows as usize, cols as usize);
    for layer in 0..layers {
        for row in 0..rows {
            for col in 0..cols {
                let idx =
                    (layer as usize * rows as usize + row as usize) * cols as usize + col as usize;
                let idx = idx * 2;
                if idx + 2 <= bytes.len() {
                    map.set(
                        layer,
                        row,
                        col,
                        u16::from_be_bytes([bytes[idx], bytes[idx + 1]]),
                    );
                }
            }
        }
    }
    map
}

/// True when `frame` is the reply kind selected by `opcode`.
fn is_reply(frame: &Frame, opcode: u8) -> bool {
    matches!(
        (frame, opcode),
        (Frame::Info(_), OPC_INFO) | (Frame::Keymap { .. }, OPC_KEYMAP)
    )
}

/// HELLO probe outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeOutcome {
    /// Major 1 with sane dimensions: run the session.
    Supported(Info),
    /// Protocol seen but major unsupported: log, keep last good state, and
    /// retry the probe on the next reconnect. No legacy fallback, since a
    /// protocol-only board may not speak legacy at all.
    Unsupported,
}

/// Probe decision for an INFO. Major ≠ 1 is `Unsupported`; major 1 with zero
/// rows or cols is malformed (`Err`). A keymap-less INFO (no dynamic keymap)
/// is `Supported`: live events work and the keymap stays empty.
fn probe_outcome(info: &Info) -> Result<ProbeOutcome> {
    if info.ver_major != 1 {
        return Ok(ProbeOutcome::Unsupported);
    }
    if info.rows == 0 || info.cols == 0 {
        return Err(anyhow!("omakeys INFO malformed"));
    }
    Ok(ProbeOutcome::Supported(info.clone()))
}

pub struct OmakeysBoardClient {
    api: HidApi,
    vid: u16,
    pid: u16,
    device: Option<HidDevice>,
    map: Keymap,
    max_layer_inclusive: u8,
    profile_rows: u8,
    profile_cols: u8,
    last_keepalive: Instant,
    next_retry: Instant,
    /// True once an unsupported-major INFO has been logged; cleared when a
    /// supported INFO starts a session, so probe retries stay silent but a
    /// later transition into unsupported logs again.
    unsupported_logged: bool,
    /// Events read while waiting for a reply or draining pushes.
    pending: Vec<HostEvent>,
}

impl OmakeysBoardClient {
    /// Connect: `Err` when the board is present but does not answer HELLO or
    /// answers a malformed INFO, so the factory falls back to legacy for this
    /// app run. If the board is absent at startup, soft-connects as the
    /// protocol client so `pump` retries on reconnect backoff. An INFO with an
    /// unsupported major version returns `Ok` with the device closed and `ok()`
    /// false; there is no legacy fallback, and `pump` retries the probe on the
    /// reconnect backoff. A supported INFO whose session start fails (keymap
    /// fetch, SUBSCRIBE, GET_STATE) also returns `Ok` with the device closed,
    /// so `pump` retries instead of falling back. After a live connect, an
    /// unplug is retried in `pump`.
    pub fn open(profile: &BoardProfile) -> Result<Self> {
        let api = HidApi::new().context("hidapi init")?;
        let mut client = Self {
            api,
            vid: profile.vid,
            pid: profile.pid,
            device: None,
            map: Keymap::empty(
                (profile.max_layer_inclusive as usize) + 1,
                profile.rows as usize,
                profile.cols as usize,
            ),
            max_layer_inclusive: profile.max_layer_inclusive,
            profile_rows: profile.rows,
            profile_cols: profile.cols,
            last_keepalive: Instant::now(),
            next_retry: Instant::now(),
            unsupported_logged: false,
            pending: Vec::new(),
        };
        match client.try_connect() {
            Ok(_) => Ok(client),
            Err(e) if e.to_string().contains("raw HID not found") => {
                // Board absent: soft-connect, pump() retries on reconnect backoff.
                Ok(client)
            }
            Err(e) => Err(e),
        }
    }

    /// Open the device and start a session. `Err` on absence, silence, or
    /// malformed INFO; `Ok(Unsupported)` leaves the device closed for a later
    /// probe retry. A supported INFO whose session start fails also leaves
    /// the device closed, but as `Ok(Supported)`: the board answered, so
    /// `pump` retries instead of the factory falling back to legacy.
    fn try_connect(&mut self) -> Result<ProbeOutcome> {
        self.api.refresh_devices().ok();
        let path = self
            .api
            .device_list()
            .find(|d| {
                d.vendor_id() == self.vid
                    && d.product_id() == self.pid
                    && d.usage_page() == USAGE_PAGE
                    && d.usage() == USAGE
            })
            .map(|d| d.path().to_owned())
            .ok_or_else(|| {
                anyhow!(
                    "omakeys raw HID not found ({:04x}:{:04x} usage FF60/61)",
                    self.vid,
                    self.pid
                )
            })?;

        let device = self.api.open_path(&path).context("open hidraw")?;
        device.set_blocking_mode(false).context("nonblocking")?;
        self.device = Some(device);
        let outcome = self.start_session();
        if !matches!(outcome, Ok(ProbeOutcome::Supported(_))) {
            self.drop_device();
        }
        outcome
    }

    fn start_session(&mut self) -> Result<ProbeOutcome> {
        let outcome = self.probe_info()?;
        if let ProbeOutcome::Supported(info) = &outcome {
            match self.start_protocol(info) {
                Ok(()) => self.last_keepalive = Instant::now(),
                Err(e) => {
                    eprintln!("omakeys: session start failed after INFO ({e}); reconnecting");
                    self.drop_device();
                }
            }
        }
        Ok(outcome)
    }

    /// Post-INFO session start: keymap refetch when the firmware offers one,
    /// then SUBSCRIBE and GET_STATE. A keymap-less INFO keeps the profile-shaped
    /// empty keymap (live events on, labels unavailable) and logs once per
    /// session. Failures drop the device while keeping the supported outcome,
    /// so `pump` retries the session instead of the factory choosing legacy.
    fn start_protocol(&mut self, info: &Info) -> Result<()> {
        let has_keymap = info.keymap_fetchable();
        if has_keymap {
            self.fetch_keymap(info)?;
        } else {
            self.map = Keymap::empty(
                (self.max_layer_inclusive as usize) + 1,
                self.profile_rows as usize,
                self.profile_cols as usize,
            );
        }
        self.send_frame(&subscribe_req())?;
        self.send_frame(&get_state_req())?;
        if !has_keymap {
            eprintln!(
                "omakeys: INFO reports no dynamic keymap (flags {:#04x}, {} layers, {} bytes); labels unavailable, live events on",
                info.flags, info.layers, info.keymap_bytes
            );
        }
        Ok(())
    }

    /// HELLO handshake. Any non-INFO frame during the window is ignored.
    /// Malformed INFO (zero rows or cols) and silence are `Err` (factory falls
    /// back to legacy); an unsupported major is `Ok(Unsupported)`, logged once
    /// per transition into that state so probe retries stay silent.
    fn probe_info(&mut self) -> Result<ProbeOutcome> {
        for _ in 0..PROBE_ATTEMPTS {
            self.send_frame(&hello_req())?;
            let deadline = Instant::now() + PROBE_WINDOW;
            while Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let timeout_ms = remaining.as_millis().clamp(1, PROBE_WINDOW.as_millis()) as i32;
                let Some(frame) = self.read_one(timeout_ms)? else {
                    continue;
                };
                if let Frame::Info(info) = frame {
                    let outcome = probe_outcome(&info);
                    match &outcome {
                        Ok(ProbeOutcome::Unsupported) => {
                            if !self.unsupported_logged {
                                self.unsupported_logged = true;
                                eprintln!(
                                    "omakeys: unsupported INFO (ver {}.{}, {}x{}, {} layers); no legacy fallback, retrying probe",
                                    info.ver_major, info.ver_minor, info.rows, info.cols, info.layers
                                );
                            }
                        }
                        Err(_) => eprintln!(
                            "omakeys: malformed INFO (ver {}.{}, {}x{}, {} layers)",
                            info.ver_major, info.ver_minor, info.rows, info.cols, info.layers
                        ),
                        Ok(ProbeOutcome::Supported(_)) => self.unsupported_logged = false,
                    }
                    return outcome;
                }
            }
        }
        Err(anyhow!("no INFO reply to HELLO"))
    }

    /// Drain all available reports without blocking, routing pushes to
    /// `pending`. Read errors surface as a write failure on the next `pump`.
    fn read_available(&mut self) {
        if self.device.is_none() {
            return;
        }
        loop {
            match self.read_one(0) {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }

    /// Read one report with `timeout_ms`; `None` on timeout. STATE (also the
    /// LAYER push shape) and KEY are routed to `pending` as `HostEvent`s.
    fn read_one(&mut self, timeout_ms: i32) -> Result<Option<Frame>> {
        let mut buf = [0u8; REPORT_LEN];
        let n = {
            let dev = self.device.as_ref().ok_or_else(|| anyhow!("no device"))?;
            match dev.read_timeout(&mut buf, timeout_ms) {
                Ok(n) => n,
                Err(e) => return Err(e).context("hid read"),
            }
        };
        if n == 0 {
            return Ok(None);
        }
        let frame = classify_frame(&buf[..n]);
        match &frame {
            Frame::State {
                default_layer,
                layer_state,
            } => {
                self.pending.push(state_event(*default_layer, *layer_state));
            }
            Frame::Key { row, col, pressed } => {
                self.pending.push(HostEvent::Key {
                    row: *row,
                    col: *col,
                    pressed: *pressed,
                });
            }
            Frame::Info(_) | Frame::Keymap { .. } | Frame::Other => {}
        }
        Ok(Some(frame))
    }

    fn send_frame(&mut self, frame: &[u8; REPORT_LEN]) -> Result<()> {
        let dev = self.device.as_ref().ok_or_else(|| anyhow!("no device"))?;
        dev.write(frame).context("hid write")?;
        Ok(())
    }

    /// Write `req` and wait up to ~1 s for the reply `opcode`, routing pushes
    /// to `pending` and ignoring unrelated frames.
    fn write_read(&mut self, req: &[u8; REPORT_LEN], opcode: u8) -> Result<Frame> {
        self.send_frame(req)?;
        for _ in 0..READ_RETRIES {
            if let Some(frame) = self.read_one(READ_TIMEOUT_MS)? {
                if is_reply(&frame, opcode) {
                    return Ok(frame);
                }
            }
        }
        Err(anyhow!("timeout waiting for opcode {opcode:#04x}"))
    }

    /// Fetch the keymap prefix sized by `keymap_span` in MAX_CHUNK reads.
    /// `map` is replaced only when every chunk arrives in order, so a failed
    /// refetch keeps the last good keymap.
    fn fetch_keymap(&mut self, info: &Info) -> Result<()> {
        let (layers, total) = keymap_span(self.max_layer_inclusive, info);
        let mut chunks: Vec<(u16, Vec<u8>)> = Vec::new();
        for (offset, len) in chunk_requests(total) {
            let req = get_keymap_req(offset, len);
            let mut chunk = None;
            for _ in 0..CHUNK_ATTEMPTS {
                if let Ok(Frame::Keymap {
                    offset: chunk_offset,
                    data,
                }) = self.write_read(&req, OPC_KEYMAP)
                {
                    if chunk_offset == offset {
                        chunk = Some((chunk_offset, data));
                        break;
                    }
                }
            }
            match chunk {
                Some(chunk) => {
                    chunks.push(chunk);
                }
                None => {
                    return Err(anyhow!("keymap chunk at offset {offset} failed"));
                }
            }
        }
        let bytes = join_chunks(&chunks, total as usize)
            .ok_or_else(|| anyhow!("keymap chunks incomplete or out of order"))?;
        self.map = fill_keymap(&bytes, layers, info.rows, info.cols);
        Ok(())
    }

    /// Pump I/O: keepalive when connected, reconnect (or re-probe an
    /// unsupported board) with backoff when not.
    pub fn pump(&mut self) -> Result<()> {
        if self.device.is_none() {
            if Instant::now() < self.next_retry {
                return Ok(());
            }
            match self.try_connect() {
                Ok(ProbeOutcome::Supported(_)) => {}
                Ok(ProbeOutcome::Unsupported) | Err(_) => {
                    self.next_retry = Instant::now() + RECONNECT_BACKOFF;
                }
            }
            return Ok(());
        }

        if self.last_keepalive.elapsed() >= KEEPALIVE {
            if let Err(e) = self.send_frame(&subscribe_req()) {
                self.drop_device();
                return Err(e);
            }
            self.last_keepalive = Instant::now();
        }

        Ok(())
    }

    fn drop_device(&mut self) {
        self.device = None;
        self.next_retry = Instant::now() + RECONNECT_BACKOFF;
    }
}

impl BoardClient for OmakeysBoardClient {
    fn poll_events(&mut self) -> Vec<HostEvent> {
        if let Err(e) = self.pump() {
            eprintln!("omakeys: board pump: {e}");
            self.drop_device();
            return Vec::new();
        }
        self.read_available();
        std::mem::take(&mut self.pending)
    }

    fn keymap(&self) -> &Keymap {
        &self.map
    }

    fn ok(&self) -> bool {
        self.device.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_req_exact_bytes() {
        let buf = hello_req();
        assert_eq!(buf.len(), REPORT_LEN);
        assert_eq!(buf[0], MAGIC);
        assert_eq!(buf[1], OPC_HELLO);
        assert!(buf[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn get_state_req_exact_bytes() {
        let buf = get_state_req();
        assert_eq!(buf.len(), REPORT_LEN);
        assert_eq!(buf[0], MAGIC);
        assert_eq!(buf[1], OPC_GET_STATE);
        assert!(buf[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn subscribe_req_exact_bytes() {
        let buf = subscribe_req();
        assert_eq!(buf.len(), REPORT_LEN);
        assert_eq!(buf[0], MAGIC);
        assert_eq!(buf[1], OPC_SUBSCRIBE);
        assert!(buf[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn unsubscribe_req_exact_bytes() {
        let buf = unsubscribe_req();
        assert_eq!(buf.len(), REPORT_LEN);
        assert_eq!(buf[0], MAGIC);
        assert_eq!(buf[1], OPC_UNSUBSCRIBE);
        assert!(buf[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn get_keymap_req_exact_bytes() {
        let buf = get_keymap_req(702, 18);
        assert_eq!(buf.len(), REPORT_LEN);
        assert_eq!(buf[0], MAGIC);
        assert_eq!(buf[1], OPC_GET_KEYMAP);
        assert_eq!(&buf[2..5], &[0xBE, 0x02, 18]);
        assert!(buf[5..].iter().all(|&b| b == 0));
    }

    #[test]
    fn info_frame_fields() {
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = MAGIC;
        buf[1] = OPC_INFO;
        buf[2] = 1;
        buf[3] = 0;
        buf[4] = 4;
        buf[5] = 12;
        buf[6] = 14;
        buf[7] = 1;
        buf[8] = 0x60;
        buf[9] = 0x04;
        match classify_frame(&buf) {
            Frame::Info(info) => {
                assert_eq!(info.ver_major, 1);
                assert_eq!(info.ver_minor, 0);
                assert_eq!(info.rows, 4);
                assert_eq!(info.cols, 12);
                assert_eq!(info.layers, 14);
                assert_eq!(info.flags, 1);
                assert_eq!(info.keymap_bytes, 0x0460);
                assert!(info.keymap_available());
            }
            other => panic!("expected INFO frame, got {other:?}"),
        }
    }

    #[test]
    fn info_keymap_available_flag() {
        let info = Info {
            ver_major: 1,
            ver_minor: 0,
            rows: 4,
            cols: 12,
            layers: 14,
            flags: 0,
            keymap_bytes: 0,
        };
        assert!(!info.keymap_available());
    }

    #[test]
    fn keymap_fetchable_requires_flag_layers_and_bytes() {
        let on = info_fixture(14, 1120);
        assert!(on.keymap_fetchable());
        for off in [
            Info {
                flags: 0,
                ..on.clone()
            },
            Info {
                layers: 0,
                ..on.clone()
            },
            Info {
                keymap_bytes: 0,
                ..on.clone()
            },
        ] {
            assert!(!off.keymap_fetchable());
        }
    }

    #[test]
    fn probe_outcome_accepts_only_supported_major_1() {
        let good = info_fixture(14, 1120);
        assert_eq!(
            probe_outcome(&good).expect("sane INFO supported"),
            ProbeOutcome::Supported(good.clone())
        );

        let future = Info {
            ver_major: 2,
            ..good.clone()
        };
        assert_eq!(
            probe_outcome(&future).expect("unsupported major stays offline"),
            ProbeOutcome::Unsupported
        );

        for malformed in [
            Info {
                rows: 0,
                ..good.clone()
            },
            Info {
                cols: 0,
                ..good.clone()
            },
        ] {
            assert!(probe_outcome(&malformed).is_err());
        }
    }

    #[test]
    fn probe_outcome_accepts_keymap_off_info() {
        let off = Info {
            ver_major: 1,
            ver_minor: 0,
            rows: 4,
            cols: 10,
            layers: 0,
            flags: 0,
            keymap_bytes: 0,
        };
        assert_eq!(
            probe_outcome(&off).expect("keymap-off INFO still connects"),
            ProbeOutcome::Supported(off)
        );
    }

    #[test]
    fn state_frame_layer_state_le() {
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = MAGIC;
        buf[1] = OPC_STATE;
        buf[2] = 1;
        buf[3] = 0;
        buf[4] = 0x00;
        buf[5] = 0x00;
        buf[6] = 0x00;
        buf[7] = 0x04;
        match classify_frame(&buf) {
            Frame::State {
                default_layer,
                layer_state,
            } => {
                assert_eq!(default_layer, 1);
                assert_eq!(layer_state, 0x0400_0000);
            }
            other => panic!("expected STATE frame, got {other:?}"),
        }
    }

    #[test]
    fn layer_push_same_shape_as_state() {
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = MAGIC;
        buf[1] = OPC_LAYER;
        buf[2] = 0;
        buf[3] = 0;
        buf[4] = 0x40;
        assert_eq!(
            classify_frame(&buf),
            Frame::State {
                default_layer: 0,
                layer_state: 0x40
            }
        );
    }

    #[test]
    fn key_frame_validation() {
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = MAGIC;
        buf[1] = OPC_KEY;
        buf[2] = 0;
        buf[3] = 0;
        buf[4] = 1;
        assert_eq!(
            classify_frame(&buf),
            Frame::Key {
                row: 0,
                col: 0,
                pressed: true
            }
        );
        buf[2] = 16;
        assert_eq!(classify_frame(&buf), Frame::Other);
        buf[2] = 0;
        buf[3] = 16;
        assert_eq!(classify_frame(&buf), Frame::Other);
        buf[3] = 0;
        buf[4] = 2;
        assert_eq!(classify_frame(&buf), Frame::Other);
        buf[4] = 1;
        assert_eq!(classify_frame(&buf[..4]), Frame::Other);
    }

    #[test]
    fn keymap_frame_len_18_short_chunk() {
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = MAGIC;
        buf[1] = OPC_KEYMAP;
        buf[2] = 0xBE;
        buf[3] = 0x02;
        buf[4] = 18;
        for (i, b) in buf[5..23].iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        match classify_frame(&buf) {
            Frame::Keymap { offset, data } => {
                assert_eq!(offset, 702);
                assert_eq!(data, (1u8..=18).collect::<Vec<_>>());
            }
            other => panic!("expected KEYMAP frame, got {other:?}"),
        }
    }

    #[test]
    fn keymap_frame_rejects_short_and_oversized_data() {
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = MAGIC;
        buf[1] = OPC_KEYMAP;
        buf[4] = 18;
        assert_eq!(classify_frame(&buf[..5 + 10]), Frame::Other);
        buf[4] = MAX_CHUNK + 1;
        assert_eq!(classify_frame(&buf), Frame::Other);
    }

    #[test]
    fn foreign_and_malformed_frames_are_other() {
        let mut via = [0u8; REPORT_LEN];
        via[0] = 0x04;
        let mut keypeek = [0u8; REPORT_LEN];
        keypeek[0] = 0xC0;
        keypeek[1] = 0xA1;
        let mut layerlens = [0u8; REPORT_LEN];
        layerlens[0] = 0xF1;
        layerlens[1] = 0x00;
        let mut keypeek_layer = [0u8; REPORT_LEN];
        keypeek_layer[0] = 0xFF;
        keypeek_layer[1] = 4;
        let mut wrong_magic = [0u8; REPORT_LEN];
        wrong_magic[0] = 0x4E;
        wrong_magic[1] = OPC_INFO;
        let mut unknown_op = [0u8; REPORT_LEN];
        unknown_op[0] = MAGIC;
        unknown_op[1] = 0x99;
        assert_eq!(classify_frame(&via), Frame::Other);
        assert_eq!(classify_frame(&keypeek), Frame::Other);
        assert_eq!(classify_frame(&layerlens), Frame::Other);
        assert_eq!(classify_frame(&keypeek_layer), Frame::Other);
        assert_eq!(classify_frame(&wrong_magic), Frame::Other);
        assert_eq!(classify_frame(&unknown_op), Frame::Other);
        assert_eq!(classify_frame(&[]), Frame::Other);
        assert_eq!(classify_frame(&[MAGIC, OPC_STATE]), Frame::Other);
    }

    #[test]
    fn state_event_maps_fields() {
        assert_eq!(
            state_event(1, 0b100),
            HostEvent::Layer {
                default_layer: 1,
                layer_state: 0b100
            }
        );
    }

    fn info_fixture(layers: u8, keymap_bytes: u16) -> Info {
        Info {
            ver_major: 1,
            ver_minor: 0,
            rows: 4,
            cols: 10,
            layers,
            flags: 1,
            keymap_bytes,
        }
    }

    #[test]
    fn chunk_requests_720_is_27_chunks_last_18() {
        let chunks = chunk_requests(720);
        assert_eq!(chunks.len(), 27);
        assert_eq!(chunks[0], (0, 27));
        assert_eq!(chunks.last().copied(), Some((702, 18)));
        let mut expected_offset = 0u16;
        for &(offset, len) in &chunks {
            assert_eq!(offset, expected_offset);
            assert!((1..=MAX_CHUNK).contains(&len));
            expected_offset += u16::from(len);
        }
        assert_eq!(expected_offset, 720);
    }

    #[test]
    fn chunk_requests_zero_is_empty() {
        assert!(chunk_requests(0).is_empty());
    }

    fn chunked_stream(total: u16) -> Vec<(u16, Vec<u8>)> {
        chunk_requests(total)
            .into_iter()
            .map(|(offset, len)| (offset, vec![0u8; len as usize]))
            .collect()
    }

    #[test]
    fn join_chunks_happy_path_720() {
        let joined = join_chunks(&chunked_stream(720), 720).expect("full stream joins");
        assert_eq!(joined.len(), 720);
    }

    #[test]
    fn join_chunks_mismatched_offset_is_none() {
        let mut chunks = chunked_stream(720);
        chunks[5].0 += 1;
        assert_eq!(join_chunks(&chunks, 720), None);
    }

    #[test]
    fn join_chunks_short_stream_is_none() {
        let mut chunks = chunked_stream(720);
        chunks.pop();
        assert_eq!(join_chunks(&chunks, 720), None);
    }

    #[test]
    fn keymap_span_clamps_layers_and_bytes() {
        assert_eq!(keymap_span(8, &info_fixture(14, 1120)), (9, 720));
        assert_eq!(keymap_span(8, &info_fixture(14, 720)), (9, 720));
        assert_eq!(keymap_span(8, &info_fixture(5, 1120)), (5, 400));
        assert_eq!(keymap_span(13, &info_fixture(14, 1120)), (14, 1120));
    }

    #[test]
    fn fill_keymap_be_round_trip() {
        let (layers, rows, cols) = (2u8, 2u8, 3u8);
        let cell = |layer: u8, row: u8, col: u8| {
            0x1000 + layer as u16 * 100 + row as u16 * 10 + col as u16
        };
        let mut bytes = Vec::new();
        for layer in 0..layers {
            for row in 0..rows {
                for col in 0..cols {
                    bytes.extend_from_slice(&cell(layer, row, col).to_be_bytes());
                }
            }
        }
        let map = fill_keymap(&bytes, layers, rows, cols);
        for layer in 0..layers {
            for row in 0..rows {
                for col in 0..cols {
                    assert_eq!(map.get(layer, row, col), cell(layer, row, col));
                }
            }
        }
    }

    #[test]
    fn fill_keymap_real_rsta_bytes_are_big_endian() {
        let bytes = [0x21, 0x14, 0x00, 0x1A, 0x00, 0x09];
        let map = fill_keymap(&bytes, 1, 1, 3);
        assert_eq!(map.get(0, 0, 0), 0x2114);
        assert_eq!(map.get(0, 0, 1), 0x001A);
        assert_eq!(map.get(0, 0, 2), 0x0009);
    }

    #[test]
    fn fill_keymap_missing_bytes_stay_zero() {
        let bytes = [0x12, 0x34, 0x78];
        let map = fill_keymap(&bytes, 1, 2, 2);
        assert_eq!(map.get(0, 0, 0), 0x1234);
        assert_eq!(map.get(0, 0, 1), 0);
        assert_eq!(map.get(0, 1, 0), 0);
        assert_eq!(map.get(0, 1, 1), 0);
    }

    #[test]
    fn fill_keymap_ignores_leftover_bytes() {
        let bytes = [0x12, 0x34, 0xAA, 0xBB];
        let map = fill_keymap(&bytes, 1, 1, 1);
        assert_eq!(map.get(0, 0, 0), 0x1234);
    }

    #[test]
    fn open_succeeds_with_live_board_or_soft_connect() {
        let profile = crate::board::profile::loremipsum36();
        let client = OmakeysBoardClient::open(&profile);
        assert!(client.is_ok());
    }
}
