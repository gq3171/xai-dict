use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Local Qwen3-ASR via sherpa-onnx (default) — no network
    Qwen3,
    /// Local whisper.cpp (`whisper-cli`) — no network
    Local,
    /// xAI cloud STT REST
    Xai,
}

impl Default for Provider {
    fn default() -> Self {
        Self::Qwen3
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `qwen3` (sherpa-onnx Qwen3-ASR, default), `local` (whisper.cpp), or `xai` (cloud)
    pub provider: Provider,
    /// HTTPS API root for xAI, e.g. https://api.x.ai
    pub api_base: String,
    pub stt_path: String,
    /// STT language code or "auto"
    pub language: String,
    pub sample_rate: u32,
    pub interim_results: bool,
    pub endpointing_ms: u32,
    pub keyterms: Vec<String>,
    pub paste: bool,
    /// HTTP proxy for cloud provider (empty = auto-detect)
    pub proxy: String,
    /// Path to whisper.cpp ggml model (local provider)
    pub local_model: String,
    /// Threads for local ASR (whisper / qwen3)
    pub local_threads: u32,
    /// Dir with conv_frontend/encoder/decoder/tokenizer for Qwen3-ASR
    pub qwen3_model_dir: String,
    /// Max new tokens for Qwen3-ASR decoder
    pub qwen3_max_new_tokens: u32,
    /// Optional comma-separated hotwords for Qwen3-ASR
    pub qwen3_hotwords: String,
    /// Global hotkey for daemon: `rightalt` (default), `leftalt`, or `none`
    pub hotkey: String,
    /// How the hotkey behaves: `toggle` (press start/stop) or `ptt` (hold to talk).
    #[serde(default = "default_hotkey_mode")]
    pub hotkey_mode: String,
    /// Optional mic source (PipeWire node name / Pulse source). Empty = system default.
    #[serde(default)]
    pub input_device: String,
    /// Commit phrases while still recording (type-as-you-speak).
    /// Uses energy VAD + per-phrase ASR. Best with local qwen3/whisper.
    pub stream: bool,
    /// Silence (ms) that ends a speech phrase while streaming.
    pub stream_min_silence_ms: u32,
    /// Force-cut a long monologue into chunks (ms).
    pub stream_max_segment_ms: u32,
    /// Ignore phrases shorter than this (ms).
    pub stream_min_speech_ms: u32,
    /// Dual-model: Zipformer streaming preedit + Qwen3 final commit.
    pub dual_model: bool,
    /// Show Zipformer text as fcitx preedit while speaking.
    /// Final commit is always Qwen3; preedit is provisional and filtered for stability.
    /// Set false if live text jumps too much vs the final result.
    #[serde(default = "default_true")]
    pub dual_preedit: bool,
    /// Streaming Zipformer model directory (zh-int8 online transducer).
    pub stream_model_dir: String,
    /// Threads for the streaming Zipformer worker (keep low; real-time).
    pub stream_threads: u32,
    /// Prefer close-talk mic; reject quieter bystander / far-field speech.
    /// Limits AGC boost and raises VAD energy gates. Keep true for office/open space.
    #[serde(default = "default_true")]
    pub near_field: bool,
    /// Absolute VAD speech RMS (s16 scale, after light AGC). 0 = auto from near_field.
    #[serde(default)]
    pub vad_speech_rms: f64,
    /// Speech must be this many × ambient noise floor (adaptive). 0 = auto.
    #[serde(default)]
    pub vad_snr: f64,
    /// Max software mic gain. 0 = auto (near_field → 4×, else 12×). Cap stops amplifying bystanders.
    #[serde(default)]
    pub agc_max_gain: f64,
}

fn default_true() -> bool {
    true
}

fn default_hotkey_mode() -> String {
    "toggle".into()
}

impl Default for Config {
    fn default() -> Self {
        let model = crate::local_whisper::default_model_path()
            .to_string_lossy()
            .into_owned();
        let qwen3 = crate::local_qwen3::default_model_dir()
            .to_string_lossy()
            .into_owned();
        Self {
            provider: Provider::Qwen3,
            api_base: "https://api.x.ai".into(),
            stt_path: "/v1/stt".into(),
            language: "zh".into(),
            sample_rate: 16_000,
            interim_results: true,
            endpointing_ms: 400,
            keyterms: Vec::new(),
            paste: true,
            proxy: "http://127.0.0.1:7897".into(),
            local_model: model,
            local_threads: std::thread::available_parallelism()
                .map(|n| n.get().min(8) as u32)
                .unwrap_or(4),
            qwen3_model_dir: qwen3,
            // 128 is enough for phrase streaming; lower = slightly faster decode.
            qwen3_max_new_tokens: 128,
            qwen3_hotwords: String::new(),
            hotkey: "rightalt".into(),
            hotkey_mode: default_hotkey_mode(),
            input_device: String::new(),
            stream: true,
            // Prefer accuracy: wait for a real pause; avoid mid-sentence force-cuts.
            stream_min_silence_ms: 600,
            // Longer force-cut: 6s mid-sentence cuts raised CER on continuous speech.
            stream_max_segment_ms: 12_000,
            stream_min_speech_ms: 280,
            dual_model: true,
            // Paraformer streaming is accurate enough to re-enable live preedit.
            dual_preedit: true,
            stream_model_dir: crate::local_stream::default_model_dir()
                .to_string_lossy()
                .into_owned(),
            // Paraformer benefits from a few more threads than tiny Zipformer.
            stream_threads: 3,
            near_field: true,
            vad_speech_rms: 0.0,
            vad_snr: 0.0,
            agc_max_gain: 0.0,
        }
    }
}

impl Config {
    /// Effective VAD absolute RMS gate.
    pub fn effective_vad_speech_rms(&self) -> f64 {
        if self.vad_speech_rms > 0.0 {
            self.vad_speech_rms
        } else if self.near_field {
            // Mild: still above room murmur, not so high that soft speech is chopped.
            520.0
        } else {
            320.0
        }
    }

    /// Effective speech-to-noise ratio gate (1.0 = disabled).
    pub fn effective_vad_snr(&self) -> f64 {
        if self.vad_snr > 0.0 {
            self.vad_snr
        } else if self.near_field {
            2.4
        } else {
            1.6
        }
    }

    /// Effective AGC max gain.
    pub fn effective_agc_max_gain(&self) -> f64 {
        if self.agc_max_gain > 0.0 {
            self.agc_max_gain.clamp(1.0, 32.0)
        } else if self.near_field {
            // Enough for quiet headsets; not 32× (that lifted bystanders).
            6.0
        } else {
            12.0
        }
    }

    /// Min peak (s16) for a committed phrase after AGC.
    pub fn effective_min_segment_peak(&self) -> i32 {
        if self.near_field {
            900
        } else {
            350
        }
    }

    /// True when hotkey is push-to-talk (hold).
    pub fn is_ptt(&self) -> bool {
        matches!(
            self.hotkey_mode.trim().to_ascii_lowercase().as_str(),
            "ptt" | "hold" | "push" | "pushtotalk" | "push-to-talk"
        )
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("xai-dict")
            .join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        let mut cfg = Self::default();
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(file_cfg) = toml::from_str::<Config>(&raw) {
                cfg = file_cfg;
            }
        }
        cfg
    }

    /// Always rewrite config so new fields (provider/local_model) appear.
    pub fn write_default_if_missing(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            let raw = toml::to_string_pretty(self).unwrap_or_default();
            fs::write(path, raw)?;
        }
        Ok(())
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self).unwrap_or_default();
        fs::write(path, raw)
    }
}
