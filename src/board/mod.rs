pub mod client;
pub mod evdev;
pub mod labels;
pub mod omakeys;
pub mod profile;
pub mod protocol;

pub use labels::{legend_for_keycode, KeyLegend, LabelStyle, OMARCHY_SUPER};

pub use client::{BoardClient, HidBoardClient, Keymap, MockBoard};
pub use evdev::EvdevBoardClient;
pub use omakeys::OmakeysBoardClient;
pub use profile::{
    generic, load_profiles, loremipsum36, samsung_trio_500, select_profile, BoardProfile,
    BoardSource, KeyPos,
};
pub use protocol::{
    keypeek_subscribe_active, layerlens_get_layer_state_req, parse_host_event,
    parse_layerlens_poll_reply, via_get_buffer_req, via_get_keycode_req, via_get_layer_count_req,
    via_parse_keycode, HostEvent, REPORT_LEN,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_typechecks() {
        let _ = open_board
            as fn(&BoardProfile, &[BoardProfile]) -> anyhow::Result<Box<dyn BoardClient>>;
    }
}

/// Open the preferred board client: the omakeys protocol when the board
/// is absent (soft-connect) or answers HELLO, else the legacy HID client.
pub fn open_board(
    profile: &BoardProfile,
    all: &[BoardProfile],
) -> anyhow::Result<Box<dyn BoardClient>> {
    if profile.source == BoardSource::Evdev {
        let claim = evdev::Claim {
            name: profile.evdev_match.map(str::to_string),
            others: all
                .iter()
                .filter(|p| p.source == BoardSource::Evdev && p.name != profile.name)
                .filter_map(|p| p.evdev_match.map(str::to_string))
                .collect(),
            skip_ids: all
                .iter()
                .filter(|p| p.source == BoardSource::RawHid)
                .map(|p| (p.vid, p.pid))
                .collect(),
        };
        return Ok(Box::new(EvdevBoardClient::new(&profile.name, claim)));
    }
    match OmakeysBoardClient::open(profile) {
        Ok(client) => Ok(Box::new(client)),
        Err(_) => Ok(Box::new(HidBoardClient::open(profile)?)),
    }
}

/// vid:pid of every Raw HID (usage page FF60) device currently plugged in.
pub fn connected_raw_hid() -> Vec<(u16, u16)> {
    let Ok(api) = hidapi::HidApi::new() else {
        return Vec::new();
    };
    api.device_list()
        .filter(|d| d.usage_page() == client::USAGE_PAGE && d.usage() == client::USAGE)
        .map(|d| (d.vendor_id(), d.product_id()))
        .collect()
}
