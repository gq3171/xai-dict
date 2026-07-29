//! Background daemon: global hotkey + Unix socket control.
//!
//! - **toggle** mode: press hotkey → start / stop
//! - **ptt** mode: hold hotkey → speak; release → finalize
//! - While recording with `stream = true`, phrases are ASR'd and committed live
//! - Also: `xai-dict toggle` / `start` / `stop` over the socket

use crate::config::Config;
use crate::hotkey::{self, HotkeyEvent};
use crate::notify;
use crate::output;
use crate::pipeline::{self, DualEvent, LiveCapture};
use crate::stream_vad::{SpeechSegment, VadConfig};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Transcribing,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Recording => "recording",
            State::Transcribing => "transcribing",
        }
    }
}

enum SessionCmd {
    Toggle {
        /// Reply with state **after** the toggle is applied (if provided).
        reply: Option<tokio::sync::oneshot::Sender<State>>,
    },
    Start,
    Stop,
    Status {
        reply: tokio::sync::oneshot::Sender<State>,
    },
    Quit,
}

/// ASR job result tagged with session id so late finishes cannot clobber a new session.
struct AsrDone {
    session: u64,
    result: std::result::Result<String, String>,
}

struct StreamOutcome {
    /// Text already committed during live streaming.
    committed: String,
    phrases: u32,
}

struct RecordingSession {
    capture: LiveCapture,
    started: std::time::Instant,
    /// Background type-as-you-speak worker (if streaming).
    stream_join: Option<JoinHandle<StreamOutcome>>,
    streaming: bool,
}

/// Ignore stop if recording shorter than this (accidental double-press).
const DEBOUNCE_MS: u64 = 400;

/// Socket path: `$XDG_RUNTIME_DIR/xai-dict.sock`
pub fn socket_path() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join("xai-dict.sock")
    } else {
        PathBuf::from(format!("/tmp/xai-dict-{}.sock", nix_uid()))
    }
}

fn nix_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|u| u.parse().ok())
        })
        .unwrap_or(1000)
}

pub fn pid_path() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join("xai-dict.pid")
    } else {
        PathBuf::from(format!("/tmp/xai-dict-{}.pid", nix_uid()))
    }
}

/// Run the long-lived daemon.
pub async fn run(cfg: Config) -> Result<()> {
    let sock = socket_path();
    if sock.exists() {
        match send_raw("STATUS").await {
            Ok(reply) if reply.starts_with("OK") => {
                bail!(
                    "daemon already running (socket {} → {reply}).\n\
                     Use: xai-dict toggle | xai-dict status | xai-dict quit",
                    sock.display()
                );
            }
            _ => {
                let _ = std::fs::remove_file(&sock);
            }
        }
    }

    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("bind {}", sock.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    }
    let _ = std::fs::write(pid_path(), format!("{}\n", std::process::id()));

    let ptt = cfg.is_ptt();
    let (hk_tx, mut hk_rx) = mpsc::unbounded_channel::<HotkeyEvent>();
    let hk_stop = if let Some(key) = hotkey::parse_key(&cfg.hotkey) {
        let label = hotkey::key_label(key);
        let mode = if ptt { "ptt (hold)" } else { "toggle" };
        tracing::info!(key = ?key, ptt, "enabling global hotkey");
        let stream_hint = if cfg.stream { " · 流式上屏" } else { "" };
        let usage = if ptt {
            format!("按住 {label} 说话，松手定稿{stream_hint}")
        } else {
            format!("按 {label} 开始/结束{stream_hint}")
        };
        eprintln!(
            "xai-dict daemon: hotkey = {label} ({mode})\n  {usage}\n  socket {}",
            sock.display()
        );
        notify::idle(&format!("已启动 · {usage}"));
        Some(hotkey::spawn_listener(key, ptt, hk_tx))
    } else {
        eprintln!(
            "xai-dict daemon: hotkey disabled\n  socket {} — use: xai-dict toggle",
            sock.display()
        );
        notify::idle("已启动 · 热键关闭，用 xai-dict toggle");
        None
    };

    tracing::info!(
        path = %sock.display(),
        stream = cfg.stream,
        ptt,
        input_device = %cfg.input_device,
        "xai-dict daemon listening"
    );

    // Preload models at daemon start (warm residents).
    if matches!(cfg.provider, crate::config::Provider::Qwen3) {
        let dir = std::path::PathBuf::from(&cfg.qwen3_model_dir);
        let threads = cfg.local_threads;
        let max_tok = cfg.qwen3_max_new_tokens;
        let hotwords = cfg.qwen3_hotwords.clone();
        tokio::task::spawn_blocking(move || {
            match crate::local_qwen3::ensure_warm(&dir, threads, max_tok, &hotwords) {
                Ok(()) => {
                    tracing::info!("qwen3 warm worker preloaded");
                    eprintln!("xai-dict: Qwen3 模型已常驻（定稿 ~0.5s/句）");
                }
                Err(e) => {
                    tracing::warn!("qwen3 warm preload failed (will cold-start): {e:#}");
                }
            }
        });
    }
    if cfg.dual_model && cfg.stream {
        let dir = std::path::PathBuf::from(&cfg.stream_model_dir);
        let thr = cfg.stream_threads;
        let sr = cfg.sample_rate;
        if crate::local_stream::model_ready(&dir) {
            tokio::task::spawn_blocking(move || {
                match crate::local_stream::ensure_warm(&dir, thr, sr) {
                    Ok(()) => {
                        tracing::info!("zipformer warm worker preloaded");
                        eprintln!("xai-dict: 流式预编辑模型已常驻（Paraformer/Zipformer）");
                    }
                    Err(e) => tracing::warn!("zipformer preload failed: {e:#}"),
                }
            });
        } else {
            tracing::warn!(
                path = %dir.display(),
                "dual_model on but stream model missing — phrase-only mode"
            );
        }
    }

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    let state = Arc::new(Mutex::new(State::Idle));

    let cmd_tx_accept = cmd_tx.clone();
    let state_accept = state.clone();
    let accept = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = cmd_tx_accept.clone();
                    let st = state_accept.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, tx, st).await {
                            tracing::debug!("client error: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    break;
                }
            }
        }
    });

    let mut recording: Option<RecordingSession> = None;

    let (asr_done_tx, mut asr_done_rx) = mpsc::unbounded_channel::<AsrDone>();
    // Monotonic id for each stop→ASR job; only the matching finish may leave Transcribing.
    let mut asr_session: u64 = 0;
    let mut active_asr_session: u64 = 0;

    let max_secs: u64 = std::env::var("XAI_DICT_MAX_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);

    const DISARM: std::time::Duration = std::time::Duration::from_secs(86400 * 30);
    let mut armed = false;
    let auto_stop = tokio::time::sleep(DISARM);
    tokio::pin!(auto_stop);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("xai-dict daemon: Ctrl+C, shutting down");
                break;
            }
            _ = &mut auto_stop => {
                if armed && recording.is_some() {
                    tracing::info!("auto-stop after {max_secs}s");
                    armed = false;
                    let _ = cmd_tx.send(SessionCmd::Stop);
                }
                auto_stop.as_mut().reset(tokio::time::Instant::now() + DISARM);
            }
            Some(ev) = hk_rx.recv() => {
                if ptt {
                    match ev {
                        HotkeyEvent::Press => {
                            let _ = cmd_tx.send(SessionCmd::Start);
                        }
                        HotkeyEvent::Release => {
                            // PTT: always honor release (no short-press debounce).
                            let _ = cmd_tx.send(SessionCmd::Stop);
                        }
                    }
                } else if matches!(ev, HotkeyEvent::Press) {
                    let _ = cmd_tx.send(SessionCmd::Toggle { reply: None });
                }
            }
            Some(done) = asr_done_rx.recv() => {
                let mut st = state.lock().await;
                if *st != State::Transcribing || done.session != active_asr_session {
                    tracing::debug!(
                        session = done.session,
                        active = active_asr_session,
                        state = st.as_str(),
                        "ignoring stale ASR result"
                    );
                    continue;
                }
                *st = State::Idle;
                drop(st);
                match done.result {
                    Ok(text) => {
                        tracing::info!(n = text.len(), session = done.session, "delivered");
                        if text.is_empty() {
                            notify::idle("未识别到语音");
                        } else {
                            notify::done(&text);
                        }
                    }
                    Err(e) => {
                        tracing::error!(session = done.session, "asr: {e}");
                        notify::error(&e);
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    SessionCmd::Quit => {
                        eprintln!("xai-dict daemon: quit requested");
                        break;
                    }
                    SessionCmd::Status { reply } => {
                        let s = *state.lock().await;
                        let _ = reply.send(s);
                    }
                    SessionCmd::Start => {
                        try_start(
                            &cfg,
                            &state,
                            &mut recording,
                            &mut armed,
                            &mut auto_stop,
                            max_secs,
                        )
                        .await;
                    }
                    SessionCmd::Stop => {
                        try_stop(
                            &cfg,
                            &state,
                            &mut recording,
                            &mut armed,
                            &mut auto_stop,
                            &mut asr_session,
                            &mut active_asr_session,
                            asr_done_tx.clone(),
                            false,
                        )
                        .await;
                    }
                    SessionCmd::Toggle { reply } => {
                        let s = *state.lock().await;
                        match s {
                            State::Idle => {
                                try_start(
                                    &cfg,
                                    &state,
                                    &mut recording,
                                    &mut armed,
                                    &mut auto_stop,
                                    max_secs,
                                )
                                .await;
                            }
                            State::Recording => {
                                try_stop(
                                    &cfg,
                                    &state,
                                    &mut recording,
                                    &mut armed,
                                    &mut auto_stop,
                                    &mut asr_session,
                                    &mut active_asr_session,
                                    asr_done_tx.clone(),
                                    true,
                                )
                                .await;
                            }
                            State::Transcribing => {
                                // Already finishing — do not spam another toast.
                                tracing::debug!("toggle ignored (transcribing)");
                            }
                        }
                        if let Some(r) = reply {
                            let s = *state.lock().await;
                            let _ = r.send(s);
                        }
                    }
                }
            }
        }
    }

    if let Some(flag) = hk_stop {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    accept.abort();
    if let Some(sess) = recording.take() {
        let _ = sess.capture.finish().await;
        if let Some(j) = sess.stream_join {
            j.abort();
        }
    }
    let _ = std::fs::remove_file(socket_path());
    let _ = std::fs::remove_file(pid_path());
    *state.lock().await = State::Idle;
    Ok(())
}

async fn try_start(
    cfg: &Config,
    state: &Arc<Mutex<State>>,
    recording: &mut Option<RecordingSession>,
    armed: &mut bool,
    auto_stop: &mut std::pin::Pin<&mut tokio::time::Sleep>,
    max_secs: u64,
) {
    if *state.lock().await != State::Idle {
        return;
    }
    match start_recording(cfg).await {
        Ok(sess) => {
            *state.lock().await = State::Recording;
            *recording = Some(sess);
            notify::recording();
            *armed = true;
            auto_stop.as_mut().reset(
                tokio::time::Instant::now() + std::time::Duration::from_secs(max_secs),
            );
            tracing::info!(stream = cfg.stream, "recording started");
        }
        Err(e) => {
            tracing::error!("start: {e:#}");
            notify::error(&format!("{e:#}"));
        }
    }
}

async fn try_stop(
    cfg: &Config,
    state: &Arc<Mutex<State>>,
    recording: &mut Option<RecordingSession>,
    armed: &mut bool,
    auto_stop: &mut std::pin::Pin<&mut tokio::time::Sleep>,
    asr_session: &mut u64,
    active_asr_session: &mut u64,
    asr_done_tx: mpsc::UnboundedSender<AsrDone>,
    honor_debounce: bool,
) {
    let Some(sess) = recording.take() else {
        return;
    };
    let elapsed_ms = sess.started.elapsed().as_millis() as u64;
    if honor_debounce && elapsed_ms < DEBOUNCE_MS {
        tracing::info!(elapsed_ms, "stop ignored (debounce) — put session back");
        *recording = Some(sess);
        return;
    }

    *armed = false;
    const DISARM: std::time::Duration = std::time::Duration::from_secs(86400 * 30);
    auto_stop
        .as_mut()
        .reset(tokio::time::Instant::now() + DISARM);

    *asr_session = asr_session.wrapping_add(1);
    let session = *asr_session;
    *active_asr_session = session;

    *state.lock().await = State::Transcribing;
    // Streaming path often finishes in <50ms (phrases already on screen).
    // Still show "识别中" so the sticky "录音中" card is replaced immediately.
    notify::transcribing();
    tracing::info!(
        elapsed_ms,
        streaming = sess.streaming,
        session,
        "recording stopped"
    );

    let cfg = cfg.clone();
    tokio::spawn(async move {
        let result = finish_and_deliver(&cfg, sess)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = asr_done_tx.send(AsrDone { session, result });
    });
}

async fn start_recording(cfg: &Config) -> Result<RecordingSession> {
    match cfg.provider {
        crate::config::Provider::Qwen3 => {
            let d = std::path::Path::new(&cfg.qwen3_model_dir);
            if !crate::local_qwen3::model_ready(d) {
                bail!("Qwen3 model not ready: {}", d.display());
            }
        }
        crate::config::Provider::Local => {
            if !std::path::Path::new(&cfg.local_model).is_file() {
                bail!("whisper model not found: {}", cfg.local_model);
            }
        }
        crate::config::Provider::Xai => {}
    }

    let use_stream = cfg.stream
        && matches!(
            cfg.provider,
            crate::config::Provider::Qwen3 | crate::config::Provider::Local
        );

    let dual = use_stream
        && cfg.dual_model
        && crate::local_stream::model_ready(std::path::Path::new(&cfg.stream_model_dir));

    // Near-field defaults: higher absolute RMS + SNR vs ambient so bystanders don't trigger.
    let speech_rms = cfg.effective_vad_speech_rms();
    let snr = cfg.effective_vad_snr();
    let min_peak = cfg.effective_min_segment_peak();
    let vad = VadConfig {
        sample_rate: cfg.sample_rate,
        frame_ms: 30,
        speech_rms,
        min_silence_ms: cfg.stream_min_silence_ms,
        min_speech_ms: cfg.stream_min_speech_ms.max(if cfg.near_field { 300 } else { 250 }),
        max_segment_ms: cfg.stream_max_segment_ms,
        snr_ratio: snr,
        min_segment_peak: min_peak,
        min_segment_rms: if cfg.near_field { 280.0 } else { 150.0 },
    };
    let tune = pipeline::CaptureTune {
        agc_max_gain: cfg.effective_agc_max_gain(),
        // Always stream continuous PCM to Paraformer; VAD alone rejects bystanders.
        preedit_min_rms: 0.0,
    };
    tracing::info!(
        near_field = cfg.near_field,
        speech_rms,
        snr,
        min_peak,
        agc_max = tune.agc_max_gain,
        preedit_min_rms = tune.preedit_min_rms,
        "capture gates"
    );

    let device = {
        let d = cfg.input_device.trim();
        if d.is_empty() {
            None
        } else {
            Some(d)
        }
    };

    let mut capture = if dual {
        LiveCapture::start_dual(cfg.sample_rate, device, vad, tune)?
    } else if use_stream {
        LiveCapture::start_streaming(cfg.sample_rate, device, vad, tune)?
    } else {
        LiveCapture::start_with_device(cfg.sample_rate, device)?
    };

    let stream_join = if dual {
        let events = capture
            .take_dual_rx()
            .expect("dual capture must expose dual_rx");
        let cfg_s = cfg.clone();
        Some(tokio::spawn(async move { dual_worker(cfg_s, events).await }))
    } else if use_stream {
        let rx = capture
            .take_segment_rx()
            .expect("streaming capture must expose segment_rx");
        let cfg_s = cfg.clone();
        Some(tokio::spawn(async move { stream_worker(cfg_s, rx).await }))
    } else {
        None
    };

    Ok(RecordingSession {
        capture,
        started: std::time::Instant::now(),
        stream_join,
        streaming: use_stream,
    })
}

/// Dual-model live path (ordered events: Chunk then Segment for each phrase):
/// - Stream model: continuous partials → fcitx Preedit (provisional, filtered)
/// - Qwen3: on each VAD phrase → Commit final (source of truth)
async fn dual_worker(
    cfg: Config,
    mut events: mpsc::UnboundedReceiver<DualEvent>,
) -> StreamOutcome {
    let mut committed = String::new();
    let mut phrases = 0u32;
    let mut dropped = 0u32;
    let mut stream_restarts = 0u32;
    // Latest raw stream decode (may be wild); used only for short fallback.
    let mut last_partial = String::new();
    // What we actually pushed to fcitx preedit (stability-gated).
    let mut shown_preedit = String::new();
    let mut pending_preedit = String::new();
    let mut pending_hits: u8 = 0;
    let show_preedit = cfg.dual_preedit;
    let session_t0 = std::time::Instant::now();
    let mut first_preedit_ms: Option<u64> = None;

    let sdir = std::path::PathBuf::from(&cfg.stream_model_dir);
    let qdir = std::path::PathBuf::from(&cfg.qwen3_model_dir);
    let thr = cfg.stream_threads;
    let sr = cfg.sample_rate;
    let qthr = cfg.local_threads;
    let qtok = cfg.qwen3_max_new_tokens;
    let hot = cfg.qwen3_hotwords.clone();
    let sdir_w = sdir.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = crate::local_stream::ensure_warm(&sdir_w, thr, sr);
        let _ = crate::local_qwen3::ensure_warm(&qdir, qthr, qtok, &hot);
    })
    .await;

    let start_ok = tokio::task::spawn_blocking(crate::local_stream::start_utterance)
        .await
        .unwrap_or(Err(anyhow::anyhow!("join")));
    if let Err(e) = start_ok {
        tracing::warn!("stream START failed: {e:#} — trying recover once");
        let sdir2 = sdir.clone();
        let recovered = tokio::task::spawn_blocking(move || {
            crate::local_stream::recover(&sdir2, thr, sr)
        })
        .await
        .unwrap_or(Err(anyhow::anyhow!("join")));
        if let Err(e2) = recovered {
            tracing::warn!("stream recover failed: {e2:#} — phrase-only fallback");
            notify::error("流式模型异常，已降级为按句定稿");
            let (stx, srx) = mpsc::unbounded_channel();
            while let Some(ev) = events.recv().await {
                if let DualEvent::Segment(s) = ev {
                    let _ = stx.send(s);
                }
            }
            drop(stx);
            return stream_worker(cfg, srx).await;
        }
        stream_restarts += 1;
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    while let Some(ev) = events.recv().await {
        match ev {
            DualEvent::Chunk(chunk) => {
                let chunk = chunk.clone();
                let res = tokio::task::spawn_blocking(move || crate::local_stream::feed_pcm(&chunk))
                    .await
                    .unwrap_or_else(|e| Err(anyhow::anyhow!("join: {e}")));
                match res {
                    Ok(partial) => {
                        let raw = partial.text.trim().to_string();
                        if raw == last_partial {
                            continue;
                        }
                        last_partial = raw.clone();
                        if !show_preedit {
                            continue;
                        }
                        // Ignore stream endpoint; VAD owns phrase cuts.
                        if let Some(to_show) = preedit_update(
                            &mut shown_preedit,
                            &mut pending_preedit,
                            &mut pending_hits,
                            &raw,
                        ) {
                            if first_preedit_ms.is_none() && !to_show.is_empty() {
                                first_preedit_ms = Some(session_t0.elapsed().as_millis() as u64);
                            }
                            let t = to_show;
                            let _ = tokio::task::spawn_blocking(move || output::set_preedit(&t))
                                .await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("stream feed: {e:#}");
                        dropped += 1;
                        // One-shot recover mid-utterance so preedit can resume.
                        if stream_restarts < 2 {
                            let sdir2 = sdir.clone();
                            let ok = tokio::task::spawn_blocking(move || {
                                crate::local_stream::recover(&sdir2, thr, sr)
                            })
                            .await
                            .unwrap_or(Err(anyhow::anyhow!("join")));
                            if ok.is_ok() {
                                stream_restarts += 1;
                                tracing::info!(stream_restarts, "stream worker recovered");
                            } else {
                                tracing::warn!("stream recover failed mid-session");
                            }
                        }
                    }
                }
            }
            DualEvent::Segment(seg) => {
                let secs = seg.pcm.len() as f64 / (cfg.sample_rate as f64 * 2.0);
                let zip_snapshot = last_partial.clone();
                let (peak, rms) = crate::wav::pcm16_levels(&seg.pcm);
                tracing::info!(
                    secs,
                    peak,
                    rms = format!("{rms:.0}"),
                    zip_n = zip_snapshot.chars().count(),
                    "dual: VAD phrase → Qwen3 finalize"
                );

                let t_asr = std::time::Instant::now();
                let qwen_text =
                    match pipeline::transcribe_pcm(&cfg, cfg.sample_rate, &seg.pcm).await {
                        Ok(t) => t.trim().to_string(),
                        Err(e) => {
                            tracing::debug!("Qwen3 finalize fail: {e:#}");
                            dropped += 1;
                            String::new()
                        }
                    };
                let qwen_ms = t_asr.elapsed().as_millis() as u64;

                let final_text = pick_dual_final(&qwen_text, &zip_snapshot, secs);
                last_partial.clear();
                shown_preedit.clear();
                pending_preedit.clear();
                pending_hits = 0;
                let _ = tokio::task::spawn_blocking(output::clear_preedit).await;

                if final_text.is_empty() {
                    tracing::info!(
                        secs,
                        qwen_ms,
                        peak,
                        "metric: phrase dropped (empty final)"
                    );
                    let _ = tokio::task::spawn_blocking(crate::local_stream::start_utterance).await;
                    continue;
                }

                let paste = cfg.paste;
                let text_c = final_text.clone();
                let t_commit = std::time::Instant::now();
                let committed_ok = tokio::task::spawn_blocking(move || {
                    output::commit_final(&text_c, paste)
                })
                .await
                .unwrap_or(Ok(false));
                let commit_ms = t_commit.elapsed().as_millis() as u64;

                match committed_ok {
                    Ok(true) => {
                        append_phrase(&mut committed, &final_text);
                        phrases += 1;
                        tracing::info!(
                            n = final_text.chars().count(),
                            phrases,
                            secs = format!("{secs:.2}"),
                            qwen_ms,
                            commit_ms,
                            peak,
                            "metric: phrase committed"
                        );
                    }
                    Ok(false) => {
                        append_phrase(&mut committed, &final_text);
                        phrases += 1;
                        tracing::warn!(
                            qwen_ms,
                            "dual: commit failed, text kept in report"
                        );
                    }
                    Err(e) => tracing::warn!("dual commit: {e:#}"),
                }

                let _ = tokio::task::spawn_blocking(crate::local_stream::start_utterance).await;
            }
        }
    }

    // Never commit raw stream leftover as "final" — that was a major source of
    // preedit/final mismatch. Short takes with no VAD phrase fall through to
    // full-buffer Qwen3 in finish_and_deliver.
    let _ = tokio::task::spawn_blocking(output::clear_preedit).await;
    let _ = tokio::task::spawn_blocking(crate::local_stream::finish_utterance).await;

    tracing::info!(
        phrases,
        dropped,
        stream_restarts,
        first_preedit_ms = first_preedit_ms.unwrap_or(0),
        session_ms = session_t0.elapsed().as_millis() as u64,
        "metric: dual session summary"
    );

    StreamOutcome {
        committed,
        phrases,
    }
}

/// Gate Zipformer partials so fcitx preedit doesn't thrash between unrelated guesses.
///
/// Accept immediately when text grows/shrinks as a prefix (normal streaming).
/// Require two identical consecutive reads before accepting a full rewrite.
fn preedit_update(
    shown: &mut String,
    pending: &mut String,
    pending_hits: &mut u8,
    new: &str,
) -> Option<String> {
    let new = new.trim();
    if new.is_empty() {
        return None;
    }
    if new == shown.as_str() {
        return None;
    }
    // Monotonic growth or small retraction — normal streaming decoder behavior.
    if shown.is_empty()
        || new.starts_with(shown.as_str())
        || (shown.starts_with(new)
            && shown.chars().count().saturating_sub(new.chars().count()) <= 6)
    {
        *shown = new.to_string();
        pending.clear();
        *pending_hits = 0;
        return Some(shown.clone());
    }
    // High overlap rewrite (e.g. one character correction mid-phrase).
    if char_similarity(shown, new) >= 0.65 {
        *shown = new.to_string();
        pending.clear();
        *pending_hits = 0;
        return Some(shown.clone());
    }
    // Drastic change: hold until Zipformer repeats the same guess twice.
    if new == pending.as_str() {
        *pending_hits = pending_hits.saturating_add(1);
    } else {
        *pending = new.to_string();
        *pending_hits = 1;
    }
    if *pending_hits >= 2 {
        *shown = new.to_string();
        pending.clear();
        *pending_hits = 0;
        return Some(shown.clone());
    }
    None
}

/// Qwen3 is source of truth. Zipformer only fills empty finals on short phrases.
fn pick_dual_final(qwen: &str, zip: &str, secs: f64) -> String {
    let q = qwen.trim();
    let z = zip.trim();
    if !q.is_empty() {
        if !z.is_empty() {
            let sim = char_similarity(q, z);
            if sim < 0.45 {
                tracing::info!(
                    sim = format!("{sim:.2}"),
                    qwen = %truncate_log(q, 40),
                    zip = %truncate_log(z, 40),
                    "dual: preedit≠final (using Qwen3)"
                );
            }
        }
        return q.to_string();
    }
    // Empty Qwen: only trust Zipformer on short, dense phrases (avoids long hallucinations).
    let zc = z.chars().count();
    let max_chars = ((secs * 6.0).ceil() as usize).clamp(4, 28);
    if secs <= 2.5 && zc >= 2 && zc <= max_chars {
        tracing::info!(n = zc, secs, "dual: short Zipformer fallback (Qwen3 empty)");
        return z.to_string();
    }
    if !z.is_empty() {
        tracing::debug!(
            n = zc,
            secs,
            "dual: discard Zipformer fallback (too long/noisy for empty Qwen3)"
        );
    }
    String::new()
}

fn char_similarity(a: &str, b: &str) -> f64 {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    if ac.is_empty() && bc.is_empty() {
        return 1.0;
    }
    if ac.is_empty() || bc.is_empty() {
        return 0.0;
    }
    // Dice on char bigrams — cheap and good enough for CJK strings.
    let bigrams = |s: &[char]| -> std::collections::HashMap<(char, char), u32> {
        let mut m = std::collections::HashMap::new();
        if s.len() == 1 {
            m.insert((s[0], '\0'), 1);
            return m;
        }
        for w in s.windows(2) {
            *m.entry((w[0], w[1])).or_insert(0) += 1;
        }
        m
    };
    let ba = bigrams(&ac);
    let bb = bigrams(&bc);
    let mut inter = 0u32;
    for (k, va) in &ba {
        if let Some(vb) = bb.get(k) {
            inter += (*va).min(*vb);
        }
    }
    let total = ba.values().sum::<u32>() + bb.values().sum::<u32>();
    if total == 0 {
        0.0
    } else {
        (2.0 * inter as f64) / total as f64
    }
}

fn truncate_log(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        s.to_string()
    } else {
        let t: String = s.chars().take(max_chars).collect();
        format!("{t}…")
    }
}

fn append_phrase(committed: &mut String, piece: &str) {
    if piece.is_empty() {
        return;
    }
    if !committed.is_empty() && needs_space_between(committed, piece) {
        committed.push(' ');
    }
    committed.push_str(piece);
}

/// Phrase-only (no stream preedit): each VAD phrase → Qwen3 → commit.
async fn stream_worker(
    cfg: Config,
    mut rx: mpsc::UnboundedReceiver<SpeechSegment>,
) -> StreamOutcome {
    let mut committed = String::new();
    let mut phrases = 0u32;
    let mut dropped = 0u32;
    let mut first = true;
    let session_t0 = std::time::Instant::now();

    while let Some(seg) = rx.recv().await {
        let secs = seg.pcm.len() as f64 / (cfg.sample_rate as f64 * 2.0);
        let (peak, rms) = crate::wav::pcm16_levels(&seg.pcm);
        tracing::info!(
            secs,
            bytes = seg.pcm.len(),
            peak,
            rms = format!("{rms:.0}"),
            "stream segment ready"
        );

        let t0 = std::time::Instant::now();
        match pipeline::transcribe_pcm(&cfg, cfg.sample_rate, &seg.pcm).await {
            Ok(text) => {
                let text = text.trim().to_string();
                let asr_ms = t0.elapsed().as_millis() as u64;
                if text.is_empty() {
                    dropped += 1;
                    tracing::info!(secs, asr_ms, peak, "metric: phrase empty");
                    continue;
                }
                let piece = text;
                match output::deliver_stream_chunk(&piece, cfg.paste, first) {
                    Ok(true) => {
                        append_phrase(&mut committed, &piece);
                        phrases += 1;
                        first = false;
                        tracing::info!(
                            n = piece.chars().count(),
                            phrases,
                            asr_ms,
                            secs = format!("{secs:.2}"),
                            peak,
                            "metric: phrase committed"
                        );
                    }
                    Ok(false) => {
                        tracing::warn!("stream phrase not delivered: {piece}");
                        append_phrase(&mut committed, &piece);
                        phrases += 1;
                    }
                    Err(e) => tracing::warn!("stream deliver: {e:#}"),
                }
            }
            Err(e) => {
                dropped += 1;
                tracing::debug!("stream ASR skip: {e:#}");
            }
        }
    }

    tracing::info!(
        phrases,
        dropped,
        session_ms = session_t0.elapsed().as_millis() as u64,
        "metric: phrase-only session summary"
    );

    StreamOutcome {
        committed,
        phrases,
    }
}

fn needs_space_between(a: &str, b: &str) -> bool {
    let Some(ac) = a.chars().last() else {
        return false;
    };
    let Some(bc) = b.chars().next() else {
        return false;
    };
    ac.is_ascii_alphanumeric() && bc.is_ascii_alphanumeric()
}

async fn finish_and_deliver(cfg: &Config, sess: RecordingSession) -> Result<String> {
    let streaming = sess.streaming;
    let stream_join = sess.stream_join;

    // Stop mic; collect task flushes final VAD segment into the stream worker.
    let pcm = sess.capture.finish().await?;
    let secs = pcm.len() as f64 / (cfg.sample_rate as f64 * 2.0);
    let (peak, rms) = crate::wav::pcm16_levels(&pcm);
    tracing::info!(
        secs,
        bytes = pcm.len(),
        peak,
        rms = format!("{rms:.0}"),
        streaming,
        "captured audio"
    );

    if streaming {
        // Wait for in-flight phrase ASR + final flushed segment.
        let (outcome, join_failed) = if let Some(j) = stream_join {
            match j.await {
                Ok(o) => (o, false),
                Err(e) => {
                    tracing::error!("stream worker join error: {e}");
                    // Do NOT fall back to full-buffer re-paste — live commits may
                    // already be on screen (would double-insert).
                    (
                        StreamOutcome {
                            committed: String::new(),
                            phrases: 0,
                        },
                        true,
                    )
                }
            }
        } else {
            (
                StreamOutcome {
                    committed: String::new(),
                    phrases: 0,
                },
                false,
            )
        };

        tracing::info!(
            phrases = outcome.phrases,
            n = outcome.committed.len(),
            join_failed,
            "stream session complete"
        );

        if join_failed {
            let _ = tokio::task::spawn_blocking(output::clear_preedit).await;
            if outcome.committed.is_empty() {
                bail!("识别任务异常结束（已避免重复上屏）");
            }
            return Ok(outcome.committed);
        }

        if outcome.committed.is_empty() {
            // No phrase committed — fall back to full-buffer ASR (with normalize).
            let min_half_sec = (cfg.sample_rate as usize) * 2 / 2;
            if pcm.len() < min_half_sec {
                bail!("录音过短 ({secs:.1}s)。说完后再按一次结束。");
            }
            if peak < 120 {
                bail!(
                    "麦克风几乎无信号 ({secs:.1}s, peak={peak}, rms={rms:.0})。\
                     请到系统设置→声音确认输入设备；当前 USB 麦无声时可改用内置麦克风"
                );
            }
            return finish_batch(cfg, &pcm, secs, peak, rms).await;
        }

        // Already on screen; just return combined text for OSD.
        return Ok(outcome.committed);
    }

    // Non-stream: classic full-buffer ASR.
    finish_batch(cfg, &pcm, secs, peak, rms).await
}

async fn finish_batch(
    cfg: &Config,
    pcm: &[u8],
    secs: f64,
    peak: i32,
    rms: f64,
) -> Result<String> {
    let pcm = {
        let max_bytes = cfg.sample_rate as usize * 2 * 60;
        if pcm.len() > max_bytes {
            tracing::warn!(secs, "truncating to last 60s for ASR");
            &pcm[pcm.len() - max_bytes..]
        } else {
            pcm
        }
    };

    let wav = pipeline::pcm_to_temp_wav(cfg.sample_rate, pcm)?;
    let text = pipeline::transcribe_wav(cfg, &wav).await;
    let _ = std::fs::remove_file(&wav);
    let text = text?;

    if text.is_empty() {
        if secs < 1.5 {
            bail!("录音过短 ({secs:.1}s)。说完后再按一次结束。");
        }
        if peak < 200 {
            bail!(
                "麦克风信号过弱 ({secs:.1}s, peak={peak}, rms={rms:.0})。\
                 请检查输入设备/静音，或换内置麦克风；调试: ~/.local/share/xai-dict/last.wav"
            );
        }
        bail!(
            "未识别到语音 ({secs:.1}s, peak={peak}, rms={rms:.0})。\
             请靠近麦克风多说几秒；调试: ~/.local/share/xai-dict/last.wav"
        );
    }

    output::deliver_text_quiet(&text, cfg.paste)?;
    Ok(text)
}

async fn handle_client(
    stream: UnixStream,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    state: Arc<Mutex<State>>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let line = match lines.next_line().await? {
        Some(l) => l.trim().to_ascii_uppercase(),
        None => return Ok(()),
    };

    let reply = match line.as_str() {
        "TOGGLE" => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx.send(SessionCmd::Toggle {
                reply: Some(tx),
            });
            match tokio::time::timeout(std::time::Duration::from_secs(3), rx).await {
                Ok(Ok(s)) => format!("OK {}\n", s.as_str()),
                _ => {
                    // Fallback: best-effort current state if toggle reply times out.
                    let s = *state.lock().await;
                    format!("OK {}\n", s.as_str())
                }
            }
        }
        "START" => {
            let _ = cmd_tx.send(SessionCmd::Start);
            "OK starting\n".into()
        }
        "STOP" => {
            let _ = cmd_tx.send(SessionCmd::Stop);
            "OK stopping\n".into()
        }
        "STATUS" => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx.send(SessionCmd::Status { reply: tx });
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx).await {
                Ok(Ok(s)) => format!("OK {}\n", s.as_str()),
                _ => {
                    let s = *state.lock().await;
                    format!("OK {}\n", s.as_str())
                }
            }
        }
        "QUIT" => {
            let _ = cmd_tx.send(SessionCmd::Quit);
            "OK quitting\n".into()
        }
        "PING" => "OK pong\n".into(),
        other => format!("ERR unknown command: {other}\n"),
    };

    writer.write_all(reply.as_bytes()).await?;
    writer.shutdown().await.ok();
    Ok(())
}

pub async fn send_raw(cmd: &str) -> Result<String> {
    let sock = socket_path();
    if !sock.exists() {
        bail!(
            "daemon not running (no {}).\n\
             Start with:  systemctl --user start xai-dict\n\
             or:          xai-dict daemon",
            sock.display()
        );
    }
    let mut stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connect {}", sock.display()))?;
    stream
        .write_all(format!("{cmd}\n").as_bytes())
        .await
        .context("write cmd")?;
    stream.shutdown().await.ok();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.context("read reply")?;
    Ok(line.trim().to_string())
}

pub async fn client_cmd(cmd: &str) -> Result<()> {
    let reply = send_raw(cmd).await?;
    println!("{reply}");
    if reply.starts_with("ERR") {
        bail!("{reply}");
    }
    Ok(())
}
