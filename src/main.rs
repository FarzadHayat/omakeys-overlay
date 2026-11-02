use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand};
use gtk::glib;
use gtk::prelude::*;
use gtk::Application;

use omakeys::board::client::Keymap;
use omakeys::board::HostEvent;
use omakeys::board::{
    connected_raw_hid, load_profiles, open_board, select_profile, BoardClient,
};
use omakeys::core::boards::BoardRouter;
use omakeys::board::BoardProfile;
use omakeys::core::{
    fetch_binds, fetch_binds_with_retry, load_config, load_overrides, save_config, AppConfig,
    Bind, OverlayCore,
};
use omakeys::doctor;
use omakeys::ipc::{self, IpcCommand};
use omakeys::ui::{
    apply_theme_icons, load_omarchy_theme, watch_dirs, watch_omarchy_theme, OverlayWindow,
};

#[derive(Parser)]
#[command(name = "omakeys", about = "Omakeys live layer map")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the overlay daemon (default)
    Run,
    /// Toggle overlay visibility via IPC
    Toggle,
    /// Open Omarchy bar settings panel
    Settings,
    /// Print config.toml as JSON
    ConfigPrint,
    /// Write config from JSON, then reload daemon if running
    ConfigApply { json: String },
    /// List board profiles as JSON (`[{value,label},…]`) for the settings panel
    Boards,
    /// Check the setup: desktop, board, permissions, daemon
    Doctor,
    /// Print the udev rule for every known board
    UdevRules,
    /// Print the code of each key you press, on every keyboard
    Keys,
    /// Check daemon socket (expects ok)
    Ping,
    /// Quit the running daemon
    Quit,
}

enum AppMsg {
    /// Events and keymaps carry the name of the board they came from.
    Events(String, Vec<HostEvent>),
    Keymap(String, Keymap),
    /// Fresh profile list (new `boards/*.toml` picked up by the board loop).
    Profiles(Vec<BoardProfile>),
    Binds(Vec<Bind>),
    Ipc(IpcCommand),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Command::Run) {
        Command::Run => run_daemon(),
        Command::Toggle => ipc::send_command(IpcCommand::Toggle),
        Command::Settings => {
            let status = std::process::Command::new("omarchy-shell")
                .args(["-q", "omakeys", "toggle"])
                .status()
                .context("omarchy-shell omakeys toggle")?;
            if !status.success() {
                anyhow::bail!("failed to toggle omakeys panel (is Omarchy shell running?)");
            }
            Ok(())
        }
        Command::ConfigPrint => {
            let cfg = load_config();
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        Command::ConfigApply { json } => {
            let cfg: AppConfig = serde_json::from_str::<AppConfig>(&json)?.normalized();
            save_config(&cfg)?;
            let _ = ipc::send_command(IpcCommand::Reload);
            Ok(())
        }
        Command::Boards => {
            let mut rows = vec![serde_json::json!({
                "value": "auto",
                "label": "Auto (follow typing)"
            })];
            for p in load_profiles() {
                let label = match p.name.as_str() {
                    "loremipsum36" => "Omakeys".into(),
                    "generic" => "Generic".into(),
                    "samsung-trio-500" => "Samsung Trio 500".into(),
                    other => other.to_string(),
                };
                rows.push(serde_json::json!({ "value": p.name, "label": label }));
            }
            println!("{}", serde_json::to_string(&rows)?);
            Ok(())
        }
        Command::Doctor => {
            if !doctor::run() {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Keys => omakeys::board::evdev::watch_codes(),
        Command::UdevRules => {
            print!("{}", doctor::udev_rules(&load_profiles()));
            Ok(())
        }
        Command::Ping => ipc::send_command(IpcCommand::Ping),
        Command::Quit => ipc::send_command(IpcCommand::Quit),
    }
}

fn run_daemon() -> anyhow::Result<()> {
    // Cursor dodge and Super bind layers both need the Hyprland session env.
    omakeys::core::ensure_hyprland_signature();
    let app = Application::builder()
        .application_id("org.omakeys.app")
        .build();

    app.connect_activate(|app| {
        let config = load_config();
        let profiles = load_profiles();
        let profile = select_profile(profiles.clone(), &config.board, &connected_raw_hid());
        let overrides = load_overrides();
        let core = Rc::new(RefCell::new(OverlayCore::new(
            profile.clone(),
            config.clone(),
            overrides,
        )));
        let router = Rc::new(RefCell::new(BoardRouter::new(profiles.clone(), &profile.name)));
        let force_hidden = core.borrow().force_hidden_handle();
        let overlay = OverlayWindow::new(app, config.hover.mode, Rc::clone(&core));
        apply_theme(&overlay);

        let (tx, rx) = mpsc::channel::<AppMsg>();
        let tx_board = tx.clone();
        thread::spawn(move || board_loop(profiles, tx_board));

        let tx_binds = tx.clone();
        thread::spawn(move || {
            // Hypr/omarchy may lag the systemd unit at login.
            if let Some(binds) = fetch_binds_with_retry(15, Duration::from_secs(2)) {
                let _ = tx_binds.send(AppMsg::Binds(binds));
            }
        });

        let (ipc_tx, ipc_rx) = mpsc::channel::<IpcCommand>();
        if let Err(e) = ipc::spawn_server(ipc_tx, force_hidden) {
            eprintln!("omakeys: IPC server: {e}");
        }

        let tx_ipc = tx.clone();
        thread::spawn(move || {
            while let Ok(cmd) = ipc_rx.recv() {
                let _ = tx_ipc.send(AppMsg::Ipc(cmd));
            }
        });

        let overlay = Rc::new(overlay);
        let rx = Rc::new(RefCell::new(rx));
        let app_rc = app.clone();
        let config_cell = Rc::new(RefCell::new(config));

        let watcher = match watch_omarchy_theme({
            let overlay = overlay.clone();
            move || apply_theme(&overlay)
        }) {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                eprintln!("omakeys: theme watch: {e}");
                None
            }
        };

        // Hyprland binds are read from config files, so refresh on edits.
        let binds_watcher = match watch_dirs(&bind_dirs(), |_, _| true, {
            let tx = tx.clone();
            move || {
                let tx = tx.clone();
                thread::spawn(move || {
                    if let Some(binds) = fetch_binds() {
                        let _ = tx.send(AppMsg::Binds(binds));
                    }
                });
            }
        }) {
            Ok(watchers) => Some(watchers),
            Err(e) => {
                eprintln!("omakeys: bind watch: {e}");
                None
            }
        };

        glib::timeout_add_local(Duration::from_millis(16), move || {
            let _ = (&watcher, &binds_watcher);
            let mut should_quit = false;
            {
                let rx = rx.borrow();
                loop {
                    match rx.try_recv() {
                        Ok(AppMsg::Events(board, events)) => {
                            let setting = config_cell.borrow().board.clone();
                            router.borrow_mut().handle(
                                &mut core.borrow_mut(),
                                &setting,
                                &board,
                                events,
                                Instant::now(),
                            );
                        }
                        Ok(AppMsg::Keymap(board, map)) => {
                            router.borrow_mut().set_keymap(&mut core.borrow_mut(), &board, map);
                        }
                        Ok(AppMsg::Profiles(profiles)) => {
                            let setting = config_cell.borrow().board.clone();
                            let mut r = router.borrow_mut();
                            r.merge_profiles(profiles);
                            r.follow_setting(&mut core.borrow_mut(), &setting);
                        }
                        Ok(AppMsg::Binds(binds)) => core.borrow_mut().set_binds(binds),
                        Ok(AppMsg::Ipc(IpcCommand::Toggle)) => {
                            // `force_hidden` already flipped in the IPC thread.
                        }
                        Ok(AppMsg::Ipc(IpcCommand::Settings)) => {
                            // GTK settings removed; panel is the Omarchy bar widget.
                        }
                        Ok(AppMsg::Ipc(IpcCommand::Reload)) => {
                            let cfg = load_config();
                            let ovr = load_overrides();
                            let profiles = load_profiles();
                            let mut c = core.borrow_mut();
                            c.set_config(cfg.clone());
                            c.set_overrides(ovr);
                            let mut r = router.borrow_mut();
                            r.merge_profiles(profiles);
                            r.follow_setting(&mut c, &cfg.board);
                            *config_cell.borrow_mut() = cfg.clone();
                            overlay.set_hover_mode(cfg.hover.mode);
                            apply_theme(&overlay);
                        }
                        Ok(AppMsg::Ipc(IpcCommand::Quit)) => {
                            should_quit = true;
                        }
                        Ok(AppMsg::Ipc(IpcCommand::Ping)) => {
                            // Handled in the IPC server thread (reply ok).
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break,
                    }
                }
            }

            if should_quit {
                app_rc.quit();
                return glib::ControlFlow::Break;
            }

            let now = Instant::now();
            core.borrow_mut().tick(now);
            if overlay.pointer_inside() {
                core.borrow_mut().note_activity(now);
            }
            overlay.tick_dodge(now);
            let frame = core.borrow().frame();
            overlay.apply_frame(&frame);
            glib::ControlFlow::Continue
        });
    });

    // Clap already consumed argv ("run", etc.). Passing process args into GTK
    // makes GIO try to OPEN them as files → CRITICAL "can not open files".
    app.run_with_args(&[] as &[&str]);
    Ok(())
}

fn apply_theme(overlay: &OverlayWindow) {
    let theme = load_omarchy_theme();
    if let Err(e) = apply_theme_icons(&theme) {
        eprintln!("omakeys: theme icons: {e}");
    }
    overlay.set_theme(theme);
}

fn bind_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(cfg) = dirs::config_dir() {
        dirs.push(cfg.join("hypr"));
    }
    dirs.push(PathBuf::from("/usr/share/omarchy/default/hypr"));
    dirs
}

fn board_loop(mut profiles: Vec<BoardProfile>, tx: mpsc::Sender<AppMsg>) {
    let mut boards: Vec<(String, Box<dyn BoardClient>, bool)> = open_all_boards(&profiles);
    if boards.is_empty() {
        eprintln!("omakeys: no board could start");
        return;
    }
    let mut next_reload = Instant::now() + Duration::from_secs(2);
    loop {
        if Instant::now() >= next_reload {
            next_reload = Instant::now() + Duration::from_secs(2);
            let fresh = load_profiles();
            let mut changed = false;
            for p in &fresh {
                if boards.iter().any(|(n, _, _)| n == &p.name) {
                    continue;
                }
                match open_board(p, &fresh) {
                    Ok(client) => {
                        eprintln!("omakeys: opened new board profile {}", p.name);
                        boards.push((p.name.clone(), client, false));
                        changed = true;
                    }
                    Err(e) => eprintln!("omakeys: board {}: {e:#}", p.name),
                }
            }
            if changed || fresh.len() != profiles.len() {
                let _ = tx.send(AppMsg::Profiles(fresh.clone()));
            }
            profiles = fresh;
        }
        for (name, client, was_ok) in &mut boards {
            let events = client.poll_events();
            if !events.is_empty() {
                let _ = tx.send(AppMsg::Events(name.clone(), events));
            }
            let ok = client.ok();
            if ok && !*was_ok {
                let _ = tx.send(AppMsg::Keymap(name.clone(), client.keymap().clone()));
            }
            *was_ok = ok;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn open_all_boards(profiles: &[BoardProfile]) -> Vec<(String, Box<dyn BoardClient>, bool)> {
    profiles
        .iter()
        .filter_map(|p| match open_board(p, profiles) {
            Ok(client) => Some((p.name.clone(), client, false)),
            Err(e) => {
                eprintln!("omakeys: board {}: {e:#}", p.name);
                None
            }
        })
        .collect()
}
