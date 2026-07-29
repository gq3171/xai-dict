//! Launch the xai-dict settings GUI / config helpers.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;

/// Prefer installed data script, then repo scripts/, then beside the binary.
pub fn settings_script_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("XAI_DICT_SETTINGS_GUI") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    for p in [
        PathBuf::from("/usr/share/xai-dict/settings_gui.py"),
        PathBuf::from("/usr/lib/xai-dict/settings_gui.py"),
    ] {
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(data) = dirs::data_local_dir() {
        let p = data.join("xai-dict/settings_gui.py");
        if p.is_file() {
            return Some(p);
        }
    }
    // cargo run from repo
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/settings_gui.py");
    if manifest.is_file() {
        return Some(manifest);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("settings_gui.py");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub fn install_settings_script() -> Result<PathBuf> {
    let data = dirs::data_local_dir()
        .context("XDG_DATA_HOME")?
        .join("xai-dict");
    std::fs::create_dir_all(&data)?;
    let dest = data.join("settings_gui.py");
    std::fs::write(&dest, include_str!("../scripts/settings_gui.py"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
    }
    Ok(dest)
}

pub fn run_gui() -> Result<()> {
    let script = match settings_script_path() {
        Some(p) => p,
        None => install_settings_script()?,
    };
    // Ensure script is up to date on install path when running from cargo install.
    if let Some(data) = dirs::data_local_dir() {
        let installed = data.join("xai-dict/settings_gui.py");
        if script == installed || !installed.is_file() {
            let _ = install_settings_script();
        }
    }

    let cfg_path = Config::config_path();
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !cfg_path.exists() {
        Config::default().save()?;
    }

    // Prefer python3 with PyQt6.
    let py = which("python3").context("python3 not found")?;
    let status = Command::new(&py)
        .arg(&script)
        .arg(cfg_path.as_os_str())
        .status()
        .with_context(|| format!("run {} {}", py, script.display()))?;

    if !status.success() {
        // Fallback: open in editor.
        eprintln!(
            "settings GUI exited ({status}). Opening config in editor…\n  {}",
            cfg_path.display()
        );
        open_editor(&cfg_path)?;
    }
    Ok(())
}

pub fn open_editor(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        Config::default().save()?;
    }
    for cmd in ["kate", "kwrite", "xdg-open", "editor"] {
        if which(cmd).is_some() {
            let st = Command::new(cmd).arg(path).status();
            if matches!(st, Ok(s) if s.success() || s.code() == Some(0)) {
                return Ok(());
            }
            // xdg-open may return before editor closes
            if cmd == "xdg-open" && st.is_ok() {
                return Ok(());
            }
        }
    }
    bail!("no editor found; edit manually: {}", path.display());
}

pub fn print_config(cfg: &Config) {
    println!("path: {}", Config::config_path().display());
    let raw = toml::to_string_pretty(cfg).unwrap_or_else(|_| "<serialize error>".into());
    print!("{raw}");
}

pub fn set_key(key: &str, value: &str) -> Result<()> {
    let mut cfg = Config::load();
    match key {
        "provider" => {
            cfg.provider = match value.to_ascii_lowercase().as_str() {
                "qwen3" | "qwen" => crate::config::Provider::Qwen3,
                "local" | "whisper" => crate::config::Provider::Local,
                "xai" | "cloud" => crate::config::Provider::Xai,
                _ => bail!("provider must be qwen3|local|xai"),
            };
        }
        "hotkey" => cfg.hotkey = value.to_string(),
        "hotkey_mode" => {
            let v = value.to_ascii_lowercase();
            cfg.hotkey_mode = match v.as_str() {
                "toggle" | "click" | "tap" => "toggle".into(),
                "ptt" | "hold" | "push" | "push-to-talk" | "pushtotalk" => "ptt".into(),
                _ => bail!("hotkey_mode must be toggle|ptt"),
            };
        }
        "input_device" => cfg.input_device = value.to_string(),
        "language" => cfg.language = value.to_string(),
        "paste" => cfg.paste = parse_bool(value)?,
        "proxy" => cfg.proxy = value.to_string(),
        "stream" => cfg.stream = parse_bool(value)?,
        "dual_model" => cfg.dual_model = parse_bool(value)?,
        "dual_preedit" => cfg.dual_preedit = parse_bool(value)?,
        "near_field" => cfg.near_field = parse_bool(value)?,
        "stream_min_silence_ms" => cfg.stream_min_silence_ms = value.parse()?,
        "stream_max_segment_ms" => cfg.stream_max_segment_ms = value.parse()?,
        "stream_min_speech_ms" => cfg.stream_min_speech_ms = value.parse()?,
        "local_threads" => cfg.local_threads = value.parse()?,
        "stream_threads" => cfg.stream_threads = value.parse()?,
        "qwen3_model_dir" => cfg.qwen3_model_dir = value.to_string(),
        "stream_model_dir" => cfg.stream_model_dir = value.to_string(),
        "local_model" => cfg.local_model = value.to_string(),
        "qwen3_hotwords" => cfg.qwen3_hotwords = value.to_string(),
        "qwen3_max_new_tokens" => cfg.qwen3_max_new_tokens = value.parse()?,
        "vad_speech_rms" => cfg.vad_speech_rms = value.parse()?,
        "vad_snr" => cfg.vad_snr = value.parse()?,
        "agc_max_gain" => cfg.agc_max_gain = value.parse()?,
        other => bail!("unknown key: {other}"),
    }
    cfg.save()?;
    println!("set {key} = {value}");
    println!("wrote {}", Config::config_path().display());
    println!("restart daemon to apply: systemctl --user restart xai-dict");
    Ok(())
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("expected true/false, got {s}"),
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
