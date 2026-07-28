use anyhow::{Context, Result, bail};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;

const START_GRACE: Duration = Duration::from_millis(300);
const READ_CHUNK: usize = 3200; // ~100ms of PCM16 mono @ 16kHz

#[derive(Clone, Copy)]
enum Recorder {
    PwRecord,
    Parec,
    Arecord,
}

impl Recorder {
    fn program(self) -> &'static str {
        match self {
            Recorder::PwRecord => "pw-record",
            Recorder::Parec => "parec",
            Recorder::Arecord => "arecord",
        }
    }

    fn args(self, rate: u32) -> Vec<String> {
        let rate = rate.to_string();
        match self {
            // --raw is load-bearing on PipeWire (container formats break pipes on older PW).
            // media-category must be Capture (pw-record defaults to Playback!).
            Recorder::PwRecord => vec![
                "--raw".into(),
                "--media-category".into(),
                "Capture".into(),
                "--media-role".into(),
                "Communication".into(),
                "--rate".into(),
                rate,
                "--channels".into(),
                "1".into(),
                "--format".into(),
                "s16".into(),
                "-".into(),
            ],
            Recorder::Parec => vec![
                "--raw".into(),
                "--format=s16le".into(),
                format!("--rate={rate}"),
                "--channels=1".into(),
                // Record from default source, not monitor
                "--device=@DEFAULT_SOURCE@".into(),
            ],
            Recorder::Arecord => vec![
                "-q".into(),
                "-t".into(),
                "raw".into(),
                "-f".into(),
                "S16_LE".into(),
                "-c".into(),
                "1".into(),
                "-r".into(),
                rate,
                "-".into(),
            ],
        }
    }
}

fn on_path(name: &str) -> bool {
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

fn detect_recorder() -> Option<Recorder> {
    [Recorder::PwRecord, Recorder::Parec, Recorder::Arecord]
        .into_iter()
        .find(|r| on_path(r.program()))
}

fn spawn_recorder(sample_rate: u32) -> Result<(Recorder, Child)> {
    let recorder = detect_recorder().ok_or_else(|| {
        anyhow::anyhow!(
            "no microphone recorder on PATH — install pipewire (pw-record), \
             pulseaudio-utils (parec), or alsa-utils (arecord)"
        )
    })?;

    let mut child = Command::new(recorder.program())
        .args(recorder.args(sample_rate))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", recorder.program()))?;

    thread::sleep(START_GRACE);
    if let Some(status) = child.try_wait().context("poll recorder")? {
        let mut err = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut err);
        }
        bail!(
            "{} exited immediately ({status}): {}",
            recorder.program(),
            err.trim()
        );
    }

    Ok((recorder, child))
}

/// Live mic capture handle. Dropping (or calling [`CaptureHandle::stop`]) ends the session.
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    child: Arc<std::sync::Mutex<Option<Child>>>,
}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut g) = self.child.lock() {
            if let Some(mut c) = g.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut g) = self.child.lock() {
            if let Some(mut c) = g.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Start PCM16 mono capture at `sample_rate`, sending chunks on `tx`.
pub fn spawn_pcm_capture(sample_rate: u32, tx: mpsc::Sender<Vec<u8>>) -> Result<CaptureHandle> {
    let (recorder, mut child) = spawn_recorder(sample_rate)?;
    tracing::info!(program = recorder.program(), sample_rate, "mic capture started");

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("recorder has no stdout"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let child = Arc::new(std::sync::Mutex::new(Some(child)));
    let child_t = child.clone();

    let join = thread::Builder::new()
        .name("mic-capture".into())
        .spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            // Keep s16le frames aligned across short reads.
            let mut carry: Option<u8> = None;
            while !stop_t.load(Ordering::Relaxed) {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut chunk = Vec::with_capacity(n + 1);
                        if let Some(b) = carry.take() {
                            chunk.push(b);
                        }
                        chunk.extend_from_slice(&buf[..n]);
                        if chunk.len() % 2 == 1 {
                            carry = chunk.pop();
                        }
                        if chunk.is_empty() {
                            continue;
                        }
                        // Bridge to async channel: block if full.
                        if tx.blocking_send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            if let Ok(mut g) = child_t.lock() {
                if let Some(mut c) = g.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        })
        .context("spawn capture thread")?;

    Ok(CaptureHandle {
        stop,
        join: Some(join),
        child,
    })
}
