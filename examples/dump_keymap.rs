//! Dump the connected board's keymap as JSON (layers x rows x cols).
use omakeys::board::client::BoardClient;
use omakeys::board::{client::HidBoardClient, loremipsum36};
use std::time::{Duration, Instant};

fn main() {
    let profile = loremipsum36();
    let mut c = HidBoardClient::open(&profile).expect("open");
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(6) {
        let _ = c.poll_events();
        if c.ok()
            && c.keymap()
                .layers
                .iter()
                .flatten()
                .flatten()
                .any(|k| *k != 0)
        {
            std::thread::sleep(Duration::from_millis(500));
            let _ = c.poll_events();
            println!("{}", serde_json::to_string(&c.keymap().layers).unwrap());
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("no keymap");
    std::process::exit(1);
}
