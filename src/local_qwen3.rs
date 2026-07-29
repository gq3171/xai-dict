//! Qwen3-ASR via a **warm** long-lived worker (C API) so model load (~2s) is paid once.
//!
//! Falls back to spawning `sherpa-onnx-offline` per file if the worker is unavailable.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct WarmWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for WarmWorker {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "QUIT");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static WORKER: Mutex<Option<WarmWorker>> = Mutex::new(None);

/// Ensure the warm worker is running (loads model once). Safe to call repeatedly.
pub fn ensure_warm(
    model_dir: &Path,
    threads: u32,
    max_new_tokens: u32,
    hotwords: &str,
) -> Result<()> {
    let mut g = WORKER
        .lock()
        .map_err(|_| anyhow::anyhow!("qwen3 mutex poisoned"))?;
    if let Some(w) = g.as_mut() {
        // Health check: process still alive?
        match w.child.try_wait() {
            Ok(None) => return Ok(()),
            Ok(Some(status)) => {
                tracing::warn!(%status, "qwen3_worker exited; restarting");
                *g = None;
            }
            Err(e) => {
                tracing::warn!(%e, "qwen3_worker poll failed; restarting");
                *g = None;
            }
        }
    }
    let worker = spawn_worker(model_dir, threads, max_new_tokens, hotwords)?;
    *g = Some(worker);
    Ok(())
}

/// Force-drop and respawn the warm worker (e.g. after timeout / poison).
pub fn recover(
    model_dir: &Path,
    threads: u32,
    max_new_tokens: u32,
    hotwords: &str,
) -> Result<()> {
    if let Ok(mut g) = WORKER.lock() {
        *g = None;
    }
    ensure_warm(model_dir, threads, max_new_tokens, hotwords)
}

/// Transcribe a WAV; reuses the warm process when available.
pub fn transcribe_file(
    wav: &Path,
    model_dir: &Path,
    threads: u32,
    max_new_tokens: u32,
    hotwords: &str,
) -> Result<String> {
    // Prefer warm path; on failure recover once then CLI fallback.
    match ensure_warm(model_dir, threads, max_new_tokens, hotwords)
        .and_then(|_| warm_transcribe(wav))
    {
        Ok(text) => return Ok(text),
        Err(e) => {
            tracing::warn!(%e, "warm qwen3 failed — recovering once");
            match recover(model_dir, threads, max_new_tokens, hotwords)
                .and_then(|_| warm_transcribe(wav))
            {
                Ok(text) => return Ok(text),
                Err(e2) => {
                    tracing::warn!(%e2, "warm qwen3 after recover failed — CLI fallback");
                    if let Ok(mut g) = WORKER.lock() {
                        *g = None;
                    }
                }
            }
        }
    }
    transcribe_file_cli(wav, model_dir, threads, max_new_tokens, hotwords)
}

fn warm_transcribe(wav: &Path) -> Result<String> {
    let mut g = WORKER
        .lock()
        .map_err(|_| anyhow::anyhow!("qwen3 mutex poisoned"))?;
    let w = g
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("warm worker not started"))?;

    let path = wav
        .canonicalize()
        .with_context(|| format!("canonicalize {}", wav.display()))?;
    let path_s = path.to_str().context("wav path utf8")?;

    let t0 = Instant::now();
    writeln!(w.stdin, "WAV {path_s}").context("write WAV cmd")?;
    w.stdin.flush().context("flush WAV cmd")?;

    // Decode: short phrases ~0.5s; long audio can be a few seconds. Cap at 45s.
    let line = match read_line_timeout(w, Duration::from_secs(45)) {
        Ok(l) => l,
        Err(e) => {
            *g = None; // force restart next call
            return Err(e);
        }
    };
    let elapsed = t0.elapsed();

    if let Some(text) = line.strip_prefix("OK ") {
        tracing::info!(
            ms = elapsed.as_millis() as u64,
            n = text.len(),
            "qwen3 warm decode"
        );
        return Ok(text.to_string());
    }
    if line == "OK" || line.starts_with("OK") {
        tracing::info!(ms = elapsed.as_millis() as u64, "qwen3 warm decode empty");
        return Ok(String::new());
    }
    if let Some(err) = line.strip_prefix("ERR ") {
        bail!("qwen3_worker: {err}");
    }
    bail!("qwen3_worker bad reply: {line}");
}

fn read_line_timeout(w: &mut WarmWorker, timeout: Duration) -> Result<String> {
    use std::io::BufRead;
    use std::os::fd::AsRawFd;
    let fd = w.stdout.get_ref().as_raw_fd();
    let deadline = Instant::now() + timeout;
    loop {
        // Already buffered?
        {
            let buf = w.stdout.fill_buf().context("qwen3 fill_buf")?;
            if !buf.is_empty() {
                let mut line = String::new();
                w.stdout
                    .read_line(&mut line)
                    .context("qwen3 read_line")?;
                return Ok(line.trim_end_matches(['\r', '\n']).to_string());
            }
        }
        if Instant::now() >= deadline {
            let _ = w.child.kill();
            bail!("qwen3 read timeout");
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
            bail!("qwen3 poll: {err}");
        }
    }
}

fn spawn_worker(
    model_dir: &Path,
    threads: u32,
    max_new_tokens: u32,
    hotwords: &str,
) -> Result<WarmWorker> {
    let paths = resolve_model_paths(model_dir)?;
    let bin = find_worker_bin().context(
        "qwen3_worker not found — rebuild with gcc + sherpa-onnx C API, \
         or use sherpa-onnx-offline CLI fallback",
    )?;

    // Stream segments rarely need 512 tokens; clamp for speed.
    let max_tok = max_new_tokens.clamp(32, 256);

    let mut cmd = Command::new(&bin);
    cmd.arg(format!("--conv={}", paths.conv_frontend.display()));
    cmd.arg(format!("--encoder={}", paths.encoder.display()));
    cmd.arg(format!("--decoder={}", paths.decoder.display()));
    cmd.arg(format!("--tokenizer={}", paths.tokenizer.display()));
    cmd.arg(format!("--threads={threads}"));
    cmd.arg(format!("--max-new-tokens={max_tok}"));
    if !hotwords.is_empty() {
        cmd.arg(format!("--hotwords={hotwords}"));
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    tracing::info!(bin = %bin.display(), "starting qwen3_worker (model load once)");
    let t0 = Instant::now();
    let mut child = cmd.spawn().with_context(|| format!("spawn {}", bin.display()))?;
    let mut stdin = child.stdin.take().context("worker stdin")?;
    let stdout = child.stdout.take().context("worker stdout")?;
    let mut stdout = BufReader::new(stdout);

    // Wait for READY (model load).
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("qwen3_worker READY timeout");
        }
        // Check process death.
        if let Ok(Some(status)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut err);
            }
            bail!(
                "qwen3_worker exited before READY ({status}): {}",
                err.trim()
            );
        }
        line.clear();
        // Blocking read_line with no easy timeout without threads; model load is ~2s.
        match stdout.read_line(&mut line) {
            Ok(0) => bail!("qwen3_worker EOF before READY"),
            Ok(_) => {
                let t = line.trim();
                if t == "READY" {
                    break;
                }
                if let Some(e) = t.strip_prefix("ERR ") {
                    bail!("qwen3_worker: {e}");
                }
                // ignore other lines
            }
            Err(e) => bail!("read READY: {e}"),
        }
    }

    // Drain stderr in background so the pipe never fills.
    if let Some(stderr) = child.stderr.take() {
        std::thread::Builder::new()
            .name("qwen3-worker-stderr".into())
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().flatten() {
                    tracing::debug!(target: "qwen3_worker", "{line}");
                }
            })
            .ok();
    }

    tracing::info!(
        ms = t0.elapsed().as_millis() as u64,
        "qwen3_worker READY (model resident)"
    );

    // Touch stdin so drop path is clear.
    let _ = &mut stdin;

    Ok(WarmWorker {
        child,
        stdin,
        stdout,
    })
}

fn find_worker_bin() -> Option<PathBuf> {
    // 1) Built by build.rs this compile
    for key in [
        option_env!("QWEN3_WORKER_PATH"),
        option_env!("Qwen3_WORKER_PATH"),
    ]
    .into_iter()
    .flatten()
    {
        let p = PathBuf::from(key);
        if p.is_file() {
            return Some(p);
        }
    }
    // 2) Next to running binary / cargo target
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("qwen3_worker");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    // 3) Debian/system package layout
    for p in [
        PathBuf::from("/usr/lib/xai-dict/qwen3_worker"),
        PathBuf::from("/usr/libexec/xai-dict/qwen3_worker"),
    ] {
        if p.is_file() {
            return Some(p);
        }
    }
    // 4) User data
    if let Some(data) = dirs::data_local_dir() {
        let p = data.join("xai-dict/bin/qwen3_worker");
        if p.is_file() {
            return Some(p);
        }
    }
    // 5) cargo bin
    if let Some(home) = dirs::home_dir() {
        let p = home.join(".cargo/bin/qwen3_worker");
        if p.is_file() {
            return Some(p);
        }
    }
    // 6) PATH
    which_bin("qwen3_worker")
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

// ─── cold CLI fallback ──────────────────────────────────────────────────────

fn transcribe_file_cli(
    wav: &Path,
    model_dir: &Path,
    threads: u32,
    max_new_tokens: u32,
    hotwords: &str,
) -> Result<String> {
    let paths = resolve_model_paths(model_dir)?;
    let bin = which_sherpa().context(
        "sherpa-onnx-offline not found — install with: sudo pacman -S sherpa-onnx",
    )?;

    let mut cmd = Command::new(&bin);
    cmd.arg(format!(
        "--qwen3-asr-conv-frontend={}",
        paths.conv_frontend.display()
    ));
    cmd.arg(format!(
        "--qwen3-asr-encoder={}",
        paths.encoder.display()
    ));
    cmd.arg(format!(
        "--qwen3-asr-decoder={}",
        paths.decoder.display()
    ));
    cmd.arg(format!(
        "--qwen3-asr-tokenizer={}",
        paths.tokenizer.display()
    ));
    cmd.arg(format!("--qwen3-asr-max-new-tokens={max_new_tokens}"));
    cmd.arg(format!("--num-threads={threads}"));
    if !hotwords.is_empty() {
        cmd.arg(format!("--qwen3-asr-hotwords={hotwords}"));
    }
    cmd.arg(wav.to_str().context("wav path utf8")?);

    let output = cmd
        .output()
        .with_context(|| format!("run {}", bin.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");

    if !output.status.success() {
        let tail: String = combined
            .chars()
            .rev()
            .take(600)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        bail!(
            "sherpa-onnx-offline failed ({}): {}",
            output.status,
            tail.trim()
        );
    }

    let text = extract_text(&combined).unwrap_or_default();
    Ok(text.trim().to_string())
}

struct ModelPaths {
    conv_frontend: PathBuf,
    encoder: PathBuf,
    decoder: PathBuf,
    tokenizer: PathBuf,
}

fn resolve_model_paths(model_dir: &Path) -> Result<ModelPaths> {
    if !model_dir.is_dir() {
        bail!(
            "Qwen3 model dir not found: {}\n\
             Download:\n  mkdir -p ~/.local/share/xai-dict/models && cd $_ && \\\n  \
             curl -fL -O https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2 && \\\n  \
             tar xjf sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2",
            model_dir.display()
        );
    }

    let conv = model_dir.join("conv_frontend.onnx");
    let enc = model_dir.join("encoder.int8.onnx");
    let dec = model_dir.join("decoder.int8.onnx");
    let tok = model_dir.join("tokenizer");

    for (label, p) in [
        ("conv_frontend.onnx", &conv),
        ("encoder.int8.onnx", &enc),
        ("decoder.int8.onnx", &dec),
    ] {
        if !p.is_file() {
            bail!("missing {label} under {}", model_dir.display());
        }
    }
    if !tok.is_dir() {
        bail!("missing tokenizer/ under {}", model_dir.display());
    }

    Ok(ModelPaths {
        conv_frontend: conv,
        encoder: enc,
        decoder: dec,
        tokenizer: tok,
    })
}

fn extract_text(raw: &str) -> Option<String> {
    for line in raw.lines().rev() {
        let line = line.trim();
        if !(line.starts_with('{') && line.contains("\"text\"")) {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                let t = t.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn which_sherpa() -> Option<PathBuf> {
    which_bin("sherpa-onnx-offline").or_else(|| {
        let p = PathBuf::from("/usr/bin/sherpa-onnx-offline");
        p.is_file().then_some(p)
    })
}

pub fn default_model_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("xai-dict")
        .join("models")
        .join("sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25")
}

pub fn model_ready(dir: &Path) -> bool {
    dir.join("encoder.int8.onnx").is_file()
        && dir.join("decoder.int8.onnx").is_file()
        && dir.join("conv_frontend.onnx").is_file()
        && dir.join("tokenizer").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_text() {
        let raw = r#"
Creating recognizer ...
Done!
----
{"lang": "", "emotion": "", "event": "", "text": "拨号，请再说一次。", "timestamps": [], "tokens":[]}
"#;
        assert_eq!(extract_text(raw).as_deref(), Some("拨号，请再说一次。"));
    }
}
