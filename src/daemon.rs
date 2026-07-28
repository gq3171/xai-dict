//! Background daemon: Right-Alt hotkey + Unix socket control.
//!
//! - **Press** Right Alt → toggle recording (start / stop)
//! - While recording with `stream = true`, phrases are ASR'd and committed live
//! - Also: `xai-dict toggle` over the socket

use crate::config::Config;
use crate::hotkey;
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
    Toggle,
    Start,
    Stop,
    Status {
        reply: tokio::sync::oneshot::Sender<State>,
    },
    Quit,
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

    let (hk_tx, mut hk_rx) = mpsc::unbounded_channel::<()>();
    let hk_stop = if let Some(key) = hotkey::parse_key(&cfg.hotkey) {
        let label = hotkey::key_label(key);
        tracing::info!(key = ?key, "enabling global hotkey (press = toggle)");
        let stream_hint = if cfg.stream { " · 流式上屏" } else { "" };
        eprintln!(
            "xai-dict daemon: hotkey = {label}  (press = start/stop{stream_hint})\n  socket {}",
            sock.display()
        );
        notify::idle(&format!("已启动 · 按 {label} 开始/结束{stream_hint}"));
        Some(hotkey::spawn_listener(key, hk_tx))
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
                        eprintln!("xai-dict: Zipformer 流式模型已常驻（预编辑实时）");
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

    let (asr_done_tx, mut asr_done_rx) =
        mpsc::unbounded_channel::<std::result::Result<String, String>>();

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
            Some(()) = hk_rx.recv() => {
                let _ = cmd_tx.send(SessionCmd::Toggle);
            }
            Some(result) = asr_done_rx.recv() => {
                match result {
                    Ok(text) => {
                        tracing::info!(n = text.len(), "delivered");
                        if text.is_empty() {
                            notify::idle("未识别到语音");
                        } else {
                            notify::done(&text);
                        }
                    }
                    Err(e) => {
                        tracing::error!("asr: {e}");
                        notify::error(&e);
                    }
                }
                *state.lock().await = State::Idle;
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
                            asr_done_tx.clone(),
                            false,
                        )
                        .await;
                    }
                    SessionCmd::Toggle => {
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
                                    asr_done_tx.clone(),
                                    true,
                                )
                                .await;
                            }
                            State::Transcribing => {
                                notify::idle("正在识别，请稍候…");
                            }
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
    asr_done_tx: mpsc::UnboundedSender<std::result::Result<String, String>>,
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

    *state.lock().await = State::Transcribing;
    notify::transcribing();
    tracing::info!(elapsed_ms, streaming = sess.streaming, "recording stopped");

    let cfg = cfg.clone();
    tokio::spawn(async move {
        let result = finish_and_deliver(&cfg, sess)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = asr_done_tx.send(result);
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

    let vad = VadConfig {
        sample_rate: cfg.sample_rate,
        frame_ms: 30,
        speech_rms: 700.0,
        min_silence_ms: cfg.stream_min_silence_ms,
        min_speech_ms: cfg.stream_min_speech_ms,
        max_segment_ms: cfg.stream_max_segment_ms,
    };

    let mut capture = if dual {
        LiveCapture::start_dual(cfg.sample_rate, vad)?
    } else if use_stream {
        LiveCapture::start_streaming(cfg.sample_rate, vad)?
    } else {
        LiveCapture::start(cfg.sample_rate)?
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
/// - Zipformer: continuous partials → fcitx Preedit
/// - Qwen3: on each VAD phrase → Commit final
async fn dual_worker(
    cfg: Config,
    mut events: mpsc::UnboundedReceiver<DualEvent>,
) -> StreamOutcome {
    let mut committed = String::new();
    let mut phrases = 0u32;
    let mut last_partial = String::new();
    // True after at least one VAD finalize ran this session.
    let mut any_vad_finalize = false;

    let sdir = std::path::PathBuf::from(&cfg.stream_model_dir);
    let qdir = std::path::PathBuf::from(&cfg.qwen3_model_dir);
    let thr = cfg.stream_threads;
    let sr = cfg.sample_rate;
    let qthr = cfg.local_threads;
    let qtok = cfg.qwen3_max_new_tokens;
    let hot = cfg.qwen3_hotwords.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = crate::local_stream::ensure_warm(&sdir, thr, sr);
        let _ = crate::local_qwen3::ensure_warm(&qdir, qthr, qtok, &hot);
    })
    .await;

    let start_ok = tokio::task::spawn_blocking(crate::local_stream::start_utterance)
        .await
        .unwrap_or(Err(anyhow::anyhow!("join")));
    if let Err(e) = start_ok {
        tracing::warn!("zipformer START failed: {e:#} — phrase-only fallback");
        // Convert remaining dual events to segments only.
        let (stx, srx) = mpsc::unbounded_channel();
        while let Some(ev) = events.recv().await {
            if let DualEvent::Segment(s) = ev {
                let _ = stx.send(s);
            }
        }
        drop(stx);
        return stream_worker(cfg, srx).await;
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
                        if partial.text != last_partial {
                            last_partial = partial.text;
                            let t = last_partial.clone();
                            let _ = tokio::task::spawn_blocking(move || output::set_preedit(&t))
                                .await;
                        }
                        // Ignore Zipformer endpoint; VAD owns phrase cuts.
                    }
                    Err(e) => tracing::debug!("zipformer feed: {e:#}"),
                }
            }
            DualEvent::Segment(seg) => {
                any_vad_finalize = true;
                let secs = seg.pcm.len() as f64 / (cfg.sample_rate as f64 * 2.0);
                tracing::info!(secs, "dual: VAD phrase → Qwen3 finalize");

                let qwen_text =
                    match pipeline::transcribe_pcm(&cfg, cfg.sample_rate, &seg.pcm).await {
                        Ok(t) => t.trim().to_string(),
                        Err(e) => {
                            tracing::debug!("Qwen3 finalize fail: {e:#}");
                            String::new()
                        }
                    };

                let final_text = if !qwen_text.is_empty() {
                    qwen_text
                } else if last_partial.chars().count() >= 2 {
                    tracing::info!(
                        n = last_partial.len(),
                        "Qwen3 empty — Zipformer partial fallback"
                    );
                    last_partial.clone()
                } else {
                    String::new()
                };
                last_partial.clear();
                let _ = tokio::task::spawn_blocking(output::clear_preedit).await;

                if final_text.is_empty() {
                    tracing::debug!("skip empty finalize");
                    let _ = tokio::task::spawn_blocking(crate::local_stream::start_utterance).await;
                    continue;
                }

                let paste = cfg.paste;
                let text_c = final_text.clone();
                let committed_ok = tokio::task::spawn_blocking(move || {
                    output::commit_final(&text_c, paste)
                })
                .await
                .unwrap_or(Ok(false));

                match committed_ok {
                    Ok(true) => {
                        append_phrase(&mut committed, &final_text);
                        phrases += 1;
                        tracing::info!(
                            n = final_text.len(),
                            phrases,
                            "dual: committed (Qwen3)"
                        );
                    }
                    Ok(false) => {
                        append_phrase(&mut committed, &final_text);
                        phrases += 1;
                        tracing::warn!("dual: commit failed, text kept in report");
                    }
                    Err(e) => tracing::warn!("dual commit: {e:#}"),
                }

                let _ = tokio::task::spawn_blocking(crate::local_stream::start_utterance).await;
            }
        }
    }

    // Leftover preedit: only if VAD never finalized (short utterance / no silence).
    // If VAD already committed phrases, leftover is usually contamination — clear only.
    let _ = tokio::task::spawn_blocking(output::clear_preedit).await;
    if !last_partial.is_empty() && !any_vad_finalize {
        let t = last_partial.clone();
        let paste = cfg.paste;
        let ok = tokio::task::spawn_blocking(move || output::commit_final(&t, paste))
            .await
            .unwrap_or(Ok(false))
            .unwrap_or(false);
        if ok || !last_partial.is_empty() {
            append_phrase(&mut committed, &last_partial);
            phrases += 1;
            tracing::info!(n = last_partial.len(), "dual: leftover partial committed");
        }
    }

    let _ = tokio::task::spawn_blocking(crate::local_stream::finish_utterance).await;

    StreamOutcome {
        committed,
        phrases,
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

/// Phrase-only (no Zipformer): each VAD phrase → Qwen3 → commit.
async fn stream_worker(
    cfg: Config,
    mut rx: mpsc::UnboundedReceiver<SpeechSegment>,
) -> StreamOutcome {
    let mut committed = String::new();
    let mut phrases = 0u32;
    let mut first = true;

    while let Some(seg) = rx.recv().await {
        let secs = seg.pcm.len() as f64 / (cfg.sample_rate as f64 * 2.0);
        tracing::info!(secs, bytes = seg.pcm.len(), "stream segment ready");

        match pipeline::transcribe_pcm(&cfg, cfg.sample_rate, &seg.pcm).await {
            Ok(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    tracing::debug!("stream segment empty ASR");
                    continue;
                }
                let piece = text;
                match output::deliver_stream_chunk(&piece, cfg.paste, first) {
                    Ok(true) => {
                        append_phrase(&mut committed, &piece);
                        phrases += 1;
                        first = false;
                        tracing::info!(n = piece.len(), phrases, "stream phrase committed");
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
                tracing::debug!("stream ASR skip: {e:#}");
            }
        }
    }

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
            // No phrase committed (very short / quiet) — fall back to full-buffer ASR.
            if pcm.len() < (cfg.sample_rate as usize) * 2 / 2 {
                bail!(
                    "录音太短或未识别到语音 ({secs:.1}s)。请多说几句再结束。"
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
            let _ = cmd_tx.send(SessionCmd::Toggle);
            let s = *state.lock().await;
            format!("OK {}\n", s.as_str())
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
