use anyhow::{Context, Result, bail};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use std::path::Path;

use crate::config::Config;
use crate::proxy;

#[derive(Debug, Deserialize)]
pub struct SttResponse {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub language: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub duration: f64,
}

/// Upload a local audio file to `POST {api_base}/v1/stt`.
pub async fn transcribe_file(cfg: &Config, bearer: &str, path: &Path) -> Result<SttResponse> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;

    let base = cfg.api_base.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let url = format!("{base}/v1/stt");

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("audio.wav")
        .to_string();

    let mut form = Form::new();
    // Fields before file (API requirement for streamable uploads).
    if !cfg.language.is_empty() && cfg.language != "auto" {
        form = form.text("language", cfg.language.clone());
        form = form.text("format", "true");
    } else {
        form = form.text("format", "false");
    }
    for term in &cfg.keyterms {
        if !term.trim().is_empty() {
            form = form.text("keyterm", term.trim().to_string());
        }
    }

    let part = Part::bytes(bytes)
        .file_name(file_name)
        .mime_str("audio/wav")
        .context("mime")?;
    form = form.part("file", part);

    let proxy_url = proxy::resolve_http_proxy(if cfg.proxy.is_empty() {
        None
    } else {
        Some(cfg.proxy.as_str())
    });
    let client = proxy::build_client(proxy_url.as_deref())?;

    let resp = client
        .post(&url)
        .bearer_auth(bearer)
        .header(
            "User-Agent",
            format!("xai-dict/{}", env!("CARGO_PKG_VERSION")),
        )
        .multipart(form)
        .send()
        .await
        .with_context(|| {
            let diag = proxy::diagnose_api_host("api.x.ai");
            match &proxy_url {
                Some(p) => format!("POST /v1/stt via proxy {p} ({diag})"),
                None => format!(
                    "POST /v1/stt with no proxy ({diag}). \
                     Set HTTPS_PROXY or proxy = \"http://127.0.0.1:7897\" in config"
                ),
            }
        })?;

    let status = resp.status();
    let body = resp.text().await.context("read STT response body")?;
    if !status.is_success() {
        bail!("STT HTTP {status}: {body}");
    }

    serde_json::from_str(&body).with_context(|| format!("parse STT JSON: {body}"))
}
