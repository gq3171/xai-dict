use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

/// Resolve a bearer token for xAI STT.
/// Priority: CLI flag / env (`XAI_API_KEY`, `GROK_API_KEY`) → `~/.grok/auth.json`.
pub fn resolve_bearer(cli_key: Option<&str>) -> Result<String> {
    if let Some(k) = cli_key.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(k.to_string());
    }
    for var in ["XAI_API_KEY", "GROK_API_KEY", "GROK_VOICE_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    read_grok_auth_json()
}

fn auth_json_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grok")
        .join("auth.json")
}

fn read_grok_auth_json() -> Result<String> {
    let path = auth_json_path();
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read {} (or set XAI_API_KEY)", path.display()))?;
    let v: Value = serde_json::from_str(&raw).context("parse ~/.grok/auth.json")?;

    // Shape: { "<scope_url>": { "key": "...", ... }, ... }
    if let Some(obj) = v.as_object() {
        // Prefer OIDC auth.x.ai scope entries that look like access tokens.
        let mut candidates: Vec<&str> = Vec::new();
        for (scope, entry) in obj {
            if let Some(key) = entry.get("key").and_then(|k| k.as_str()) {
                if !key.is_empty() {
                    if scope.contains("auth.x.ai") {
                        candidates.insert(0, key);
                    } else {
                        candidates.push(key);
                    }
                }
            }
        }
        if let Some(k) = candidates.first() {
            return Ok((*k).to_string());
        }
    }

    // Flat fallbacks
    for field in ["access_token", "token", "api_key", "key"] {
        if let Some(k) = v.get(field).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
            return Ok(k.to_string());
        }
    }

    bail!(
        "no usable token in {} and XAI_API_KEY is unset — run `grok login` or export XAI_API_KEY",
        path.display()
    );
}
