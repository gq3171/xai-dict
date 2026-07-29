//! Global hotkey via evdev (Right Alt by default).
//!
//! Emits a single **Toggle** on key **press** (not release). This avoids broken
//! down/up pairing when keyd + physical keyboards both appear, or when Up is lost.
//!
//! Requires membership in the `input` group.

use evdev::{Device, InputEventKind, Key};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Debounce duplicate presses (two devices, key repeat noise).
const DEBOUNCE_MS: u64 = 120;

/// Parse config: "rightalt" | "leftalt" | "none"
pub fn parse_key(name: &str) -> Option<Key> {
    match name.trim().to_ascii_lowercase().as_str() {
        "" | "rightalt" | "alt_r" | "ralt" | "right_alt" => Some(Key::KEY_RIGHTALT),
        "leftalt" | "alt_l" | "lalt" | "left_alt" => Some(Key::KEY_LEFTALT),
        "none" | "off" | "disabled" => None,
        other => {
            tracing::warn!(key = other, "unknown hotkey, defaulting to Right Alt");
            Some(Key::KEY_RIGHTALT)
        }
    }
}

pub fn key_label(key: Key) -> &'static str {
    match key {
        Key::KEY_RIGHTALT => "Right Alt",
        Key::KEY_LEFTALT => "Left Alt",
        _ => "hotkey",
    }
}

/// Spawn background thread; each press of `key` sends one unit on `tx`.
pub fn spawn_listener(key: Key, tx: mpsc::UnboundedSender<()>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();

    thread::Builder::new()
        .name("xai-dict-hotkey".into())
        .spawn(move || {
            if let Err(e) = run_loop(key, tx, stop_t) {
                tracing::error!("hotkey listener exited: {e:#}");
            }
        })
        .expect("spawn hotkey thread");

    stop
}

/// Prefer keyd virtual keyboard (it grabs hardware); else all keyboards with the key.
fn list_keyboards(want: Key) -> Vec<PathBuf> {
    let mut all = Vec::new();
    let mut keyd = Vec::new();
    let Ok(dir) = std::fs::read_dir("/dev/input") else {
        return all;
    };
    for ent in dir.flatten() {
        let path = ent.path();
        let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !fname.starts_with("event") {
            continue;
        }
        let Ok(dev) = Device::open(&path) else {
            continue;
        };
        let has = dev
            .supported_keys()
            .map(|keys| keys.contains(want))
            .unwrap_or(false);
        if !has {
            continue;
        }
        let name = dev.name().unwrap_or("").to_ascii_lowercase();
        if name.contains("keyd") {
            keyd.push(path.clone());
        }
        all.push(path);
    }
    if !keyd.is_empty() {
        tracing::debug!(
            n = keyd.len(),
            "hotkey: using keyd virtual keyboard only (avoids double events)"
        );
        return keyd;
    }
    all
}

fn run_loop(
    key: Key,
    tx: mpsc::UnboundedSender<()>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut devices: Vec<Device> = Vec::new();
    let mut watched_paths: Vec<PathBuf> = Vec::new();
    let mut last_scan = Instant::now() - Duration::from_secs(60);
    let mut last_fire = Instant::now() - Duration::from_secs(10);
    let mut logged_empty = false;

    while !stop.load(Ordering::Relaxed) {
        if last_scan.elapsed() > Duration::from_secs(5) || devices.is_empty() {
            last_scan = Instant::now();
            let paths = list_keyboards(key);
            // Only reopen / log when the device set actually changes.
            if paths != watched_paths || devices.is_empty() {
                devices.clear();
                watched_paths = paths.clone();
                for path in &paths {
                    match Device::open(path) {
                        Ok(d) => {
                            tracing::info!(
                                path = %path.display(),
                                name = d.name().unwrap_or("?"),
                                "hotkey: watching"
                            );
                            devices.push(d);
                        }
                        Err(e) => tracing::debug!(path = %path.display(), "open: {e}"),
                    }
                }
                if devices.is_empty() {
                    if !logged_empty {
                        tracing::warn!(
                            "hotkey: no keyboard exposes {:?} — add user to `input` group?",
                            key
                        );
                        logged_empty = true;
                    }
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
                logged_empty = false;
            }
        }

        let mut got = false;
        let mut dead = false;
        for dev in &mut devices {
            let events = match dev.fetch_events() {
                Ok(ev) => ev,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => {
                    tracing::debug!("fetch_events: {e}");
                    dead = true;
                    continue;
                }
            };
            for ev in events {
                got = true;
                let InputEventKind::Key(k) = ev.kind() else {
                    continue;
                };
                if k != key {
                    continue;
                }
                // 1 = press only (ignore 0=release, 2=repeat)
                if ev.value() != 1 {
                    continue;
                }
                if last_fire.elapsed() < Duration::from_millis(DEBOUNCE_MS) {
                    tracing::debug!("hotkey press debounced");
                    continue;
                }
                last_fire = Instant::now();
                tracing::info!(key = ?key, "hotkey press → toggle");
                if tx.send(()).is_err() {
                    return Ok(());
                }
            }
        }
        if dead {
            devices.clear();
        }
        if !got {
            thread::sleep(Duration::from_millis(8));
        }
    }
    Ok(())
}
