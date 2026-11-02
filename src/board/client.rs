//! Raw HID board session: VIA keymap fetch + KeyPeek subscribe/events.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use hidapi::{HidApi, HidDevice};

use super::profile::BoardProfile;
use super::protocol::{
    keypeek_subscribe_active, layerlens_get_layer_state_req, layerlens_poll_ambiguous_with_keypeek,
    parse_host_event, parse_layerlens_poll_reply, via_get_keycode_req, via_parse_keycode,
    HostEvent, REPORT_LEN,
};

pub const USAGE_PAGE: u16 = 0xFF60;
pub const USAGE: u16 = 0x61;
const KEEPALIVE: Duration = Duration::from_millis(1500);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keymap {
    /// [layer][row][col]
    pub layers: Vec<Vec<Vec<u16>>>,
}

impl Keymap {
    pub fn empty(layers: usize, rows: usize, cols: usize) -> Self {
        Self {
            layers: vec![vec![vec![0u16; cols]; rows]; layers],
        }
    }

    pub fn get(&self, layer: u8, row: u8, col: u8) -> u16 {
        self.layers
            .get(layer as usize)
            .and_then(|r| r.get(row as usize))
            .and_then(|c| c.get(col as usize))
            .copied()
            .unwrap_or(0)
    }

    pub fn set(&mut self, layer: u8, row: u8, col: u8, kc: u16) {
        if let Some(cell) = self
            .layers
            .get_mut(layer as usize)
            .and_then(|r| r.get_mut(row as usize))
            .and_then(|c| c.get_mut(col as usize))
        {
            *cell = kc;
        }
    }

    /// True if any layer maps this matrix cell to a layer switch (LT/MO/TO/TG).
    pub fn position_can_change_layer(&self, row: u8, col: u8) -> bool {
        self.layers.iter().any(|layer| {
            layer
                .get(row as usize)
                .and_then(|r| r.get(col as usize))
                .copied()
                .is_some_and(keycode_can_change_layer)
        })
    }
}

/// QMK layer-tap / momentary / toggle / goto — holding or tapping these
/// changes `layer_state`. Plain mods and mod-tap letters do not.
fn keycode_can_change_layer(kc: u16) -> bool {
    matches!(kc, 0x4000..=0x4FFF | 0x5200..=0x529F)
}

pub trait BoardClient: Send {
    fn poll_events(&mut self) -> Vec<HostEvent>;
    fn keymap(&self) -> &Keymap;
    fn ok(&self) -> bool;
}

/// Test double used by OverlayCore unit tests.
pub struct MockBoard {
    pub events: Vec<HostEvent>,
    pub map: Keymap,
    pub ok: bool,
}

impl BoardClient for MockBoard {
    fn poll_events(&mut self) -> Vec<HostEvent> {
        std::mem::take(&mut self.events)
    }

    fn keymap(&self) -> &Keymap {
        &self.map
    }

    fn ok(&self) -> bool {
        self.ok
    }
}

pub struct HidBoardClient {
    api: HidApi,
    vid: u16,
    pid: u16,
    device: Option<HidDevice>,
    map: Keymap,
    max_layer: u8,
    keys: Vec<(u8, u8)>,
    last_keepalive: Instant,
    next_retry: Instant,
    /// Events read while waiting for a VIA/LayerLens reply.
    pending: Vec<HostEvent>,
}

impl HidBoardClient {
    pub fn open(profile: &BoardProfile) -> Result<Self> {
        let api = HidApi::new().context("hidapi init")?;
        let keys: Vec<(u8, u8)> = profile.keys.iter().map(|k| (k.row, k.col)).collect();
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
            max_layer: profile.max_layer_inclusive,
            keys,
            last_keepalive: Instant::now() - KEEPALIVE,
            next_retry: Instant::now(),
            pending: Vec::new(),
        };
        // Soft-connect: missing keyboard is OK; pump() retries.
        let _ = client.try_connect();
        Ok(client)
    }

    fn try_connect(&mut self) -> Result<()> {
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
        self.fetch_keymap()?;
        self.send_subscribe()?;
        self.last_keepalive = Instant::now();
        // Seed layer from LayerLens (reliable on current firmware).
        if let Ok(Some(state)) = self.query_layerlens_state() {
            self.pending.push(HostEvent::Layer {
                default_layer: 0,
                layer_state: state,
            });
        }
        Ok(())
    }

    fn send_subscribe(&mut self) -> Result<()> {
        let dev = self.device.as_ref().ok_or_else(|| anyhow!("no device"))?;
        let pkt = keypeek_subscribe_active();
        dev.write(&pkt).context("keypeek subscribe")?;
        Ok(())
    }

    fn fetch_keymap(&mut self) -> Result<()> {
        let layers = self.max_layer;
        let keys = self.keys.clone();
        for layer in 0..=layers {
            for &(row, col) in &keys {
                let req = via_get_keycode_req(layer, row, col);
                let resp = self.write_read(&req)?;
                let kc = via_parse_keycode(&resp).unwrap_or(0);
                self.map.set(layer, row, col, kc);
            }
        }
        Ok(())
    }

    fn write_read(&mut self, req: &[u8; REPORT_LEN]) -> Result<[u8; REPORT_LEN]> {
        let dev = self.device.as_ref().ok_or_else(|| anyhow!("no device"))?;
        dev.write(req).context("hid write")?;
        let mut buf = [0u8; REPORT_LEN];
        // VIA / LayerLens replies promptly; KeyPeek pushes may arrive first.
        for _ in 0..50 {
            match dev.read_timeout(&mut buf, 20) {
                Ok(n) if n > 0 => {
                    if req[0] == 0x04 && buf[0] == 0x04 {
                        return Ok(buf);
                    }
                    if req[0] == 0xF1 && req[1] == 0x00 {
                        if let Some(state) = parse_layerlens_poll_reply(&buf) {
                            // (0,2) KeyPeek ↔ LayerLens state=0 are byte-identical.
                            // Forward KeyPeek so a real F event during the wait is
                            // not lost, then accept state=0 as the LayerLens reply.
                            // Safe: we only poll LayerLens on layer-switch keys /
                            // keepalive, not on every letter — so fake F releases
                            // cannot re-trigger a poll loop.
                            if layerlens_poll_ambiguous_with_keypeek(&buf) {
                                if let Some(ev) = parse_host_event(&buf) {
                                    self.pending.push(ev);
                                }
                                if state == 0 {
                                    return Ok(buf);
                                }
                                continue;
                            }
                            return Ok(buf);
                        }
                    }
                    if let Some(ev) = parse_host_event(&buf) {
                        self.pending.push(ev);
                        continue;
                    }
                    // Unknown frame; return it if it looks like our request echo.
                    if buf[0] == req[0] {
                        return Ok(buf);
                    }
                }
                Ok(_) => {}
                Err(e) => return Err(e).context("hid read"),
            }
        }
        Err(anyhow!("timeout waiting for HID reply"))
    }

    /// Ask LayerLens for the live layer bitmask (already wired in rsta firmware).
    fn query_layerlens_state(&mut self) -> Result<Option<u32>> {
        let req = layerlens_get_layer_state_req();
        match self.write_read(&req) {
            Ok(resp) => Ok(parse_layerlens_poll_reply(&resp)),
            Err(_) => Ok(None),
        }
    }

    /// Pump I/O: keepalive, read events, reconnect on failure.
    pub fn pump(&mut self) -> Result<()> {
        if self.device.is_none() {
            if Instant::now() < self.next_retry {
                return Ok(());
            }
            match self.try_connect() {
                Ok(()) => {}
                Err(_) => {
                    self.next_retry = Instant::now() + RECONNECT_BACKOFF;
                }
            }
            return Ok(());
        }

        if self.last_keepalive.elapsed() >= KEEPALIVE {
            if let Err(e) = self.send_subscribe() {
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

    fn read_available(&mut self) -> Vec<HostEvent> {
        let mut out = Vec::new();
        let Some(dev) = self.device.as_ref() else {
            return out;
        };
        loop {
            let mut buf = [0u8; REPORT_LEN];
            match dev.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(ev) = parse_host_event(&buf) {
                        out.push(ev);
                    }
                }
                Err(_) => {
                    // Would block or error — treat disconnect on next pump write.
                    break;
                }
            }
        }
        out
    }
}

impl BoardClient for HidBoardClient {
    fn poll_events(&mut self) -> Vec<HostEvent> {
        if let Err(e) = self.pump() {
            eprintln!("omakeys: board pump: {e}");
            self.drop_device();
            return Vec::new();
        }
        let mut events = std::mem::take(&mut self.pending);
        events.extend(self.read_available());

        // Live layer updates: KeyPeek 0xFF pushes are unreliable on current
        // firmware, so poll LayerLens. Only do it for layer-switch keys
        // (MO/LT/TG/TO) or right after keepalive — polling on every key
        // raced with F (0,2), whose KeyPeek packet matches LayerLens state=0.
        let layer_key_activity = events.iter().any(|e| match e {
            HostEvent::Key { row, col, .. } => self.map.position_can_change_layer(*row, *col),
            _ => false,
        });
        let just_keepalive = self.last_keepalive.elapsed() < Duration::from_millis(50);
        if layer_key_activity || just_keepalive {
            if let Ok(Some(state)) = self.query_layerlens_state() {
                events.push(HostEvent::Layer {
                    default_layer: 0,
                    layer_state: state,
                });
            }
        }
        events
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
    use crate::board::protocol::HostEvent;

    #[test]
    fn position_can_change_layer_detects_mo_and_lt() {
        let mut map = Keymap::empty(4, 4, 10);
        map.set(0, 3, 6, 0x5223); // MO(3)
        map.set(0, 3, 3, 0x4200 | 0x29); // LT(2, Esc)
        map.set(0, 0, 0, 0x2114); // LCTL_T(Q) — not a layer switch
        assert!(map.position_can_change_layer(3, 6));
        assert!(map.position_can_change_layer(3, 3));
        assert!(!map.position_can_change_layer(0, 0));
        assert!(!map.position_can_change_layer(1, 1));
    }

    #[test]
    fn mock_board_poll_drains() {
        let mut m = MockBoard {
            events: vec![HostEvent::Key {
                row: 0,
                col: 1,
                pressed: true,
            }],
            map: Keymap::empty(1, 4, 10),
            ok: true,
        };
        assert_eq!(m.poll_events().len(), 1);
        assert!(m.poll_events().is_empty());
    }

    #[test]
    fn keymap_get_set() {
        let mut map = Keymap::empty(2, 4, 10);
        map.set(1, 2, 3, 0x0004);
        assert_eq!(map.get(1, 2, 3), 0x0004);
        assert_eq!(map.get(0, 0, 0), 0);
    }
}
