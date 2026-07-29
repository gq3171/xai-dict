//! Status feedback without stealing keyboard/IME focus.
//!
//! Prefer KDE Plasma `org.kde.osdService.showText` — it never takes focus.
//! Fall back to a **single replaceable** FreeDesktop notification (never stacks).
//! The old PyQt floating bar is disabled because on Wayland it created an fcitx
//! InputContext and stole focus from the user's text field.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Last successful Plasma OSD call. Used for rate-limit + recovery.
static LAST_PLASMA_OK: Mutex<Option<Instant>> = Mutex::new(None);
/// After a Plasma failure, wait this long before retrying (don't pin forever).
static PLASMA_RETRY_AFTER: Mutex<Option<Instant>> = Mutex::new(None);
/// FreeDesktop notification id we keep replacing (0 = none yet).
static NOTIFY_ID: AtomicU32 = AtomicU32::new(0);
/// Whether notify-send / DBus notify path is available.
static NOTIFY_OK: AtomicBool = AtomicBool::new(true);

const PLASMA_RETRY_MS: u64 = 15_000;
const PLASMA_MIN_GAP_MS: u64 = 80;

/// Sticky in-progress states (recording / transcribing). Replaced, not stacked.
const EXPIRE_STATUS_MS: u32 = 120_000;
/// Final success / idle toast.
const EXPIRE_DONE_MS: u32 = 2_500;
/// Error toast.
const EXPIRE_ERROR_MS: u32 = 4_500;

fn which(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        dir.join(name)
            .metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

fn plasma_allowed() -> bool {
    let g = PLASMA_RETRY_AFTER.lock().unwrap_or_else(|e| e.into_inner());
    match *g {
        None => true,
        Some(t) if t.elapsed() >= Duration::from_millis(PLASMA_RETRY_MS) => true,
        Some(_) => false,
    }
}

fn mark_plasma_ok() {
    if let Ok(mut g) = LAST_PLASMA_OK.lock() {
        *g = Some(Instant::now());
    }
    if let Ok(mut g) = PLASMA_RETRY_AFTER.lock() {
        *g = None;
    }
}

fn mark_plasma_failed() {
    if let Ok(mut g) = PLASMA_RETRY_AFTER.lock() {
        *g = Some(Instant::now());
    }
}

/// Show Plasma OSD. Returns true on success.
fn plasma_show(icon: &str, text: &str) -> bool {
    if !plasma_allowed() {
        return false;
    }

    // Soft rate-limit: drop only if identical storm within gap (still allow status).
    if let Ok(g) = LAST_PLASMA_OK.lock() {
        if let Some(t) = *g {
            if t.elapsed() < Duration::from_millis(PLASMA_MIN_GAP_MS) {
                // Allow through — state changes matter; Plasma coalesces OSD.
            }
        }
    }

    let status = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.kde.plasmashell",
            "/org/kde/osdService",
            "org.kde.osdService",
            "showText",
            "ss",
            icon,
            text,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if matches!(status, Ok(s) if s.success()) {
        mark_plasma_ok();
        return true;
    }

    for bus in ["qdbus6", "qdbus-qt6", "qdbus"] {
        if !which(bus) {
            continue;
        }
        let st = Command::new(bus)
            .args([
                "org.kde.plasmashell",
                "/org/kde/osdService",
                "org.kde.osdService.showText",
                icon,
                text,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(st, Ok(s) if s.success()) {
            mark_plasma_ok();
            return true;
        }
    }

    mark_plasma_failed();
    false
}

/// Replaceable FreeDesktop notification via notify-send.
/// Always reuses the same id so Plasma history never stacks xai-dict toasts.
fn notify_replace(icon: &str, body: &str, ms: u32) {
    if !NOTIFY_OK.load(Ordering::Relaxed) {
        return;
    }
    if !which("notify-send") {
        NOTIFY_OK.store(false, Ordering::Relaxed);
        return;
    }

    let prev = NOTIFY_ID.load(Ordering::Relaxed);
    let mut args = vec![
        "--app-name=xai-dict".to_string(),
        format!("--icon={icon}"),
        "--urgency=low".to_string(),
        "--transient".to_string(),
        format!("--expire-time={ms}"),
        "--print-id".to_string(),
    ];
    if prev != 0 {
        args.push(format!("--replace-id={prev}"));
    }
    // Desktop Entry id helps Plasma group/replace by app.
    args.push("--hint=string:desktop-entry:xai-dict".to_string());
    args.push("xai-dict".to_string());
    args.push(body.to_string());

    let output = Command::new("notify-send")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let id_str = String::from_utf8_lossy(&o.stdout);
            if let Ok(id) = id_str.trim().parse::<u32>() {
                if id != 0 {
                    NOTIFY_ID.store(id, Ordering::Relaxed);
                }
            }
        }
        Ok(_) | Err(_) => {
            // Fall back: try D-Bus Notify directly.
            if !notify_via_busctl(icon, body, ms, prev) {
                tracing::debug!("status notify failed");
            }
        }
    }
}

fn notify_via_busctl(icon: &str, body: &str, ms: u32, replaces: u32) -> bool {
    // Notify(s app, u replaces, s icon, s summary, s body, as actions, a{sv} hints, i timeout) → u
    let output = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            "susssasa{sv}i",
            "xai-dict",
            &replaces.to_string(),
            icon,
            "xai-dict",
            body,
            "0",
            "0",
            &((ms as i32).to_string()),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(o) if o.status.success() => {
            // stdout like: u 42
            let s = String::from_utf8_lossy(&o.stdout);
            if let Some(id) = s
                .split_whitespace()
                .filter_map(|t| t.parse::<u32>().ok())
                .next()
            {
                if id != 0 {
                    NOTIFY_ID.store(id, Ordering::Relaxed);
                }
            }
            true
        }
        _ => false,
    }
}

fn close_notification() {
    let id = NOTIFY_ID.swap(0, Ordering::Relaxed);
    if id == 0 {
        return;
    }
    let _ = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "CloseNotification",
            "u",
            &id.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Prefer Plasma OSD; always keep notify fallback as a single replaceable card.
fn show_status(icon: &str, plasma_text: &str, notify_body: &str, expire_ms: u32) {
    let plasma_ok = plasma_show(icon, plasma_text);
    // Even when Plasma OSD works, some sessions never see the overlay (fullscreen,
    // multi-monitor). Use a short replaceable toast only when OSD failed.
    if !plasma_ok {
        notify_replace(icon, notify_body, expire_ms);
    } else {
        // Close any leftover sticky notify from a previous Plasma-down window.
        close_notification();
    }
}

pub fn recording() {
    show_status(
        "audio-input-microphone",
        "xai-dict · 录音中… 再按右 Alt 结束",
        "录音中… 再按右 Alt 结束",
        EXPIRE_STATUS_MS,
    );
}

pub fn transcribing() {
    show_status(
        "view-refresh",
        "xai-dict · 识别中…",
        "识别中…",
        EXPIRE_STATUS_MS,
    );
}

pub fn done(text: &str) {
    let preview: String = text.chars().take(40).collect();
    let plasma = if preview.is_empty() {
        "xai-dict · 完成".to_string()
    } else {
        format!("xai-dict · {preview}")
    };
    let body = if preview.is_empty() {
        "完成".to_string()
    } else {
        preview
    };
    show_status("dialog-ok", &plasma, &body, EXPIRE_DONE_MS);
}

pub fn error(msg: &str) {
    let preview: String = msg.chars().take(80).collect();
    let preview = preview.replace('\n', " ");
    show_status(
        "dialog-error",
        &format!("xai-dict · {preview}"),
        &preview,
        EXPIRE_ERROR_MS,
    );
}

/// Hide Plasma OSD (best-effort) and close our status notification.
pub fn hide() {
    let _ = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.kde.plasmashell",
            "/org/kde/osdService",
            "org.kde.osdService",
            "hide",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    close_notification();
}

/// One-shot toast (daemon start, brief idle hints).
pub fn boot_hint(msg: &str) {
    show_status(
        "audio-input-microphone",
        &format!("xai-dict · {msg}"),
        msg,
        EXPIRE_DONE_MS,
    );
}
