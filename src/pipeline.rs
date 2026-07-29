//! Shared record → ASR → deliver pipeline used by CLI and daemon.

use crate::auth;
use crate::capture;
use crate::config::{Config, Provider};
use crate::local_qwen3;
use crate::local_whisper;
use crate::proxy;
use crate::stream_vad::{Segmenter, SpeechSegment, VadConfig};
use crate::stt_rest;
use crate::wav;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Transcribe an existing WAV with the configured provider.
pub async fn transcribe_wav(cfg: &Config, wav_path: &Path) -> Result<String> {
    match cfg.provider {
        Provider::Qwen3 => {
            let dir = PathBuf::from(&cfg.qwen3_model_dir);
            if !local_qwen3::model_ready(&dir) {
                bail!(
                    "Qwen3 model not ready at {}\n\
                     See: xai-dict whoami",
                    dir.display()
                );
            }
            let threads = cfg.local_threads;
            let max_tok = cfg.qwen3_max_new_tokens;
            let hotwords = cfg.qwen3_hotwords.clone();
            let wav = wav_path.to_path_buf();
            let text = tokio::task::spawn_blocking(move || {
                local_qwen3::transcribe_file(&wav, &dir, threads, max_tok, &hotwords)
            })
            .await
            .context("join qwen3")??;
            Ok(text.trim().to_string())
        }
        Provider::Local => {
            let model = PathBuf::from(&cfg.local_model);
            let lang = cfg.language.clone();
            let threads = cfg.local_threads;
            let to_simp = lang == "zh" || lang == "chinese" || lang == "auto";
            let wav = wav_path.to_path_buf();
            let text = tokio::task::spawn_blocking(move || {
                local_whisper::transcribe_file(&wav, &model, &lang, threads, to_simp)
            })
            .await
            .context("join whisper")??;
            Ok(text.trim().to_string())
        }
        Provider::Xai => {
            let bearer = auth::resolve_bearer(None)?;
            let _proxy = proxy::resolve_http_proxy(if cfg.proxy.is_empty() {
                None
            } else {
                Some(cfg.proxy.as_str())
            });
            let result = stt_rest::transcribe_file(cfg, &bearer, wav_path).await?;
            Ok(result.text.trim().to_string())
        }
    }
}

/// Transcribe raw PCM16 mono in-memory (no strict min-length for short stream phrases).
pub async fn transcribe_pcm(cfg: &Config, sample_rate: u32, pcm: &[u8]) -> Result<String> {
    let min_bytes = (sample_rate as usize) * 2 / 4; // 0.25s
    if pcm.len() < min_bytes {
        bail!("segment too short");
    }
    let (peak, _) = wav::pcm16_levels(pcm);
    // After stream AGC, true silence stays very low; quiet speech is boosted.
    if peak < 120 {
        bail!("segment silent");
    }
    let prepared = wav::pcm16_prepare_for_asr(pcm, sample_rate);
    let tmp = std::env::temp_dir().join(format!(
        "xai-dict-seg-{}-{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    wav::write_pcm16_mono_wav(&tmp, sample_rate, &prepared)?;
    let text = transcribe_wav(cfg, &tmp).await;
    let _ = std::fs::remove_file(&tmp);
    text
}

/// Write PCM16 mono to a temp WAV path (normalized for ASR).
pub fn pcm_to_temp_wav(sample_rate: u32, pcm: &[u8]) -> Result<PathBuf> {
    // Need at least ~0.4s of audio
    let min_bytes = (sample_rate as usize) * 2 * 2 / 5; // 0.4s
    if pcm.len() < min_bytes {
        bail!(
            "录音太短 ({:.2}s) — 说完后再按一次快捷键",
            pcm.len() as f64 / (sample_rate as f64 * 2.0)
        );
    }
    let (peak, rms) = wav::pcm16_levels(pcm);
    if peak < 120 {
        let src = default_source_hint();
        bail!(
            "麦克风几乎无信号 (peak={peak}, rms={rms:.0}){src}。\
             请检查：系统设置→声音→输入设备是否选对、是否静音；USB 麦可换内置麦试一下"
        );
    }

    let prepared = wav::pcm16_prepare_for_asr(pcm, sample_rate);
    let (peak2, rms2) = wav::pcm16_levels(&prepared);
    tracing::info!(
        peak_in = peak,
        rms_in = format!("{rms:.0}"),
        peak_out = peak2,
        rms_out = format!("{rms2:.0}"),
        "pcm prepared for ASR"
    );

    let tmp = std::env::temp_dir().join(format!(
        "xai-dict-{}.wav",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    wav::write_pcm16_mono_wav(&tmp, sample_rate, &prepared)?;

    // Also keep a copy for debugging empty-ASR cases.
    if let Some(data_dir) = dirs::data_local_dir() {
        let last = data_dir.join("xai-dict").join("last.wav");
        if let Some(parent) = last.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = wav::write_pcm16_mono_wav(&last, sample_rate, &prepared);
        tracing::info!(path = %last.display(), "saved last.wav");
    }
    Ok(tmp)
}

/// Best-effort name of the current default capture device (for error messages).
fn default_source_hint() -> String {
    let try_cmd = |prog: &str, args: &[&str]| -> Option<String> {
        let o = std::process::Command::new(prog)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !o.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };
    // pactl: alsa_input....
    if let Some(name) = try_cmd("pactl", &["get-default-source"]) {
        return format!("，当前输入={name}");
    }
    String::new()
}

/// Ordered dual-model events: chunks always precede any segment they produce.
#[derive(Debug)]
pub enum DualEvent {
    /// Even-length s16le mono PCM.
    Chunk(Vec<u8>),
    /// Completed VAD speech phrase (after the chunks that formed it).
    Segment(SpeechSegment),
}

/// Live mic session. Optionally streams speech segments / ordered dual events.
pub struct LiveCapture {
    pub handle: capture::CaptureHandle,
    /// Full PCM (for final flush / non-stream path).
    pcm: Arc<Mutex<Vec<u8>>>,
    collect: tokio::task::JoinHandle<()>,
    /// Completed speech phrases (phrase streaming mode).
    segment_rx: Option<mpsc::UnboundedReceiver<SpeechSegment>>,
    /// Ordered dual-model event stream (chunks then segments).
    dual_rx: Option<mpsc::UnboundedReceiver<DualEvent>>,
}

/// Optional capture tuning (near-field gates + AGC).
#[derive(Debug, Clone)]
pub struct CaptureTune {
    pub agc_max_gain: f64,
    /// Only forward dual-model preedit chunks at/above this RMS (0 = always).
    pub preedit_min_rms: f64,
}

impl Default for CaptureTune {
    fn default() -> Self {
        Self {
            agc_max_gain: 4.0,
            preedit_min_rms: 700.0,
        }
    }
}

impl LiveCapture {
    pub fn start(sample_rate: u32) -> Result<Self> {
        Self::start_inner(sample_rate, None, false, CaptureTune::default())
    }

    pub fn start_streaming(sample_rate: u32, vad: VadConfig, tune: CaptureTune) -> Result<Self> {
        Self::start_inner(sample_rate, Some(vad), false, tune)
    }

    /// Dual-model: ordered Chunk/Segment events for stream preedit + Qwen3.
    pub fn start_dual(sample_rate: u32, vad: VadConfig, tune: CaptureTune) -> Result<Self> {
        Self::start_inner(sample_rate, Some(vad), true, tune)
    }

    fn start_inner(
        sample_rate: u32,
        vad: Option<VadConfig>,
        dual: bool,
        tune: CaptureTune,
    ) -> Result<Self> {
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<u8>>(64);
        let handle = capture::spawn_pcm_capture(sample_rate, pcm_tx)?;
        let pcm = Arc::new(Mutex::new(Vec::new()));
        let pcm_c = pcm.clone();

        let (seg_tx, seg_rx) = if vad.is_some() && !dual {
            let (t, r) = mpsc::unbounded_channel();
            (Some(t), Some(r))
        } else {
            (None, None)
        };

        let (dual_tx, dual_rx) = if dual {
            let (t, r) = mpsc::unbounded_channel();
            (Some(t), Some(r))
        } else {
            (None, None)
        };

        let collect = tokio::spawn(async move {
            let mut segmenter = vad.map(Segmenter::new);
            // Carry a leftover odd byte so dual PCM is always even s16 frames.
            let mut odd_carry: Option<u8> = None;
            // Limited AGC: enough for slightly quiet mics, not enough to lift far talkers.
            let mut agc = wav::StreamAgc::with_max_gain(tune.agc_max_gain);
            let _ = tune.preedit_min_rms;

            while let Some(mut chunk) = pcm_rx.recv().await {
                chunk = agc.process(&chunk);

                {
                    let mut g = pcm_c.lock().await;
                    g.extend_from_slice(&chunk);
                }

                // Align s16le for dual/stream path.
                if dual {
                    if let Some(b) = odd_carry.take() {
                        let mut aligned = Vec::with_capacity(chunk.len() + 1);
                        aligned.push(b);
                        aligned.extend_from_slice(&chunk);
                        chunk = aligned;
                    }
                    if chunk.len() % 2 == 1 {
                        odd_carry = chunk.pop();
                    }
                }

                if let Some(tx) = dual_tx.as_ref() {
                    if !chunk.is_empty() {
                        // Continuous audio for streaming ASR (do not drop quiet frames).
                        let _ = tx.send(DualEvent::Chunk(chunk.clone()));
                    }
                }

                if let Some(seg) = segmenter.as_mut() {
                    for s in seg.push(&chunk) {
                        if let Some(tx) = dual_tx.as_ref() {
                            let _ = tx.send(DualEvent::Segment(s));
                        } else if let Some(tx) = seg_tx.as_ref() {
                            let _ = tx.send(s);
                        }
                    }
                }
            }
            if let Some(seg) = segmenter.as_mut() {
                if let Some(s) = seg.flush() {
                    if let Some(tx) = dual_tx.as_ref() {
                        let _ = tx.send(DualEvent::Segment(s));
                    } else if let Some(tx) = seg_tx.as_ref() {
                        let _ = tx.send(s);
                    }
                }
            }
        });

        Ok(Self {
            handle,
            pcm,
            collect,
            segment_rx: seg_rx,
            dual_rx,
        })
    }

    pub fn take_segment_rx(&mut self) -> Option<mpsc::UnboundedReceiver<SpeechSegment>> {
        self.segment_rx.take()
    }

    pub fn take_dual_rx(&mut self) -> Option<mpsc::UnboundedReceiver<DualEvent>> {
        self.dual_rx.take()
    }

    /// Stop capture and return all accumulated PCM.
    pub async fn finish(self) -> Result<Vec<u8>> {
        self.handle.stop();
        self.collect.await.context("collect pcm")?;
        let pcm = self.pcm.lock().await.clone();
        Ok(pcm)
    }
}
