//! Local Unix socket IPC: `toggle`, `settings`, `reload`, `quit`, `ping`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcCommand {
    Toggle,
    Settings,
    Reload,
    Quit,
    Ping,
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("omakeys.sock")
}

pub fn parse_command(line: &str) -> Option<IpcCommand> {
    match line.trim() {
        "toggle" => Some(IpcCommand::Toggle),
        "settings" => Some(IpcCommand::Settings),
        "reload" => Some(IpcCommand::Reload),
        "quit" => Some(IpcCommand::Quit),
        "ping" => Some(IpcCommand::Ping),
        _ => None,
    }
}

/// Ping replies: `ok live` (overlay allowed) or `ok hidden` (toggled off).
pub fn ping_reply(force_hidden: bool) -> &'static str {
    if force_hidden {
        "ok hidden\n"
    } else {
        "ok live\n"
    }
}

pub fn send_command(cmd: IpcCommand) -> Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connect {}", path.display()))?;
    let line = match cmd {
        IpcCommand::Toggle => "toggle\n",
        IpcCommand::Settings => "settings\n",
        IpcCommand::Reload => "reload\n",
        IpcCommand::Quit => "quit\n",
        IpcCommand::Ping => "ping\n",
    };
    stream.write_all(line.as_bytes())?;
    if matches!(cmd, IpcCommand::Ping) {
        let mut reader = BufReader::new(&stream);
        let mut reply = String::new();
        reader
            .read_line(&mut reply)
            .context("read ping reply")?;
        if !reply.trim().starts_with("ok") {
            anyhow::bail!("ping failed: {reply:?}");
        }
        // Panel parses stdout for Live vs Hidden — must print the reply.
        print!("{reply}");
    }
    Ok(())
}

pub fn spawn_server(tx: Sender<IpcCommand>, force_hidden: Arc<AtomicBool>) -> Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind {}", path.display()))?;
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let mut line = String::new();
            {
                let mut reader = BufReader::new(&stream);
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
            }
            let Some(cmd) = parse_command(&line) else {
                continue;
            };
            if cmd == IpcCommand::Ping {
                let reply = ping_reply(force_hidden.load(Ordering::Relaxed));
                let _ = stream.write_all(reply.as_bytes());
                continue;
            }
            // Flip before main-loop delivery so a following `ping` sees Hidden.
            if cmd == IpcCommand::Toggle {
                force_hidden.fetch_xor(true, Ordering::Relaxed);
            }
            let _ = tx.send(cmd);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_commands() {
        assert_eq!(parse_command("quit\n"), Some(IpcCommand::Quit));
        assert_eq!(parse_command("ping"), Some(IpcCommand::Ping));
        assert_eq!(parse_command("toggle"), Some(IpcCommand::Toggle));
        assert_eq!(parse_command("reload"), Some(IpcCommand::Reload));
        assert_eq!(parse_command("settings"), Some(IpcCommand::Settings));
        assert_eq!(parse_command("nope"), None);
    }

    #[test]
    fn ping_reply_live_or_hidden() {
        assert_eq!(ping_reply(false).trim(), "ok live");
        assert_eq!(ping_reply(true).trim(), "ok hidden");
    }
}
