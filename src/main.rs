mod auth;
mod capture;
mod config;
mod daemon;
mod hotkey;
mod local_qwen3;
mod local_whisper;
mod notify;
mod osd;
mod output;
mod pipeline;
mod proxy;
mod local_stream;
mod settings;
mod stream_vad;
mod stt;
mod stt_rest;
mod wav;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::{Config, Provider};
use stt::{SttEvent, SttSession};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "xai-dict",
    about = "Voice dictation IME-style: Qwen3-ASR / Whisper / xAI → focused app",
    version
)]
struct Cli {
    /// Backend: qwen3 (default), local (whisper.cpp), or xai (cloud)
    #[arg(long, value_enum)]
    provider: Option<ProviderCli>,

    /// xAI API key (cloud only; else XAI_API_KEY / ~/.grok/auth.json)
    #[arg(long, env = "XAI_API_KEY")]
    api_key: Option<String>,

    /// Language code (zh, en, auto, …) — mainly for whisper
    #[arg(long)]
    language: Option<String>,

    /// Do not auto-paste; only print transcript
    #[arg(long)]
    no_paste: bool,

    /// Use WebSocket streaming (xai only; needs direct WSS)
    #[arg(long)]
    stream: bool,

    /// HTTP proxy for cloud provider
    #[arg(long, env = "HTTPS_PROXY")]
    proxy: Option<String>,

    /// Path to whisper.cpp ggml model (local provider)
    #[arg(long)]
    model: Option<String>,

    /// Qwen3-ASR model directory (qwen3 provider)
    #[arg(long)]
    qwen3_dir: Option<String>,

    #[arg(long, default_value = "info")]
    log: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Debug, ValueEnum)]
enum ProviderCli {
    Qwen3,
    Local,
    Xai,
}

impl From<ProviderCli> for Provider {
    fn from(p: ProviderCli) -> Self {
        match p {
            ProviderCli::Qwen3 => Provider::Qwen3,
            ProviderCli::Local => Provider::Local,
            ProviderCli::Xai => Provider::Xai,
        }
    }
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// One-shot: record until Ctrl+C / Enter, then paste
    Dict {
        #[arg(long, default_value_t = 0)]
        max_secs: u64,
    },
    /// Background daemon (Lazy-style). Bind a global hotkey to `xai-dict toggle`.
    Daemon,
    /// Toggle recording (talks to daemon). Bind this to a global shortcut.
    Toggle,
    /// Start recording (daemon)
    Start,
    /// Stop recording → ASR → paste (daemon)
    Stop,
    /// Daemon state: idle | recording | transcribing
    Status,
    /// Stop the daemon
    Quit,
    /// Install systemd --user service + desktop entries
    Install {
        /// Also try to enable the service now
        #[arg(long, default_value_t = true)]
        enable: bool,
    },
    /// Write / refresh default config
    InitConfig,
    /// Show / edit configuration (GUI by default)
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// Show auth / model / daemon status
    Whoami,
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Open the graphical settings window (default)
    Gui,
    /// Print current config (path + TOML)
    Show,
    /// Print config file path
    Path,
    /// Open config.toml in a text editor
    Edit,
    /// Set one key: xai-dict config set dual_preedit false
    Set {
        key: String,
        value: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cli.log)),
        )
        .with_writer(std::io::stderr)
        .init();

    let mut cfg = Config::load();
    let _ = cfg.write_default_if_missing();

    if let Some(p) = cli.provider {
        cfg.provider = p.into();
    }
    if let Some(lang) = &cli.language {
        cfg.language = lang.clone();
    }
    if cli.no_paste {
        cfg.paste = false;
    }
    if let Some(p) = &cli.proxy {
        cfg.proxy = p.clone();
    }
    if let Some(m) = &cli.model {
        cfg.local_model = m.clone();
    }
    if let Some(d) = &cli.qwen3_dir {
        cfg.qwen3_model_dir = d.clone();
    }

    match cli.command.unwrap_or(Commands::Dict { max_secs: 0 }) {
        Commands::InitConfig => {
            let def = Config::default();
            def.save()?;
            println!("wrote {}", Config::config_path().display());
            Ok(())
        }
        Commands::Config { action } => match action.unwrap_or(ConfigCmd::Gui) {
            ConfigCmd::Gui => settings::run_gui(),
            ConfigCmd::Show => {
                settings::print_config(&cfg);
                Ok(())
            }
            ConfigCmd::Path => {
                println!("{}", Config::config_path().display());
                Ok(())
            }
            ConfigCmd::Edit => settings::open_editor(&Config::config_path()),
            ConfigCmd::Set { key, value } => settings::set_key(&key, &value),
        },
        Commands::Whoami => {
            println!("provider: {:?}", cfg.provider);
            println!("config: {}", Config::config_path().display());
            println!("socket: {}", daemon::socket_path().display());
            match daemon::send_raw("STATUS").await {
                Ok(s) => println!("daemon: {s}"),
                Err(_) => println!("daemon: not running"),
            }
            match cfg.provider {
                Provider::Qwen3 => {
                    let d = std::path::Path::new(&cfg.qwen3_model_dir);
                    println!("qwen3 model dir: {}", d.display());
                    println!("model ready: {}", local_qwen3::model_ready(d));
                    println!("threads: {}", cfg.local_threads);
                    println!(
                        "sherpa-onnx-offline: {}",
                        which("sherpa-onnx-offline").unwrap_or_else(|| "NOT FOUND".into())
                    );
                }
                Provider::Local => {
                    let m = std::path::Path::new(&cfg.local_model);
                    println!("local model: {}", m.display());
                    println!("model exists: {}", m.is_file());
                    println!("language: {}", cfg.language);
                    println!(
                        "whisper-cli: {}",
                        which("whisper-cli").unwrap_or_else(|| "NOT FOUND".into())
                    );
                }
                Provider::Xai => {
                    let token = auth::resolve_bearer(cli.api_key.as_deref())?;
                    println!("token length: {}", token.len());
                    let proxy = proxy::resolve_http_proxy(if cfg.proxy.is_empty() {
                        None
                    } else {
                        Some(cfg.proxy.as_str())
                    });
                    println!("proxy: {}", proxy.as_deref().unwrap_or("(none)"));
                }
            }
            Ok(())
        }
        Commands::Daemon => daemon::run(cfg).await,
        Commands::Toggle => daemon::client_cmd("TOGGLE").await,
        Commands::Start => daemon::client_cmd("START").await,
        Commands::Stop => daemon::client_cmd("STOP").await,
        Commands::Status => daemon::client_cmd("STATUS").await,
        Commands::Quit => daemon::client_cmd("QUIT").await,
        Commands::Install { enable } => install_service(enable),
        Commands::Dict { max_secs } => match cfg.provider {
            Provider::Qwen3 | Provider::Local => run_oneshot(cfg, max_secs).await,
            Provider::Xai if cli.stream => run_stream(cfg, cli.api_key.as_deref(), max_secs).await,
            Provider::Xai => run_xai_batch(cfg, cli.api_key.as_deref(), max_secs).await,
        },
    }
}

async fn run_oneshot(cfg: Config, max_secs: u64) -> Result<()> {
    let label = match cfg.provider {
        Provider::Qwen3 => format!(
            "Qwen3-ASR ({})",
            std::path::Path::new(&cfg.qwen3_model_dir)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("qwen3")
        ),
        Provider::Local => format!(
            "whisper ({})",
            std::path::Path::new(&cfg.local_model)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("model")
        ),
        Provider::Xai => "xAI".into(),
    };
    eprintln!("xai-dict: {label} — speak, then Enter / Ctrl+C");

    let live = pipeline::LiveCapture::start(cfg.sample_rate)?;
    wait_for_stop(max_secs).await;
    eprintln!("xai-dict: stopping capture…");
    let pcm = live.finish().await?;

    let wav = pipeline::pcm_to_temp_wav(cfg.sample_rate, &pcm)?;
    let secs = pcm.len() as f64 / (cfg.sample_rate as f64 * 2.0);
    eprintln!("xai-dict: transcribing {secs:.1}s…");

    let text = pipeline::transcribe_wav(&cfg, &wav).await;
    let _ = std::fs::remove_file(&wav);
    let text = text?;

    if text.is_empty() {
        anyhow::bail!("empty transcript (try speaking longer / closer to mic)");
    }
    output::deliver_text(&text, cfg.paste)?;
    Ok(())
}

async fn run_xai_batch(cfg: Config, api_key: Option<&str>, max_secs: u64) -> Result<()> {
    let _bearer = auth::resolve_bearer(api_key)?;
    eprintln!("xai-dict: recording (xAI cloud)… speak, then Enter / Ctrl+C");

    let live = pipeline::LiveCapture::start(cfg.sample_rate)?;
    wait_for_stop(max_secs).await;
    eprintln!("xai-dict: stopping capture…");
    let pcm = live.finish().await?;

    let wav = pipeline::pcm_to_temp_wav(cfg.sample_rate, &pcm)?;
    let proxy_hint = proxy::resolve_http_proxy(if cfg.proxy.is_empty() {
        None
    } else {
        Some(cfg.proxy.as_str())
    });
    eprintln!(
        "xai-dict: uploading {:.1}s to STT{}…",
        pcm.len() as f64 / (cfg.sample_rate as f64 * 2.0),
        proxy_hint
            .as_ref()
            .map(|p| format!(" via {p}"))
            .unwrap_or_else(|| " (direct)".into())
    );

    let text = pipeline::transcribe_wav(&cfg, &wav).await;
    let _ = std::fs::remove_file(&wav);
    let text = text?;
    if text.is_empty() {
        anyhow::bail!("empty transcript");
    }
    output::deliver_text(&text, cfg.paste)?;
    Ok(())
}

async fn run_stream(cfg: Config, api_key: Option<&str>, max_secs: u64) -> Result<()> {
    let bearer = auth::resolve_bearer(api_key)?;
    eprintln!("xai-dict: connecting streaming STT…");
    let mut stt = SttSession::connect(&cfg, &bearer)
        .await
        .context("connect streaming STT")?;
    eprintln!("xai-dict: listening…");

    let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<u8>>(64);
    let capture = capture::spawn_pcm_capture(cfg.sample_rate, pcm_tx)?;
    let audio_tx = stt.audio_sender().context("STT audio channel missing")?;
    let forward = tokio::spawn(async move {
        while let Some(chunk) = pcm_rx.recv().await {
            if audio_tx.send(chunk).await.is_err() {
                break;
            }
        }
    });

    let mut latest = String::new();
    let mut finals: Vec<String> = Vec::new();
    let stop = wait_for_stop(max_secs);
    tokio::pin!(stop);

    loop {
        tokio::select! {
            _ = &mut stop => break,
            ev = stt.recv() => match ev {
                Some(SttEvent::Partial { text, is_final, speech_final }) => {
                    if !text.is_empty() {
                        eprint!("\r\x1b[2K{text}");
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                        latest = text.clone();
                    }
                    if is_final && speech_final && !text.is_empty() {
                        finals.push(text);
                        latest.clear();
                        eprintln!();
                    }
                }
                Some(SttEvent::Done { text }) => {
                    if !text.is_empty() { latest = text; }
                    break;
                }
                Some(SttEvent::Error { message }) => {
                    eprintln!("\nSTT error: {message}");
                    break;
                }
                Some(SttEvent::Ready) | None => break,
            }
        }
    }

    capture.stop();
    forward.abort();
    stt.finish_audio();

    let mut parts = finals;
    if !latest.trim().is_empty() {
        parts.push(latest);
    }
    parts.dedup();
    let transcript = parts
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if transcript.is_empty() {
        anyhow::bail!("empty transcript");
    }
    output::deliver_text(&transcript, cfg.paste)?;
    Ok(())
}

async fn wait_for_stop(max_secs: u64) {
    use std::io::IsTerminal;

    let stdin_is_tty = std::io::stdin().is_terminal();
    let enter = async {
        if !stdin_is_tty {
            std::future::pending::<()>().await;
            return;
        }
        let _ = tokio::task::spawn_blocking(|| {
            let mut l = String::new();
            let _ = std::io::stdin().read_line(&mut l);
        })
        .await;
    };
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    let max_secs = if !stdin_is_tty && max_secs == 0 {
        8
    } else {
        max_secs
    };

    if max_secs > 0 {
        tokio::select! {
            _ = enter => {}
            _ = ctrl_c => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(max_secs)) => {
                eprintln!("xai-dict: max_secs reached");
            }
        }
    } else {
        tokio::select! {
            _ = enter => {}
            _ = ctrl_c => {}
        }
    }
}

fn install_service(enable: bool) -> Result<()> {
    let home = dirs::home_dir().context("HOME")?;
    let cargo_bin = home.join(".cargo/bin/xai-dict");
    let bin = if cargo_bin.is_file() {
        cargo_bin
    } else {
        which("xai-dict")
            .map(std::path::PathBuf::from)
            .context("xai-dict not on PATH — run: cargo install --path .")?
    };
    let bin_s = bin.display().to_string();

    // systemd user unit
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit = unit_dir.join("xai-dict.service");
    // Note: do not use PartOf=graphical-session.target together with
    // WantedBy=default.target in ways that fight session lifecycle.
    // ydotoold must NOT use After=default.target or we get an ordering cycle:
    // default → xai-dict → ydotoold → default
    let unit_body = format!(
        r#"[Unit]
Description=xai-dict voice dictation daemon (Lazy-style)
After=pipewire.service graphical-session.target ydotoold.service
Wants=ydotoold.service

[Service]
Type=simple
ExecStart={bin_s} daemon
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info
Environment=YDOTOOL_SOCKET=%t/.ydotool_socket

[Install]
WantedBy=default.target
"#
    );
    std::fs::write(&unit, unit_body)?;
    println!("wrote {}", unit.display());

    // Desktop entries
    let apps = home.join(".local/share/applications");
    std::fs::create_dir_all(&apps)?;

    let toggle_desktop = apps.join("xai-dict-toggle.desktop");
    std::fs::write(
        &toggle_desktop,
        format!(
            r#"[Desktop Entry]
Name=xai-dict Toggle
Comment=Start/stop voice dictation (bind a global shortcut to this)
Exec={bin_s} toggle
Icon=audio-input-microphone
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
StartupNotify=false
"#
        ),
    )?;
    println!("wrote {}", toggle_desktop.display());

    // Install OSD script for Lazy-style bottom bar
    let data = home.join(".local/share/xai-dict");
    std::fs::create_dir_all(&data)?;
    let osd_py = data.join("osd_bar.py");
    std::fs::write(&osd_py, include_str!("../scripts/osd_bar.py"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&osd_py, std::fs::Permissions::from_mode(0o755));
    }
    println!("wrote {}", osd_py.display());

    // Settings GUI script
    let settings_py = settings::install_settings_script()?;
    println!("wrote {}", settings_py.display());

    let app_desktop = apps.join("xai-dict.desktop");
    std::fs::write(
        &app_desktop,
        format!(
            r#"[Desktop Entry]
Name=xai-dict
Comment=Voice dictation (Qwen3-ASR / Whisper / xAI)
Exec={bin_s} whoami
Icon=audio-input-microphone
Terminal=true
Type=Application
Categories=Utility;AudioVideo;
"#
        ),
    )?;
    println!("wrote {}", app_desktop.display());

    let settings_desktop = apps.join("xai-dict-settings.desktop");
    std::fs::write(
        &settings_desktop,
        format!(
            r#"[Desktop Entry]
Name=xai-dict 设置
Name[en]=xai-dict Settings
Comment=Configure voice dictation (models, hotkey, near-field, streaming)
Exec={bin_s} config gui
Icon=preferences-desktop-multimedia
Terminal=false
Type=Application
Categories=Settings;Utility;AudioVideo;
StartupNotify=true
"#
        ),
    )?;
    println!("wrote {}", settings_desktop.display());

    // Update desktop database if available
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&apps)
        .status();

    if enable {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let status = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "xai-dict.service"])
            .status()
            .context("systemctl enable")?;
        if status.success() {
            println!("enabled & started: systemctl --user status xai-dict");
        } else {
            eprintln!("systemctl enable failed — start manually: xai-dict daemon");
        }
    }

    // Ensure config has hotkey = rightalt
    let mut cfg = Config::load();
    if cfg.hotkey.is_empty() {
        cfg.hotkey = "rightalt".into();
        let _ = cfg.save();
    }

    // Ensure ydotool daemon for Wayland text inject.
    // Never use After=default.target here — with xai-dict After=ydotoold + both
    // WantedBy=default.target, systemd detects an ordering cycle and drops xai-dict.
    let ydotool_unit = home.join(".config/systemd/user/ydotoold.service");
    if let Some(parent) = ydotool_unit.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &ydotool_unit,
        r#"[Unit]
Description=ydotool daemon (uinput keyboard automation for xai-dict)

[Service]
Type=simple
ExecStart=/usr/bin/ydotoold --socket-path=%t/.ydotool_socket --socket-perm=0600
Restart=on-failure
RestartSec=1

[Install]
WantedBy=default.target
"#,
    );
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "ydotoold.service"])
        .status();
    println!("wrote & ensured {}", ydotool_unit.display());

    println!(
        r#"
=== 全局热键（已内置）===
  右 Alt 点按 → 开始/结束录音
  识别结果会自动填入当前输入框（ydotool）

配置: ~/.config/xai-dict/config.toml
  hotkey = "rightalt"
  paste = true

调试:
  xai-dict status
  journalctl --user -u xai-dict -f
  systemctl --user status ydotoold
"#
    );
    Ok(())
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
