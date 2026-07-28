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
    /// Streaming Zipformer model directory (zh-int8 online transducer).
    pub stream_model_dir: String,
    /// Threads for the streaming Zipformer worker (keep low; real-time).
    pub stream_threads: u32,
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
            stream: true,
            // Prefer accuracy: wait for a real pause; avoid mid-sentence force-cuts.
            stream_min_silence_ms: 550,
            stream_max_segment_ms: 12_000,
            stream_min_speech_ms: 320,
            dual_model: true,
            stream_model_dir: crate::local_stream::default_model_dir()
                .to_string_lossy()
                .into_owned(),
            stream_threads: 2,
        }
    }
}

impl Config {
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
