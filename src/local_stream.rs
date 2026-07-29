//! Warm streaming ASR worker (partials for dual-model dictation).
//!
//! Supports sherpa-onnx **online Paraformer** (preferred, Chinese-focused) and
//! **online Zipformer transducer** (legacy). Same IPC protocol either way.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct StreamWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for StreamWorker {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "QUIT");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static WORKER: Mutex<Option<StreamWorker>> = Mutex::new(None);

/// Default timeout for a single stream IPC reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub struct PartialResult {
    pub text: String,
    /// True when FINISH returned FINAL.
    pub is_final: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamBackend {
    /// encoder + decoder (no joiner)
    Paraformer,
    /// encoder + decoder + joiner
    Zipformer,
}

/// Preferred default: FunASR streaming Paraformer (bilingual zh-en) via sherpa-onnx.
pub fn default_model_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("xai-dict/models/sherpa-onnx-streaming-paraformer-bilingual-zh-en")
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    for n in names {
        let p = dir.join(n);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn detect_backend(dir: &Path) -> Option<StreamBackend> {
    let tokens = dir.join("tokens.txt");
    if !tokens.is_file() {
        return None;
    }
    let has_enc = first_existing(
        dir,
        &[
            "encoder.int8.onnx",
            "encoder.onnx",
            "encoder.int8",
            "encoder",
        ],
    )
    .is_some();
    let has_dec = first_existing(
        dir,
        &[
            "decoder.int8.onnx",
            "decoder.onnx",
            "decoder.int8",
            "decoder",
        ],
    )
    .is_some();
    if !has_enc || !has_dec {
        return None;
    }
    let has_joiner = first_existing(
        dir,
        &[
            "joiner.int8.onnx",
            "joiner.onnx",
            "joiner.int8",
            "joiner",
        ],
    )
    .is_some();
    if has_joiner {
        Some(StreamBackend::Zipformer)
    } else {
        Some(StreamBackend::Paraformer)
    }
}

pub fn model_ready(dir: &Path) -> bool {
    detect_backend(dir).is_some()
}

struct ModelPaths {
    backend: StreamBackend,
    encoder: PathBuf,
    decoder: PathBuf,
    joiner: Option<PathBuf>,
    tokens: PathBuf,
}

fn resolve_model_paths(dir: &Path) -> Result<ModelPaths> {
    let backend = detect_backend(dir).ok_or_else(|| {
        anyhow::anyhow!(
            "streaming model not ready at {}\n\
             Expected Paraformer (encoder+decoder+tokens) or Zipformer (+joiner).\n\
             Download Paraformer:\n\
               cd ~/.local/share/xai-dict/models && \\\n\
               curl -L -O https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-paraformer-bilingual-zh-en.tar.bz2 && \\\n\
               tar xvf sherpa-onnx-streaming-paraformer-bilingual-zh-en.tar.bz2",
            dir.display()
        )
    })?;
    let encoder = first_existing(
        dir,
        &["encoder.int8.onnx", "encoder.onnx"],
    )
    .context("encoder onnx")?;
    let decoder = first_existing(
        dir,
        &["decoder.int8.onnx", "decoder.onnx"],
    )
    .context("decoder onnx")?;
    let joiner = first_existing(dir, &["joiner.int8.onnx", "joiner.onnx"]);
    let tokens = dir.join("tokens.txt");
    if !tokens.is_file() {
        bail!("missing tokens.txt in {}", dir.display());
    }
    if backend == StreamBackend::Zipformer && joiner.is_none() {
        bail!("zipformer model missing joiner in {}", dir.display());
    }
    Ok(ModelPaths {
        backend,
        encoder,
        decoder,
        joiner,
        tokens,
    })
}

pub fn ensure_warm(model_dir: &Path, threads: u32, sample_rate: u32) -> Result<()> {
    let mut g = WORKER
        .lock()
        .map_err(|_| anyhow::anyhow!("stream worker mutex poisoned"))?;
    if let Some(w) = g.as_mut() {
        match w.child.try_wait() {
            Ok(None) => return Ok(()),
            Ok(Some(st)) => {
                tracing::warn!(%st, "stream_worker exited; restarting");
                *g = None;
            }
            Err(e) => {
                tracing::warn!(%e, "stream worker poll failed");
                *g = None;
            }
        }
    }
    *g = Some(spawn_worker(model_dir, threads, sample_rate)?);
    Ok(())
}

pub fn start_utterance() -> Result<()> {
    with_worker(|w| {
        writeln!(w.stdin, "START")?;
        w.stdin.flush()?;
        let line = read_line_timeout(w, REPLY_TIMEOUT)?;
        if line == "OK" {
            return Ok(());
        }
        bail!("START failed: {line}");
    })
}

/// Feed raw s16le mono PCM; returns latest partial text for this stream.
pub fn feed_pcm(pcm: &[u8]) -> Result<PartialResult> {
    if pcm.is_empty() {
        return Ok(PartialResult {
            text: String::new(),
            is_final: false,
        });
    }
    // Only send even-length s16 frames.
    let n = pcm.len() - (pcm.len() % 2);
    if n == 0 {
        return Ok(PartialResult {
            text: String::new(),
            is_final: false,
        });
    }
    let pcm = &pcm[..n];
    with_worker(|w| {
        writeln!(w.stdin, "PCM {}", pcm.len())?;
        w.stdin.write_all(pcm)?;
        w.stdin.flush()?;
        let line = read_line_timeout(w, REPLY_TIMEOUT)?;
        parse_partial(&line)
    })
}

pub fn finish_utterance() -> Result<PartialResult> {
    with_worker(|w| {
        writeln!(w.stdin, "FINISH")?;
        w.stdin.flush()?;
        let line = read_line_timeout(w, REPLY_TIMEOUT)?;
        parse_partial(&line)
    })
}

fn parse_partial(line: &str) -> Result<PartialResult> {
    if let Some(t) = line.strip_prefix("PARTIAL ") {
        return Ok(PartialResult {
            text: t.to_string(),
            is_final: false,
        });
    }
    if let Some(t) = line.strip_prefix("FINAL ") {
        return Ok(PartialResult {
            text: t.to_string(),
            is_final: true,
        });
    }
    if line == "PARTIAL" || line == "FINAL" {
        return Ok(PartialResult {
            text: String::new(),
            is_final: line.starts_with("FINAL"),
        });
    }
    if let Some(e) = line.strip_prefix("ERR ") {
        bail!("stream_worker: {e}");
    }
    bail!("stream worker bad reply: {line}");
}

fn with_worker<T>(f: impl FnOnce(&mut StreamWorker) -> Result<T>) -> Result<T> {
    let mut g = WORKER
        .lock()
        .map_err(|_| anyhow::anyhow!("stream worker mutex poisoned"))?;
    let w = g
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("stream worker not started"))?;
    match f(w) {
        Ok(v) => Ok(v),
        Err(e) => {
            if e.to_string().contains("timeout") || e.to_string().contains("EOF") {
                *g = None;
            }
            Err(e)
        }
    }
}

fn read_line_timeout(w: &mut StreamWorker, timeout: Duration) -> Result<String> {
    use std::os::fd::AsRawFd;
    let fd = w.stdout.get_ref().as_raw_fd();
    let deadline = Instant::now() + timeout;
    loop {
        {
            let buf = w.stdout.fill_buf().context("stream fill_buf")?;
            if !buf.is_empty() {
                let mut line = String::new();
                w.stdout
                    .read_line(&mut line)
                    .context("stream read_line")?;
                return Ok(line.trim_end_matches(['\r', '\n']).to_string());
            }
        }
        if Instant::now() >= deadline {
            let _ = w.child.kill();
            bail!("stream read timeout");
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let remain = deadline.saturating_duration_since(Instant::now());
        let ms = remain.as_millis().min(i32::MAX as u128) as i32;
        let pr = unsafe { libc::poll(&mut pfd, 1, ms.max(1)) };
        if pr < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            bail!("stream poll: {err}");
        }
    }
}

fn spawn_worker(model_dir: &Path, threads: u32, sample_rate: u32) -> Result<StreamWorker> {
    let paths = resolve_model_paths(model_dir)?;
    let bin = find_worker_bin().context("zipformer_worker binary not found")?;

    let mut cmd = Command::new(&bin);
    cmd.arg(format!("--encoder={}", paths.encoder.display()));
    cmd.arg(format!("--decoder={}", paths.decoder.display()));
    cmd.arg(format!("--tokens={}", paths.tokens.display()));
    cmd.arg(format!("--threads={}", threads.max(1)));
    cmd.arg(format!("--sample-rate={sample_rate}"));
    match paths.backend {
        StreamBackend::Paraformer => {
            cmd.arg("--model-type=paraformer");
        }
        StreamBackend::Zipformer => {
            let j = paths.joiner.as_ref().context("joiner")?;
            cmd.arg(format!("--joiner={}", j.display()));
            cmd.arg("--model-type=transducer");
        }
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    tracing::info!(
        bin = %bin.display(),
        backend = ?paths.backend,
        dir = %model_dir.display(),
        "starting stream_worker"
    );
    let t0 = Instant::now();
    let mut child = cmd.spawn().context("spawn stream_worker")?;
    let stdin = child.stdin.take().context("stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("stdout")?);

    if let Some(stderr) = child.stderr.take() {
        std::thread::Builder::new()
            .name("stream-worker-stderr".into())
            .spawn(move || {
                let r = BufReader::new(stderr);
                for line in r.lines().flatten() {
                    tracing::debug!(target: "stream_worker", "{line}");
                }
            })
            .ok();
    }

    let mut w = StreamWorker {
        child,
        stdin,
        stdout,
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() > deadline {
            let _ = w.child.kill();
            bail!("stream worker READY timeout");
        }
        if let Ok(Some(st)) = w.child.try_wait() {
            bail!("stream worker exited before READY ({st})");
        }
        let remain = deadline.saturating_duration_since(Instant::now());
        let line = read_line_timeout(&mut w, remain.min(Duration::from_secs(5)))?;
        if line == "READY" {
            break;
        }
        if let Some(e) = line.strip_prefix("ERR ") {
            bail!("stream worker: {e}");
        }
    }
    tracing::info!(
        ms = t0.elapsed().as_millis() as u64,
        backend = ?paths.backend,
        "stream_worker READY"
    );
    Ok(w)
}

fn find_worker_bin() -> Option<PathBuf> {
    if let Some(p) = option_env!("ZIPFORMER_WORKER_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("zipformer_worker");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Some(data) = dirs::data_local_dir() {
        let p = data.join("xai-dict/bin/zipformer_worker");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = dirs::home_dir() {
        let p = home.join(".cargo/bin/zipformer_worker");
        if p.is_file() {
            return Some(p);
        }
    }
    which_bin("zipformer_worker")
}

fn which_bin(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
        {
            return Some(p);
        }
    }
    None
}

#[allow(dead_code)]
fn _use_read<R: Read>(_: &mut R) {}
