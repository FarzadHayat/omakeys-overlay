//! KeyPeek push + VIA dynamic-keymap Raw HID codecs (32-byte reports).

pub const REPORT_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    Layer {
        default_layer: u32,
        layer_state: u32,
    },
    Key {
        row: u8,
        col: u8,
        pressed: bool,
    },
}

pub fn keypeek_subscribe_active() -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = 0xC0;
    buf[1] = 0xA1;
    buf
}

/// Parse a KeyPeek (or ignore LayerLens) inbound report.
pub fn parse_host_event(buf: &[u8]) -> Option<HostEvent> {
    if buf.len() < 4 {
        return None;
    }
    match buf[0] {
        0xFF => {
            let size = buf[1] as usize;
            if size != 4 || buf.len() < 2 + size * 2 {
                return None;
            }
            let default_layer = u32::from_le_bytes(buf[2..6].try_into().ok()?);
            let layer_state = u32::from_le_bytes(buf[6..10].try_into().ok()?);
            Some(HostEvent::Layer {
                default_layer,
                layer_state,
            })
        }
        0xF1 => {
            // KeyPeek key: [0xF1, row, col, pressed∈{0,1}, zeros…].
            // LayerLens poll reply: [0xF1, 0x00, version=0x02, state_be…].
            // Ambiguous: KeyPeek (row=0,col=2) shares the first three bytes with a
            // LayerLens poll. Prefer KeyPeek when pressed≤1 and trailing zeros —
            // that is how F (Colemak top-row) is reported. LayerLens with any
            // non-zero state fails the trailing-zero check; sync polls are claimed
            // in write_read via parse_layerlens_poll_reply before this path.
            let row = buf[1];
            let col = buf[2];
            let pressed_byte = buf[3];
            if row > 15 || col > 15 || pressed_byte > 1 {
                return None;
            }
            if !buf[4..].iter().all(|&b| b == 0) {
                return None;
            }
            Some(HostEvent::Key {
                row,
                col,
                pressed: pressed_byte != 0,
            })
        }
        _ => None,
    }
}

/// LayerLens poll: GET_LAYER_STATE (works with current rsta firmware today).
pub fn layerlens_get_layer_state_req() -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = 0xF1;
    buf[1] = 0x00;
    buf
}

/// LayerLens poll-reply version byte (see `LAYERLENS_NOTIFY_PROTOCOL_VERSION`).
pub const LAYERLENS_POLL_VERSION: u8 = 0x02;

/// Parse LayerLens poll reply → layer bitmask (big-endian).
///
/// Wire: `[0xF1, 0x00, version=0x02, state_be…]`. The version check is required:
/// without it every KeyPeek event on row 0 (`[0xF1, 0, col, …]`) looks like a
/// poll reply and gets swallowed while the host waits for LayerLens.
pub fn parse_layerlens_poll_reply(buf: &[u8]) -> Option<u32> {
    if buf.len() < 7
        || buf[0] != 0xF1
        || buf[1] != 0x00
        || buf[2] != LAYERLENS_POLL_VERSION
    {
        return None;
    }
    Some(u32::from_be_bytes([buf[3], buf[4], buf[5], buf[6]]))
}

/// True when a LayerLens-shaped poll buffer is also a valid KeyPeek for (0, 2).
/// Layer state 0 and KeyPeek F-release are byte-identical.
pub fn layerlens_poll_ambiguous_with_keypeek(buf: &[u8]) -> bool {
    parse_layerlens_poll_reply(buf).is_some()
        && buf[3] <= 1
        && buf[4..].iter().all(|&b| b == 0)
}

pub fn via_get_keycode_req(layer: u8, row: u8, col: u8) -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = 0x04;
    buf[1] = layer;
    buf[2] = row;
    buf[3] = col;
    buf
}

pub fn via_parse_keycode(resp: &[u8]) -> Option<u16> {
    if resp.len() < 6 || resp[0] != 0x04 {
        return None;
    }
    Some(u16::from_be_bytes([resp[4], resp[5]]))
}

pub fn via_get_layer_count_req() -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = 0x11;
    buf
}

pub fn via_get_buffer_req(offset: u16, size: u8) -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    buf[0] = 0x12;
    buf[1] = (offset >> 8) as u8;
    buf[2] = (offset & 0xFF) as u8;
    buf[3] = size;
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_active_bytes() {
        let s = keypeek_subscribe_active();
        assert_eq!(s[0], 0xC0);
        assert_eq!(s[1], 0xA1);
        assert!(s[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn parse_keypeek_layer_le() {
        let mut buf = [0u8; 32];
        buf[0] = 0xFF;
        buf[1] = 4;
        buf[2..6].copy_from_slice(&1u32.to_le_bytes());
        buf[6..10].copy_from_slice(&0b100u32.to_le_bytes());
        match parse_host_event(&buf).unwrap() {
            HostEvent::Layer {
                default_layer,
                layer_state,
            } => {
                assert_eq!(default_layer, 1);
                assert_eq!(layer_state, 0b100);
            }
            _ => panic!("expected layer"),
        }
    }

    #[test]
    fn parse_keypeek_key() {
        let mut buf = [0u8; 32];
        buf[0] = 0xF1;
        buf[1] = 1;
        buf[2] = 2;
        buf[3] = 1;
        match parse_host_event(&buf).unwrap() {
            HostEvent::Key { row, col, pressed } => {
                assert_eq!((row, col, pressed), (1, 2, true));
            }
            _ => panic!("expected key"),
        }
    }

    #[test]
    fn ignore_layerlens_poll_reply_with_state() {
        // Non-zero layer state → not KeyPeek (trailing bytes set).
        let mut buf = [0u8; 32];
        buf[0] = 0xF1;
        buf[1] = 0x00;
        buf[2] = 0x02;
        buf[6] = 0x04; // layer bit 2 in BE state
        assert!(parse_host_event(&buf).is_none());
    }

    #[test]
    fn keypeek_row0_col2_is_f_not_layerlens() {
        // Colemak F sits at (0,2). Must not be swallowed as LayerLens poll.
        let mut buf = [0u8; 32];
        buf[0] = 0xF1;
        buf[1] = 0;
        buf[2] = 2;
        buf[3] = 1;
        match parse_host_event(&buf).unwrap() {
            HostEvent::Key { row, col, pressed } => {
                assert_eq!((row, col, pressed), (0, 2, true));
            }
            _ => panic!("expected key"),
        }
        buf[3] = 0;
        match parse_host_event(&buf).unwrap() {
            HostEvent::Key { row, col, pressed } => {
                assert_eq!((row, col, pressed), (0, 2, false));
            }
            _ => panic!("expected key"),
        }
    }

    #[test]
    fn parse_via_keycode_be() {
        let mut buf = [0u8; 32];
        buf[0] = 0x04;
        buf[4] = 0x00;
        buf[5] = 0x04;
        assert_eq!(via_parse_keycode(&buf), Some(0x0004));
    }

    #[test]
    fn parse_layerlens_poll_be() {
        let mut buf = [0u8; 32];
        buf[0] = 0xF1;
        buf[1] = 0x00;
        buf[2] = 0x02;
        // layer bit 2 → 0x00000004 BE
        buf[3] = 0;
        buf[4] = 0;
        buf[5] = 0;
        buf[6] = 0x04;
        assert_eq!(parse_layerlens_poll_reply(&buf), Some(0b100));
    }

    #[test]
    fn layerlens_poll_ignores_row0_keypeek_other_cols() {
        // KeyPeek W release (0,1) must NOT parse as LayerLens.
        let mut buf = [0u8; 32];
        buf[0] = 0xF1;
        buf[1] = 0;
        buf[2] = 1; // col, not version
        buf[3] = 0;
        assert_eq!(parse_layerlens_poll_reply(&buf), None);
    }

    #[test]
    fn layerlens_poll_ambiguous_f_release() {
        let mut buf = [0u8; 32];
        buf[0] = 0xF1;
        buf[1] = 0;
        buf[2] = 2;
        buf[3] = 0;
        assert!(layerlens_poll_ambiguous_with_keypeek(&buf));
        assert_eq!(parse_layerlens_poll_reply(&buf), Some(0));
    }
}
