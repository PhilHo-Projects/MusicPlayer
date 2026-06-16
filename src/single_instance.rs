//! Single-instance support.
//!
//! Double-clicking a track in Explorer launches `MusicPlayer.exe "<path>"`. Rather
//! than stacking a new window each time, a second launch hands its path to the
//! already-running window over a loopback socket and then exits — so the open
//! player just loads the new track (and pops to the front).

use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};

use eframe::egui;

// Loopback only, so Windows Firewall never prompts. A fixed high port keyed to
// this app; a clash with another loopback service is vanishingly unlikely.
const HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
const PORT: u16 = 49677;

pub enum Startup {
    /// This process owns the window; hold the listener and serve file opens.
    Primary(TcpListener),
    /// Another instance is already running; we've handed off and should exit.
    Secondary,
}

/// Decide whether this process is the primary window or a secondary launch. A
/// secondary launch forwards `forward_path` to the primary before returning.
pub fn acquire(forward_path: Option<&Path>) -> Startup {
    // A primary already listening? Forward our file and bow out.
    if forward(forward_path) {
        return Startup::Secondary;
    }
    match TcpListener::bind((HOST, PORT)) {
        Ok(listener) => Startup::Primary(listener),
        // Lost a startup race — someone bound just now. Forward to them instead.
        Err(_) => {
            forward(forward_path);
            Startup::Secondary
        }
    }
}

/// Try to reach a running primary and send `path`. Returns true if a primary was
/// reached (regardless of whether a path was sent).
fn forward(path: Option<&Path>) -> bool {
    let Ok(mut stream) = TcpStream::connect((HOST, PORT)) else {
        return false;
    };
    if let Some(path) = path {
        let _ = stream.write_all(path.to_string_lossy().as_bytes());
    }
    true
}

/// Spawn the primary's listener thread. Returns a receiver the UI polls for
/// file-open requests; each forwarded path wakes the UI through `ctx`.
pub fn serve(listener: TcpListener, ctx: egui::Context) -> Receiver<PathBuf> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = String::new();
            // Each connection sends one path then closes (EOF ends the read).
            if stream.read_to_string(&mut buf).is_ok() {
                let path = buf.trim();
                if !path.is_empty() && tx.send(PathBuf::from(path)).is_ok() {
                    ctx.request_repaint();
                }
            }
        }
    });
    rx
}
