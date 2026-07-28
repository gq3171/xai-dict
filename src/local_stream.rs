//! Warm streaming Zipformer worker (partials for dual-model dictation).

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

#[derive(Debug, Clone)]
pub struct PartialResult {
    pub text: String,
    /// True when Zipformer endpointed / FINISH returned FINAL.
    pub is_final: bool,
}

pub fn default_model_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("xai-dict/models/sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30")
}

pub fn model_ready(dir: &Path) -> bool {
    dir.join("encoder.int8.onnx").is_file()
        && dir.join("decoder.onnx").is_file()
        && dir.join("joiner.int8.onnx").is_file()
        && dir.join("tokens.txt").is_file()
}

pub fn ensure_warm(model_dir: &Path, threads: u32, sample_rate: u32) -> Result<()> {
    let mut g = WORKER.lock().expect("stream mutex");
    if let Some(w) = g.as_mut() {
        match w.child.try_wait() {
            Ok(None) => return Ok(()),
            Ok(Some(st)) => {
                tracing::warn!(%st, "zipformer_worker exited; restarting");
                *g = None;
            }
            Err(e) => {
                tracing::warn!(%e, "zipformer poll failed");
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
        let line = read_line(w)?;
        if line == "OK" {
            return Ok(());
        }
        bail!("START failed: {line}");
    })
}

/// Feed raw s16le mono PCM; returns latest partial/final text for this stream.
pub fn feed_pcm(pcm: &[u8]) -> Result<PartialResult> {
    if pcm.is_empty() {
        return Ok(PartialResult {
            text: String::new(),
            is_final: false,
        });
    }
    with_worker(|w| {
        writeln!(w.stdin, "PCM {}", pcm.len())?;
        w.stdin.write_all(pcm)?;
        w.stdin.flush()?;
        let line = read_line(w)?;
        parse_partial(&line)
    })
}

pub fn finish_utterance() -> Result<PartialResult> {
    with_worker(|w| {
        writeln!(w.stdin, "FINISH")?;
        w.stdin.flush()?;
        let line = read_line(w)?;
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
        bail!("zipformer_worker: {e}");
    }
    bail!("zipformer bad reply: {line}");
}

fn with_worker<T>(f: impl FnOnce(&mut StreamWorker) -> Result<T>) -> Result<T> {
    let mut g = WORKER.lock().expect("stream mutex");
    let w = g
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("zipformer worker not started"))?;
    f(w)
}

fn read_line(w: &mut StreamWorker) -> Result<String> {
    let mut line = String::new();
    w.stdout.read_line(&mut line).context("read zipformer")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn spawn_worker(model_dir: &Path, threads: u32, sample_rate: u32) -> Result<StreamWorker> {
    let enc = model_dir.join("encoder.int8.onnx");
    let dec = model_dir.join("decoder.onnx");
    let join = model_dir.join("joiner.int8.onnx");
    let tokens = model_dir.join("tokens.txt");
    for p in [&enc, &dec, &join, &tokens] {
        if !p.is_file() {
            bail!("missing {}", p.display());
        }
    }
    let bin = find_worker_bin().context("zipformer_worker binary not found")?;

    let mut cmd = Command::new(&bin);
    cmd.arg(format!("--encoder={}", enc.display()));
    cmd.arg(format!("--decoder={}", dec.display()));
    cmd.arg(format!("--joiner={}", join.display()));
    cmd.arg(format!("--tokens={}", tokens.display()));
    cmd.arg(format!("--threads={}", threads.max(1)));
    cmd.arg(format!("--sample-rate={sample_rate}"));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    tracing::info!(bin = %bin.display(), "starting zipformer_worker");
    let t0 = Instant::now();
    let mut child = cmd.spawn().context("spawn zipformer_worker")?;
    let stdin = child.stdin.take().context("stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("stdout")?);

    if let Some(stderr) = child.stderr.take() {
        std::thread::Builder::new()
            .name("zipformer-stderr".into())
            .spawn(move || {
                let r = BufReader::new(stderr);
                for line in r.lines().flatten() {
                    tracing::debug!(target: "zipformer_worker", "{line}");
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
            bail!("zipformer READY timeout");
        }
        if let Ok(Some(st)) = w.child.try_wait() {
            bail!("zipformer exited before READY ({st})");
        }
        let line = read_line(&mut w)?;
        if line == "READY" {
            break;
        }
        if let Some(e) = line.strip_prefix("ERR ") {
            bail!("zipformer: {e}");
        }
    }
    tracing::info!(
        ms = t0.elapsed().as_millis() as u64,
        "zipformer_worker READY"
    );
    Ok(w)
}

fn find_worker_bin() -> Option<PathBuf> {
    for key in ["ZIPFORMER_WORKER_PATH", "Qwen3_WORKER_PATH"] {
        // only first is relevant; listed for symmetry
        let _ = key;
    }
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

// silence unused import warning for Read if only BufRead used
#[allow(dead_code)]
fn _use_read<R: Read>(_: &mut R) {}
