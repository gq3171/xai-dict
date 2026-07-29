mod auth;
mod capture;
mod config;
mod daemon;
mod hotkey;
mod install_svc;
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
        /// Also build & install fcitx5 Module (Super+V / F9, coexists with Pinyin)
        #[arg(long, default_value_t = false)]
        fcitx: bool,
    },
    /// Ensure user service is enabled/started (for deb postinst / login autostart)
    Ensure {
        /// Do not print noise
        #[arg(long, default_value_t = false)]
        quiet: bool,
    },
    /// Test microphone levels for ~secs (peak / rms)
    MicTest {
        /// Capture duration in seconds
        #[arg(long, default_value_t = 3.0)]
        secs: f32,
        /// Optional PipeWire/Pulse/ALSA device (else config `input_device`)
        #[arg(long)]
        device: Option<String>,
    },
    /// List capture sources (pactl / wpctl)
    MicList,
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
        Commands::Install { enable, fcitx } => install_svc::install_all(enable, fcitx),
        Commands::Ensure { quiet } => {
            if quiet {
                install_svc::ensure_running_quiet()
            } else {
                install_svc::ensure_user_service(true)?;
                println!("ok — bin={}", install_svc::resolve_bin()?.display());
                Ok(())
            }
        }
        Commands::MicTest { secs, device } => {
            let dev = device
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    let d = cfg.input_device.trim();
                    if d.is_empty() {
                        None
                    } else {
                        Some(d)
                    }
                });
            println!(
                "mic test: {:.1}s @ {} Hz, device={}",
                secs,
                cfg.sample_rate,
                dev.unwrap_or("(default)")
            );
            let (peak, rms, bytes) =
                capture::measure_levels(cfg.sample_rate, dev, secs)?;
            let secs_got = bytes as f64 / (cfg.sample_rate as f64 * 2.0);
            println!("captured: {secs_got:.2}s  peak={peak}  rms={rms:.0}");
            let hint = if peak < 200 {
                "信号过弱 — 检查静音/输入设备，或改用内置麦；可设 near_field=false 再试"
            } else if peak < 1500 {
                "偏轻但可用 — 近讲模式可开；USB 麦可调大系统输入音量"
            } else if peak > 28000 {
                "很响 — 注意削波；可降低系统输入增益"
            } else {
                "电平正常"
            };
            println!("评估: {hint}");
            Ok(())
        }
        Commands::MicList => {
            let devices = capture::list_input_devices();
            if devices.is_empty() {
                println!("(未列出设备 — 安装 pactl 或 wpctl，或保持 input_device 为空用默认)");
            } else {
                println!("capture sources:");
                for d in devices {
                    println!("  {d}");
                }
                println!("\n写入配置: xai-dict config set input_device '<name>'");
            }
            Ok(())
        }
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
