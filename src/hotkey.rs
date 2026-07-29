//! Global hotkey via evdev.
//!
//! - **toggle mode**: emit [`HotkeyEvent::Press`] on key down (daemon toggles).
//! - **ptt mode**: emit Press on down, [`HotkeyEvent::Release`] on up (hold-to-talk).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// Physical key down (or toggle-mode fire).
    Press,
    /// Physical key up (ptt mode only).
    Release,
}

/// Parse config hotkey name → evdev key. `none` disables the listener.
pub fn parse_key(name: &str) -> Option<Key> {
    let n = name.trim().to_ascii_lowercase().replace(['-', ' '], "");
    match n.as_str() {
        "" | "rightalt" | "altr" | "ralt" | "alt_r" | "right_alt" | "altgr" => {
            Some(Key::KEY_RIGHTALT)
        }
        "leftalt" | "altl" | "lalt" | "alt_l" | "left_alt" | "alt" => Some(Key::KEY_LEFTALT),
        "rightctrl" | "rctrl" | "ctrl_r" | "control_r" => Some(Key::KEY_RIGHTCTRL),
        "leftctrl" | "lctrl" | "ctrl_l" | "control_l" | "ctrl" | "control" => {
            Some(Key::KEY_LEFTCTRL)
        }
        "rightshift" | "rshift" | "shift_r" => Some(Key::KEY_RIGHTSHIFT),
        "leftshift" | "lshift" | "shift_l" | "shift" => Some(Key::KEY_LEFTSHIFT),
        "rightmeta" | "rmeta" | "rightsuper" | "rsuper" | "super_r" | "meta_r" => {
            Some(Key::KEY_RIGHTMETA)
        }
        "leftmeta" | "lmeta" | "leftsuper" | "lsuper" | "super" | "super_l" | "meta"
        | "meta_l" | "win" => Some(Key::KEY_LEFTMETA),
        "capslock" | "caps" => Some(Key::KEY_CAPSLOCK),
        "scrolllock" | "scroll" => Some(Key::KEY_SCROLLLOCK),
        "pause" | "break" => Some(Key::KEY_PAUSE),
        "insert" | "ins" => Some(Key::KEY_INSERT),
        "home" => Some(Key::KEY_HOME),
        "end" => Some(Key::KEY_END),
        "delete" | "del" => Some(Key::KEY_DELETE),
        "menu" | "compose" | "app" => Some(Key::KEY_COMPOSE),
        "f1" => Some(Key::KEY_F1),
        "f2" => Some(Key::KEY_F2),
        "f3" => Some(Key::KEY_F3),
        "f4" => Some(Key::KEY_F4),
        "f5" => Some(Key::KEY_F5),
        "f6" => Some(Key::KEY_F6),
        "f7" => Some(Key::KEY_F7),
        "f8" => Some(Key::KEY_F8),
        "f9" => Some(Key::KEY_F9),
        "f10" => Some(Key::KEY_F10),
        "f11" => Some(Key::KEY_F11),
        "f12" => Some(Key::KEY_F12),
        "none" | "off" | "disabled" | "disable" => None,
        other => {
            tracing::warn!(key = other, "unknown hotkey, defaulting to Right Alt");
            Some(Key::KEY_RIGHTALT)
        }
    }
}

/// Canonical config string for a key (for round-trip with settings GUI).
#[allow(dead_code)]
pub fn key_to_config(key: Key) -> &'static str {
    match key {
        Key::KEY_RIGHTALT => "rightalt",
        Key::KEY_LEFTALT => "leftalt",
        Key::KEY_RIGHTCTRL => "rightctrl",
        Key::KEY_LEFTCTRL => "leftctrl",
        Key::KEY_RIGHTSHIFT => "rightshift",
        Key::KEY_LEFTSHIFT => "leftshift",
        Key::KEY_RIGHTMETA => "rightmeta",
        Key::KEY_LEFTMETA => "leftmeta",
        Key::KEY_CAPSLOCK => "capslock",
        Key::KEY_SCROLLLOCK => "scrolllock",
        Key::KEY_PAUSE => "pause",
        Key::KEY_INSERT => "insert",
        Key::KEY_HOME => "home",
        Key::KEY_END => "end",
        Key::KEY_DELETE => "delete",
        Key::KEY_COMPOSE => "menu",
        Key::KEY_F1 => "f1",
        Key::KEY_F2 => "f2",
        Key::KEY_F3 => "f3",
        Key::KEY_F4 => "f4",
        Key::KEY_F5 => "f5",
        Key::KEY_F6 => "f6",
        Key::KEY_F7 => "f7",
        Key::KEY_F8 => "f8",
        Key::KEY_F9 => "f9",
        Key::KEY_F10 => "f10",
        Key::KEY_F11 => "f11",
        Key::KEY_F12 => "f12",
        _ => "rightalt",
    }
}

pub fn key_label(key: Key) -> &'static str {
    match key {
        Key::KEY_RIGHTALT => "Right Alt",
        Key::KEY_LEFTALT => "Left Alt",
        Key::KEY_RIGHTCTRL => "Right Ctrl",
        Key::KEY_LEFTCTRL => "Left Ctrl",
        Key::KEY_RIGHTSHIFT => "Right Shift",
        Key::KEY_LEFTSHIFT => "Left Shift",
        Key::KEY_RIGHTMETA => "Right Super",
        Key::KEY_LEFTMETA => "Left Super",
        Key::KEY_CAPSLOCK => "Caps Lock",
        Key::KEY_SCROLLLOCK => "Scroll Lock",
        Key::KEY_PAUSE => "Pause",
        Key::KEY_INSERT => "Insert",
        Key::KEY_HOME => "Home",
        Key::KEY_END => "End",
        Key::KEY_DELETE => "Delete",
        Key::KEY_COMPOSE => "Menu",
        Key::KEY_F1 => "F1",
        Key::KEY_F2 => "F2",
        Key::KEY_F3 => "F3",
        Key::KEY_F4 => "F4",
        Key::KEY_F5 => "F5",
        Key::KEY_F6 => "F6",
        Key::KEY_F7 => "F7",
        Key::KEY_F8 => "F8",
        Key::KEY_F9 => "F9",
        Key::KEY_F10 => "F10",
        Key::KEY_F11 => "F11",
        Key::KEY_F12 => "F12",
        _ => "hotkey",
    }
}

/// Human label for a config string (including `none`).
#[allow(dead_code)]
pub fn config_label(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "none" | "off" | "disabled" | "" => "已关闭".into(),
        other => match parse_key(other) {
            Some(k) => key_label(k).to_string(),
            None => other.to_string(),
        },
    }
}

/// Spawn background thread listening for `key`.
///
/// When `ptt` is false, only Press is sent (toggle semantics).
/// When `ptt` is true, Press on down and Release on up.
pub fn spawn_listener(
    key: Key,
    ptt: bool,
    tx: mpsc::UnboundedSender<HotkeyEvent>,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();

    thread::Builder::new()
        .name("xai-dict-hotkey".into())
        .spawn(move || {
            if let Err(e) = run_loop(key, ptt, tx, stop_t) {
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
    ptt: bool,
    tx: mpsc::UnboundedSender<HotkeyEvent>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut devices: Vec<Device> = Vec::new();
    let mut watched_paths: Vec<PathBuf> = Vec::new();
    let mut last_scan = Instant::now() - Duration::from_secs(60);
    let mut last_press = Instant::now() - Duration::from_secs(10);
    let mut last_release = Instant::now() - Duration::from_secs(10);
    let mut logged_empty = false;
    // Track held state so key-repeat (value=2) does not re-fire Press in PTT.
    let mut held = false;

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
                // 1 = press, 0 = release, 2 = repeat
                match ev.value() {
                    1 => {
                        if ptt {
                            if held {
                                continue;
                            }
                            held = true;
                        }
                        if last_press.elapsed() < Duration::from_millis(DEBOUNCE_MS) {
                            tracing::debug!("hotkey press debounced");
                            continue;
                        }
                        last_press = Instant::now();
                        tracing::info!(key = ?key, ptt, "hotkey press");
                        if tx.send(HotkeyEvent::Press).is_err() {
                            return Ok(());
                        }
                    }
                    0 if ptt => {
                        if !held {
                            // Orphan release (lost press) — ignore.
                            continue;
                        }
                        held = false;
                        if last_release.elapsed() < Duration::from_millis(DEBOUNCE_MS) {
                            tracing::debug!("hotkey release debounced");
                            continue;
                        }
                        last_release = Instant::now();
                        tracing::info!(key = ?key, "hotkey release (ptt stop)");
                        if tx.send(HotkeyEvent::Release).is_err() {
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
        }
        if dead {
            devices.clear();
            held = false;
        }
        if !got {
            thread::sleep(Duration::from_millis(8));
        }
    }
    Ok(())
}
