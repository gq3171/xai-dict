use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Linux input keycodes (ydotool only).
mod key {
    pub const LEFTCTRL: u16 = 29;
    pub const LEFTSHIFT: u16 = 42;
    pub const V: u16 = 47;
    pub const LEFTALT: u16 = 56;
    pub const RIGHTCTRL: u16 = 97;
    pub const RIGHTALT: u16 = 100;
    pub const INSERT: u16 = 110;
    pub const LEFTMETA: u16 = 125;
    pub const RIGHTMETA: u16 = 126;
    pub const RIGHTSHIFT: u16 = 54;
}

pub fn deliver_text(text: &str, paste: bool) -> Result<()> {
    if text.is_empty() {
        bail!("empty transcript — nothing to paste");
    }
    println!("\n——— transcript ———\n{text}\n—————————————————");
    deliver_text_quiet(text, paste)
}

/// Fast path for streaming phrases: prefer fcitx commit, minimal delay.
///
/// `first` = first chunk of this recording (release modifiers once).
/// Mid-stream chunks skip clipboard paste (would thrash the selection).
/// Update live preedit (streaming partial). Best-effort; ignores failures.
pub fn set_preedit(text: &str) {
    let _ = busctl_call(
        "Preedit",
        &["s", text],
        /* want_true */ false,
    );
}

pub fn clear_preedit() {
    let _ = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.fcitx.Fcitx5",
            "/xaidict",
            "org.fcitx.Fcitx.XaiDict1",
            "ClearPreedit",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Commit final phrase and clear preedit (fcitx addon clears preedit on Commit).
pub fn commit_final(text: &str, paste: bool) -> Result<bool> {
    if text.is_empty() {
        clear_preedit();
        return Ok(false);
    }
    if !paste {
        return Ok(false);
    }
    for _ in 0..3 {
        if try_fcitx5_commit(text) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // last resort
    deliver_text_quiet(text, true)?;
    Ok(true)
}

fn busctl_call(method: &str, extra: &[&str], want_true: bool) -> bool {
    let mut args = vec![
        "--user",
        "call",
        "org.fcitx.Fcitx5",
        "/xaidict",
        "org.fcitx.Fcitx.XaiDict1",
        method,
    ];
    args.extend_from_slice(extra);
    let output = Command::new("busctl")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => {
            if !want_true {
                return true;
            }
            String::from_utf8_lossy(&o.stdout).contains("true")
        }
        _ => false,
    }
}

pub fn deliver_stream_chunk(text: &str, paste: bool, first: bool) -> Result<bool> {
    if text.is_empty() || !paste {
        return Ok(false);
    }
    if first {
        std::thread::sleep(Duration::from_millis(120));
        release_modifiers_silent();
    }
    // A couple of quick fcitx tries — user is mid-dictation, focus should hold.
    for _ in 0..3 {
        if try_fcitx5_commit(text) {
            tracing::info!(n = text.len(), first, "stream commit via fcitx5");
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    // Fallback only for the first chunk (clipboard paste mid-stream is messy).
    if first {
        tracing::info!("stream fcitx failed on first chunk — clipboard paste once");
        deliver_text_quiet(text, true)?;
        return Ok(true);
    }
    tracing::warn!(n = text.len(), "stream chunk not committed (no focused IC)");
    Ok(false)
}

/// Inject text into the focused field.
///
/// Order:
/// 1. fcitx5 `commitString` (focused IC) — no clipboard, no key synthesis
/// 2. Clipboard + Ctrl+V via uinput / keyd chord (only real key events)
pub fn deliver_text_quiet(text: &str, paste: bool) -> Result<()> {
    if text.is_empty() {
        bail!("empty transcript — nothing to paste");
    }
    if !paste {
        return Ok(());
    }

    crate::osd::hide();
    // Let physical Right Alt fully release. Do NOT send keyd word-commands
    // here — on keyd 2.x unknown tokens are typed as letter sequences
    // ("keyupall" → k e y u p a l l), which polluted the text field.
    std::thread::sleep(Duration::from_millis(280));
    release_modifiers_silent();

    // ── 1) Pure IME path (no clipboard, no synthetic keys) ────────────────
    for attempt in 1..=5 {
        if try_fcitx5_commit(text) {
            tracing::info!(attempt, "committed via fcitx5 IME (no clipboard)");
            eprintln!("已上屏 (fcitx5 IME，未占用剪贴板)");
            return Ok(());
        }
        if attempt < 5 {
            std::thread::sleep(Duration::from_millis(120));
        }
    }
    tracing::info!("fcitx5 has no focused IC after retries — using clipboard paste");

    // ── 2) Clipboard + paste ──────────────────────────────────────────────
    let previous = read_clipboard();
    let holder = set_clipboard_held(text)?;
    std::thread::sleep(Duration::from_millis(200));

    let on_clip = read_clipboard().unwrap_or_default();
    if on_clip != text {
        tracing::warn!(
            got_len = on_clip.len(),
            want_len = text.len(),
            "clipboard mismatch after set — still attempting paste"
        );
    }

    release_modifiers_silent();
    std::thread::sleep(Duration::from_millis(50));

    let method = try_paste_any();
    if let Some(how) = method {
        tracing::info!(%how, "paste key sequence sent (clipboard held)");
        eprintln!("已填入 ({how})");
        schedule_clipboard_restore(holder, previous, text.to_string());
        return Ok(());
    }

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(30));
        drop(holder);
    });
    tracing::warn!("auto key-paste failed; text left on clipboard for manual Ctrl+V");
    eprintln!("自动按键粘贴失败 — 文字已在剪贴板，请到输入框按 Ctrl+V");
    Ok(())
}

struct ClipboardHolder {
    child: Option<std::process::Child>,
}

impl Drop for ClipboardHolder {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn schedule_clipboard_restore(
    holder: ClipboardHolder,
    previous: Option<String>,
    current: String,
) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        drop(holder);
        if let Some(prev) = previous {
            if read_clipboard().as_deref() == Some(current.as_str()) {
                let _ = set_clipboard_quick(&prev);
            }
        }
    });
}

// ─── fcitx5 IME ─────────────────────────────────────────────────────────────

fn try_fcitx5_commit(text: &str) -> bool {
    let output = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.fcitx.Fcitx5",
            "/xaidict",
            "org.fcitx.Fcitx.XaiDict1",
            "Commit",
            "s",
            text,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let ok = stdout.contains("true");
            if !ok {
                tracing::debug!(%stdout, "fcitx5 Commit returned false");
            }
            ok
        }
        Ok(o) => {
            tracing::debug!(
                stderr = %String::from_utf8_lossy(&o.stderr),
                "fcitx5 Commit call failed"
            );
            false
        }
        Err(e) => {
            tracing::debug!(%e, "busctl not available");
            false
        }
    }
}

// ─── clipboard ──────────────────────────────────────────────────────────────

fn set_clipboard_held(text: &str) -> Result<ClipboardHolder> {
    let _ = set_klipper(text);

    if which("wl-copy") {
        let mut child = Command::new("wl-copy")
            .args(["--foreground", "--type", "text/plain"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn wl-copy --foreground")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        std::thread::sleep(Duration::from_millis(50));
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!(%status, "wl-copy --foreground exited early; using quick mode");
                set_clipboard_quick(text)?;
                return Ok(ClipboardHolder { child: None });
            }
            Ok(None) => return Ok(ClipboardHolder { child: Some(child) }),
            Err(e) => tracing::debug!(%e, "wl-copy try_wait failed"),
        }
    }

    set_clipboard_quick(text)?;
    Ok(ClipboardHolder { child: None })
}

fn set_clipboard_quick(text: &str) -> Result<()> {
    let _ = set_klipper(text);

    if which("wl-copy") {
        let mut child = Command::new("wl-copy")
            .args(["--type", "text/plain"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn wl-copy")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
        bail!("wl-copy failed: {status}");
    }
    if which("xclip") {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn xclip")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
        bail!("xclip failed: {status}");
    }
    bail!("no clipboard tool (need wl-copy or xclip)");
}

fn set_klipper(text: &str) -> bool {
    for bus in ["qdbus6", "qdbus-qt6", "qdbus"] {
        if !which(bus) {
            continue;
        }
        let status = Command::new(bus)
            .args([
                "org.kde.klipper",
                "/klipper",
                "org.kde.klipper.klipper.setClipboardContents",
                text,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(status, Ok(s) if s.success()) {
            return true;
        }
    }
    false
}

fn read_clipboard() -> Option<String> {
    for bus in ["qdbus6", "qdbus-qt6", "qdbus"] {
        if !which(bus) {
            continue;
        }
        let out = Command::new(bus)
            .args([
                "org.kde.klipper",
                "/klipper",
                "org.kde.klipper.klipper.getClipboardContents",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if out.status.success() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                let s = s.trim_end_matches('\n').to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    if which("wl-paste") {
        let out = Command::new("wl-paste")
            .args(["--no-newline"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if out.status.success() {
            return String::from_utf8(out.stdout).ok();
        }
    }
    None
}

// ─── keyboard paste ─────────────────────────────────────────────────────────

fn try_paste_any() -> Option<&'static str> {
    // Only emit real EV_KEY events. Never feed multi-letter tokens to
    // `keyd do` except known chords (C-v) — bare words are typed as letters.
    if try_uinput_paste() {
        return Some("uinput Ctrl+V");
    }
    if try_keyd_chord_paste() {
        return Some("keyd Ctrl+V");
    }
    if try_ydotool_paste_variants() {
        return Some("ydotool Ctrl+V");
    }
    if try_xdotool_paste() {
        return Some("xdotool Ctrl+V");
    }
    if try_wtype_paste() {
        return Some("wtype Ctrl+V");
    }
    None
}

/// Release modifiers via raw uinput only (never keyd word commands).
fn release_modifiers_silent() {
    let _ = uinput_emit_keys(&[
        (evdev::Key::KEY_RIGHTALT, 0),
        (evdev::Key::KEY_LEFTALT, 0),
        (evdev::Key::KEY_LEFTCTRL, 0),
        (evdev::Key::KEY_RIGHTCTRL, 0),
        (evdev::Key::KEY_LEFTSHIFT, 0),
        (evdev::Key::KEY_RIGHTSHIFT, 0),
        (evdev::Key::KEY_LEFTMETA, 0),
        (evdev::Key::KEY_RIGHTMETA, 0),
    ]);
    // ydotool is often a no-op on this machine; still try.
    release_modifiers_ydotool();
}

fn try_uinput_paste() -> bool {
    // Release mods + Ctrl+V in one short-lived virtual keyboard.
    let ok = uinput_emit_keys(&[
        (evdev::Key::KEY_RIGHTALT, 0),
        (evdev::Key::KEY_LEFTALT, 0),
        (evdev::Key::KEY_LEFTCTRL, 0),
        (evdev::Key::KEY_RIGHTCTRL, 0),
        (evdev::Key::KEY_LEFTSHIFT, 0),
        (evdev::Key::KEY_RIGHTSHIFT, 0),
        (evdev::Key::KEY_LEFTMETA, 0),
        (evdev::Key::KEY_RIGHTMETA, 0),
        (evdev::Key::KEY_LEFTCTRL, 1),
        (evdev::Key::KEY_V, 1),
        (evdev::Key::KEY_V, 0),
        (evdev::Key::KEY_LEFTCTRL, 0),
    ]);
    if ok {
        std::thread::sleep(Duration::from_millis(150));
    }
    ok
}

/// Create a temporary virtual keyboard, emit (key, value) pairs, drop device.
fn uinput_emit_keys(seq: &[(evdev::Key, i32)]) -> bool {
    use evdev::{AttributeSet, EventType, InputEvent, Key, uinput::VirtualDeviceBuilder};

    let mut keys = AttributeSet::<Key>::new();
    for k in [
        Key::KEY_LEFTCTRL,
        Key::KEY_RIGHTCTRL,
        Key::KEY_LEFTALT,
        Key::KEY_RIGHTALT,
        Key::KEY_LEFTSHIFT,
        Key::KEY_RIGHTSHIFT,
        Key::KEY_LEFTMETA,
        Key::KEY_RIGHTMETA,
        Key::KEY_V,
        Key::KEY_INSERT,
    ] {
        keys.insert(k);
    }

    let mut dev = match VirtualDeviceBuilder::new()
        .and_then(|b| b.name("xai-dict-paste").with_keys(&keys))
        .and_then(|b| b.build())
    {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(%e, "uinput VirtualDevice create failed");
            return false;
        }
    };

    // Compositor must see the device before events matter.
    std::thread::sleep(Duration::from_millis(80));

    let events: Vec<InputEvent> = seq
        .iter()
        .map(|(k, v)| InputEvent::new(EventType::KEY, k.code(), *v))
        .collect();

    if let Err(e) = dev.emit(&events) {
        tracing::debug!(%e, "uinput emit failed");
        return false;
    }
    true
}

/// keyd: ONLY single chord tokens that cannot be mistaken for letter typing.
/// Never pass "keyup", "keydown", "keyupall", or multi-word strings.
fn try_keyd_chord_paste() -> bool {
    if !which("keyd") {
        return false;
    }
    // Each of these is one argv token → one chord / macro, not letter spam.
    for chord in ["C-v", "C-S-v", "S-insert"] {
        if keyd_do_cmd(&[chord]) {
            std::thread::sleep(Duration::from_millis(120));
            return true;
        }
    }
    false
}

fn keyd_do_cmd(args: &[&str]) -> bool {
    let mut cmd = Command::new("keyd");
    cmd.arg("do");
    for a in args {
        cmd.arg(a);
    }
    matches!(
        cmd.stdout(Stdio::null()).stderr(Stdio::null()).status(),
        Ok(s) if s.success()
    )
}

fn ydotool_env(cmd: &mut Command) {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let sock = format!("{runtime}/.ydotool_socket");
        if std::path::Path::new(&sock).exists() {
            cmd.env("YDOTOOL_SOCKET", sock);
            return;
        }
    }
    if let Ok(sock) = std::env::var("YDOTOOL_SOCKET") {
        cmd.env("YDOTOOL_SOCKET", sock);
    }
}

fn ydotool_keys(args: &[String]) -> bool {
    if !which("ydotool") {
        return false;
    }
    let mut cmd = Command::new("ydotool");
    ydotool_env(&mut cmd);
    let status = cmd
        .arg("key")
        .arg("-d")
        .arg("40")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match status {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            tracing::debug!(
                stderr = %String::from_utf8_lossy(&o.stderr),
                "ydotool key failed"
            );
            false
        }
        Err(e) => {
            tracing::debug!(%e, "ydotool spawn failed");
            false
        }
    }
}

fn release_modifiers_ydotool() {
    if !which("ydotool") {
        return;
    }
    let ups = [
        key::RIGHTALT,
        key::LEFTALT,
        key::LEFTCTRL,
        key::RIGHTCTRL,
        key::LEFTSHIFT,
        key::RIGHTSHIFT,
        key::LEFTMETA,
        key::RIGHTMETA,
    ]
    .map(|c| format!("{c}:0"));
    let _ = ydotool_keys(&ups);
}

fn try_ydotool_paste_variants() -> bool {
    if !which("ydotool") {
        return false;
    }
    let sock_ok = std::env::var("YDOTOOL_SOCKET")
        .map(|p| std::path::Path::new(&p).exists())
        .unwrap_or(false)
        || std::env::var("XDG_RUNTIME_DIR")
            .map(|r| std::path::Path::new(&r).join(".ydotool_socket").exists())
            .unwrap_or(false);
    if !sock_ok {
        return false;
    }

    for combo in [
        vec![
            format!("{}:1", key::LEFTCTRL),
            format!("{}:1", key::V),
            format!("{}:0", key::V),
            format!("{}:0", key::LEFTCTRL),
        ],
        vec![
            format!("{}:1", key::LEFTCTRL),
            format!("{}:1", key::LEFTSHIFT),
            format!("{}:1", key::V),
            format!("{}:0", key::V),
            format!("{}:0", key::LEFTSHIFT),
            format!("{}:0", key::LEFTCTRL),
        ],
        vec![
            format!("{}:1", key::LEFTSHIFT),
            format!("{}:1", key::INSERT),
            format!("{}:0", key::INSERT),
            format!("{}:0", key::LEFTSHIFT),
        ],
    ] {
        release_modifiers_ydotool();
        std::thread::sleep(Duration::from_millis(40));
        if ydotool_keys(&combo) {
            std::thread::sleep(Duration::from_millis(150));
            return true;
        }
    }
    false
}

fn try_wtype_paste() -> bool {
    if !which("wtype") {
        return false;
    }
    let status = Command::new("wtype")
        .args(["-M", "ctrl", "-k", "v", "-m", "ctrl"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(status, Ok(s) if s.success())
}

fn try_xdotool_paste() -> bool {
    if !which("xdotool") {
        return false;
    }
    Command::new("xdotool")
        .args(["key", "--clearmodifiers", "ctrl+v"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

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
