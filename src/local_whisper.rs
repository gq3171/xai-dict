use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Transcribe a WAV file with system `whisper-cli` (whisper.cpp).
pub fn transcribe_file(
    wav: &Path,
    model: &Path,
    language: &str,
    threads: u32,
    // Force Traditional→Simplified when Chinese.
    to_simplified: bool,
) -> Result<String> {
    if !model.is_file() {
        bail!(
            "local whisper model not found: {}\n\
             Download e.g.:\n  mkdir -p ~/.local/share/xai-dict/models\n  \
             curl -fL -o ~/.local/share/xai-dict/models/ggml-small.bin \\\n    \
             https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
            model.display()
        );
    }

    let whisper = which_whisper_cli().context(
        "whisper-cli not found — install with: sudo pacman -S whisper-cpp",
    )?;

    let lang = if language.is_empty() || language == "auto" {
        "auto"
    } else {
        language
    };

    // Bias decoder toward Simplified Chinese when language is zh.
    let zh_prompt = "以下是普通话的简体中文句子。";

    let mut cmd = Command::new(&whisper);
    cmd.args([
        "-m",
        model.to_str().context("model path utf8")?,
        "-f",
        wav.to_str().context("wav path utf8")?,
        "-l",
        lang,
        "-nt",
        "-np",
        "-t",
        &threads.to_string(),
        // Suppress some silence hallucinations
        "-nth",
        "0.6",
    ]);
    if lang == "zh" || lang == "chinese" {
        cmd.args(["--prompt", zh_prompt]);
    }

    let output = cmd
        .output()
        .with_context(|| format!("run {}", whisper.display()))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("whisper-cli failed ({}): {}", output.status, err.trim());
    }

    // Result is on stdout; stderr has backend noise.
    let text = String::from_utf8_lossy(&output.stdout);
    let mut cleaned = clean_transcript(&text);

    // Whisper often mixes 繁体/简体; normalize to 简体 for zh.
    if to_simplified && looks_chinese(&cleaned) {
        if let Ok(s) = traditional_to_simplified(&cleaned) {
            cleaned = s;
        }
    }

    Ok(cleaned)
}

fn looks_chinese(s: &str) -> bool {
    s.chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// Convert Traditional → Simplified via system `opencc -c t2s.json` (or t2s).
fn traditional_to_simplified(text: &str) -> Result<String> {
    if text.is_empty() {
        return Ok(String::new());
    }
    if !which_bin("opencc") {
        return Ok(text.to_string());
    }

    // Prefer config name that exists on Arch opencc package.
    for cfg in ["t2s.json", "t2s", "tw2s.json", "tw2sp.json"] {
        let mut child = match Command::new("opencc")
            .args(["-c", cfg])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(text.as_bytes()).is_err() {
                continue;
            }
        }
        let out = match child.wait_with_output() {
            Ok(o) if o.status.success() => o,
            _ => continue,
        };
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    Ok(text.to_string())
}

fn which_bin(name: &str) -> bool {
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

fn clean_transcript(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("ggml_")
                && !l.starts_with("load_backend")
                && !l.starts_with("whisper_")
                && !l.starts_with("system_info")
                && !l.starts_with("main:")
                && !l.contains("processing")
        })
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn which_whisper_cli() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["whisper-cli", "whisper-cpp", "main"] {
            let p = dir.join(name);
            if p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
            {
                return Some(p);
            }
        }
    }
    // common install path
    let p = PathBuf::from("/usr/bin/whisper-cli");
    if p.is_file() {
        return Some(p);
    }
    None
}

pub fn default_model_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("xai-dict")
        .join("models")
        .join("ggml-small.bin")
}
