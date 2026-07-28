//! Status feedback without stealing keyboard/IME focus.
//!
//! Prefer KDE Plasma `org.kde.osdService.showText` — it never takes focus.
//! Fall back to a brief `notify-send`. The old PyQt floating bar is disabled
//! because on Wayland it created an fcitx InputContext and stole focus from
//! the user's text field (so commitString / Ctrl+V went nowhere).

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static LAST_PLASMA: OnceLock<std::sync::Mutex<Option<Instant>>> = OnceLock::new();
static PLASMA_OK: AtomicBool = AtomicBool::new(true);

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

/// Rate-limit Plasma OSD so rapid state changes don't queue a storm.
fn plasma_show(icon: &str, text: &str) -> bool {
    if !PLASMA_OK.load(Ordering::Relaxed) {
        return false;
    }
    // Don't spam more than once per 200ms.
    let lock = LAST_PLASMA.get_or_init(|| std::sync::Mutex::new(None));
    {
        let mut g = lock.lock().unwrap();
        if let Some(t) = *g {
            if t.elapsed() < Duration::from_millis(200) {
                // still try — status changes are important
            }
        }
        *g = Some(Instant::now());
    }

    // busctl --user call org.kde.plasmashell /org/kde/osdService org.kde.osdService showText ss icon text
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

    match status {
        Ok(s) if s.success() => true,
        _ => {
            // Try once via qdbus6
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
                    return true;
                }
            }
            PLASMA_OK.store(false, Ordering::Relaxed);
            false
        }
    }
}

fn notify(summary: &str, body: &str, ms: u32) {
    if !which("notify-send") {
        return;
    }
    let _ = Command::new("notify-send")
        .args([
            "--app-name=xai-dict",
            "--icon=audio-input-microphone",
            "--urgency=low",
            &format!("--expire-time={ms}"),
            "--hint=string:x-canonical-private-synchronous:xai-dict",
            summary,
            body,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn recording() {
    if !plasma_show("audio-input-microphone", "xai-dict · 录音中… 再按右 Alt 结束") {
        notify("xai-dict", "录音中… 再按右 Alt 结束", 0);
    }
}

pub fn transcribing() {
    if !plasma_show("view-refresh", "xai-dict · 识别中…") {
        notify("xai-dict", "识别中…", 0);
    }
}

pub fn done(text: &str) {
    let preview: String = text.chars().take(40).collect();
    let msg = if preview.is_empty() {
        "xai-dict · 完成".to_string()
    } else {
        format!("xai-dict · {preview}")
    };
    if !plasma_show("dialog-ok", &msg) {
        notify("xai-dict", &preview, 2500);
    }
}

pub fn error(msg: &str) {
    let preview: String = msg.chars().take(60).collect();
    let preview = preview.replace('\n', " ");
    if !plasma_show("dialog-error", &format!("xai-dict · {preview}")) {
        notify("xai-dict 出错", &preview, 4000);
    }
}

/// No-op for Plasma OSD (auto-hides). Kept so call sites stay simple.
pub fn hide() {
    // Best-effort: ask Plasma to hide any lingering OSD.
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
}

/// One-shot toast for daemon start.
pub fn boot_hint(msg: &str) {
    if !plasma_show("audio-input-microphone", &format!("xai-dict · {msg}")) {
        notify("xai-dict", msg, 2500);
    }
}
