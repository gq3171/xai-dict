//! Package / user install helpers: systemd unit, path resolution, input group.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Preferred packaged layout.
pub const SYSTEM_BIN: &str = "/usr/bin/xai-dict";
pub const SYSTEM_UNIT: &str = "/usr/lib/systemd/user/xai-dict.service";
pub const SYSTEM_LIBDIR: &str = "/usr/lib/xai-dict";
#[allow(dead_code)]
pub const SYSTEM_SHARE: &str = "/usr/share/xai-dict";

/// Resolve which `xai-dict` binary end users should run under systemd.
///
/// Priority:
/// 1. `XAI_DICT_BIN` override
/// 2. `/usr/bin/xai-dict` (Debian package) if present
/// 3. current executable (if it looks like a real install, not a temp path)
/// 4. `~/.cargo/bin/xai-dict`
/// 5. PATH
pub fn resolve_bin() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("XAI_DICT_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    let system = PathBuf::from(SYSTEM_BIN);
    if system.is_file() {
        return Ok(system);
    }
    if let Ok(exe) = std::env::current_exe() {
        if exe.is_file() {
            let s = exe.to_string_lossy();
            // Prefer stable paths over target/debug during packaging tests.
            if !s.contains("/target/debug/") {
                return Ok(exe);
            }
        }
    }
    if let Some(home) = dirs::home_dir() {
        let cargo = home.join(".cargo/bin/xai-dict");
        if cargo.is_file() {
            return Ok(cargo);
        }
    }
    which("xai-dict")
        .map(PathBuf::from)
        .context("xai-dict not found — install the .deb or: cargo install --path .")
}

pub fn system_package_installed() -> bool {
    Path::new(SYSTEM_BIN).is_file() && Path::new(SYSTEM_UNIT).is_file()
}

/// User unit that would shadow the packaged unit.
#[allow(dead_code)]
pub fn user_unit_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config/systemd/user/xai-dict.service"))
}

/// Ensure systemd --user has xai-dict enabled and (optionally) started.
///
/// For deb installs: prefer the **system** unit under `/usr/lib/systemd/user/`
/// and remove a stale user override that still points at `~/.cargo/bin`.
pub fn ensure_user_service(start_now: bool) -> Result<()> {
    let bin = resolve_bin()?;
    let bin_s = bin.display().to_string();
    let home = dirs::home_dir().context("HOME")?;
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let user_unit = unit_dir.join("xai-dict.service");

    // Stale user unit from old cargo install can shadow the packaged one.
    if system_package_installed() {
        if user_unit.is_file() {
            let raw = std::fs::read_to_string(&user_unit).unwrap_or_default();
            if raw.contains(".cargo/bin/xai-dict") || !raw.contains("/usr/bin/xai-dict") {
                // Back up then remove so packaged unit wins.
                let bak = unit_dir.join("xai-dict.service.cargo-bak");
                let _ = std::fs::rename(&user_unit, &bak);
                println!(
                    "moved stale user unit → {} (using packaged {})",
                    bak.display(),
                    SYSTEM_UNIT
                );
            }
        }
        // When packaged unit exists, do not write a competing user unit.
    } else {
        // Dev / cargo install: write a user unit pointing at this binary.
        let libdir = detect_libdir(&bin);
        let unit_body = format!(
            r#"[Unit]
Description=xai-dict voice dictation daemon
Documentation=https://github.com/gq3171/xai-dict
After=pipewire.service pipewire-pulse.service graphical-session.target
Wants=pipewire.service

[Service]
Type=simple
ExecStart={bin_s} daemon
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info
Environment=PATH=/usr/local/bin:/usr/bin:/bin
Environment=LD_LIBRARY_PATH={libdir}
Environment=XAI_DICT_LIBDIR={libdir}
# Do not kill daemon when the terminal that ran enable exits
KillMode=process

[Install]
WantedBy=default.target
"#
        );
        std::fs::write(&user_unit, unit_body)?;
        println!("wrote {}", user_unit.display());
    }

    // Default config so first login is not empty.
    let _ = crate::config::Config::default().write_default_if_missing();

    // Settings script for non-deb cargo installs.
    if !Path::new("/usr/share/xai-dict/settings_gui.py").is_file() {
        let _ = crate::settings::install_settings_script();
    }

    reload_and_enable(start_now)?;
    check_input_group();
    Ok(())
}

fn detect_libdir(bin: &Path) -> String {
    if Path::new(SYSTEM_LIBDIR).is_dir() {
        return SYSTEM_LIBDIR.into();
    }
    if let Some(parent) = bin.parent() {
        // cargo target/release next to workers
        let cand = parent.join(".");
        if parent.join("qwen3_worker").is_file() {
            return parent.display().to_string();
        }
        let _ = cand;
    }
    SYSTEM_LIBDIR.into()
}

fn reload_and_enable(start_now: bool) -> Result<()> {
    let st = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    if !matches!(st, Ok(s) if s.success()) {
        eprintln!(
            "warn: systemctl --user daemon-reload failed — is a user session running?\n\
             After login: systemctl --user enable --now xai-dict"
        );
        return Ok(());
    }
    let enable = Command::new("systemctl")
        .args(["--user", "enable", "xai-dict.service"])
        .status()
        .context("systemctl enable")?;
    if !enable.success() {
        bail!("systemctl --user enable xai-dict.service failed");
    }
    println!("enabled: systemctl --user enable xai-dict");
    if start_now {
        let start = Command::new("systemctl")
            .args(["--user", "restart", "xai-dict.service"])
            .status();
        if matches!(start, Ok(s) if s.success()) {
            println!("started: systemctl --user status xai-dict");
        } else {
            // Prefer start if restart fails (first install)
            let _ = Command::new("systemctl")
                .args(["--user", "start", "xai-dict.service"])
                .status();
            println!("tried start — check: systemctl --user status xai-dict");
        }
    }
    Ok(())
}

/// Warn if user cannot read /dev/input (hotkey will not work).
pub fn check_input_group() {
    let in_group = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            // Groups: line lists supplementary GIDs
            s.lines()
                .find(|l| l.starts_with("Groups:"))
                .map(|l| l.to_string())
        });
    let input_gid = std::fs::read_to_string("/etc/group")
        .ok()
        .and_then(|g| {
            g.lines().find_map(|l| {
                if l.starts_with("input:") {
                    l.split(':').nth(2)?.parse::<u32>().ok()
                } else {
                    None
                }
            })
        });
    let ok = match (in_group, input_gid) {
        (Some(groups), Some(gid)) => groups.split_whitespace().any(|x| x == gid.to_string()),
        _ => {
            // fallback: try open a sample
            std::fs::read_dir("/dev/input").is_ok()
        }
    };
    if !ok {
        if let Ok(user) = std::env::var("USER") {
            eprintln!(
                "提示: 当前用户不在 input 组，内置全局热键可能无效。\n\
                 执行后重新登录:\n\
                   sudo usermod -aG input {user}\n\
                 或仅用 fcitx Super+V / 系统快捷键绑 xai-dict toggle。"
            );
        } else {
            eprintln!(
                "提示: 可能无法访问 /dev/input — 将用户加入 input 组后重新登录。"
            );
        }
    }
}

/// Full install for CLI `xai-dict install` (desktops + service + optional fcitx).
pub fn install_all(enable: bool, fcitx: bool) -> Result<()> {
    let bin = resolve_bin()?;
    let bin_s = bin.display().to_string();
    let home = dirs::home_dir().context("HOME")?;

    ensure_user_service(enable)?;

    // User desktop entries (always; harmless alongside /usr/share ones)
    let apps = home.join(".local/share/applications");
    std::fs::create_dir_all(&apps)?;

    for (name, body) in [
        (
            "xai-dict-toggle.desktop",
            format!(
                r#"[Desktop Entry]
Name=xai-dict Toggle
Comment=Start/stop voice dictation
Exec={bin_s} toggle
Icon=audio-input-microphone
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
StartupNotify=false
"#
            ),
        ),
        (
            "xai-dict-settings.desktop",
            format!(
                r#"[Desktop Entry]
Name=xai-dict 设置
Name[en]=xai-dict Settings
Comment=Configure voice dictation
Exec={bin_s} config gui
Icon=preferences-desktop-multimedia
Terminal=false
Type=Application
Categories=Settings;Utility;AudioVideo;
StartupNotify=true
"#
            ),
        ),
        (
            "xai-dict.desktop",
            format!(
                r#"[Desktop Entry]
Name=xai-dict
Comment=Voice dictation status
Exec={bin_s} whoami
Icon=audio-input-microphone
Terminal=true
Type=Application
Categories=Utility;AudioVideo;
"#
            ),
        ),
    ] {
        let p = apps.join(name);
        std::fs::write(&p, body)?;
        println!("wrote {}", p.display());
    }

    let data = home.join(".local/share/xai-dict");
    std::fs::create_dir_all(&data)?;
    if let Ok(s) = crate::settings::install_settings_script() {
        println!("wrote {}", s.display());
    }
    let osd = data.join("osd_bar.py");
    if let Ok(()) = std::fs::write(&osd, include_str!("../scripts/osd_bar.py")) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&osd, std::fs::Permissions::from_mode(0o755));
        }
        println!("wrote {}", osd.display());
    }

    let _ = Command::new("update-desktop-database")
        .arg(&apps)
        .status();

    // Optional ydotool for Wayland inject fallback
    ensure_ydotoold(&home);

    if fcitx {
        install_fcitx()?;
    }

    println!(
        r#"
=== 安装完成 ===
二进制: {bin_s}
登录后自动启动: systemctl --user enable xai-dict  （已处理）

首次使用:
  1. xai-dict config          # 下载模型 + 可视化热键
  2. 把用户加入 input 组（全局热键）:
       sudo usermod -aG input $USER   &&  重新登录
  3. 或装 fcitx 插件: xai-dict install --fcitx   (Super+V)

检查:
  systemctl --user status xai-dict
  xai-dict status
  xai-dict mic-test
"#
    );
    Ok(())
}

fn ensure_ydotoold(home: &Path) {
    let ydotool_unit = home.join(".config/systemd/user/ydotoold.service");
    if let Some(parent) = ydotool_unit.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if which("ydotoold").is_none() {
        return;
    }
    let _ = std::fs::write(
        &ydotool_unit,
        r#"[Unit]
Description=ydotool daemon (keyboard automation for xai-dict)

[Service]
Type=simple
ExecStart=/usr/bin/ydotoold --socket-path=%t/.ydotool_socket --socket-perm=0600
Restart=on-failure
RestartSec=1

[Install]
WantedBy=default.target
"#,
    );
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "enable", "--now", "ydotoold.service"])
        .status();
    println!("ensured {}", ydotool_unit.display());
}

fn install_fcitx() -> Result<()> {
    let candidates = [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fcitx5-xaidict/install-user.sh"),
        PathBuf::from("/usr/share/xai-dict/fcitx5-xaidict/install-user.sh"),
        dirs::home_dir()
            .unwrap_or_default()
            .join("Projects/rust/xai-dict/fcitx5-xaidict/install-user.sh"),
    ];
    let script = candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        anyhow::anyhow!(
            "找不到 fcitx5-xaidict/install-user.sh\n\
             deb 暂不内置插件源码时，请从仓库执行: cd fcitx5-xaidict && ./install-user.sh"
        )
    })?;
    println!("running {}", script.display());
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("fcitx install failed ({status})");
    }
    Ok(())
}

/// Called from autostart / postinst helper: enable+start if not running.
pub fn ensure_running_quiet() -> Result<()> {
    let sock = crate::daemon::socket_path();
    if sock.exists() {
        if probe_daemon_alive(&sock) {
            return Ok(());
        }
        // Stale socket after crash — clear so systemd / bind can recover.
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(crate::daemon::pid_path());
    }
    let _ = ensure_user_service(true);
    Ok(())
}

/// Blocking PING on the control socket (no nested tokio runtime).
fn probe_daemon_alive(sock: &Path) -> bool {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let Ok(mut stream) = UnixStream::connect(sock) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if stream.write_all(b"PING\n").is_err() {
        return false;
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => String::from_utf8_lossy(&buf[..n]).contains("OK"),
        _ => false,
    }
}

fn which(name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
        {
            return Some(p.display().to_string());
        }
    }
    None
}
