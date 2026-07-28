use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Write mono PCM16 little-endian samples as a WAV file.
pub fn write_pcm16_mono_wav(path: &Path, sample_rate: u32, pcm: &[u8]) -> Result<()> {
    let mut f = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let data_len = pcm.len() as u32;
    let byte_rate = sample_rate * 2; // mono * 16-bit
    let block_align: u16 = 2;
    let bits_per_sample: u16 = 16;
    let channels: u16 = 1;

    // RIFF header
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    // fmt chunk
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // PCM chunk size
    f.write_all(&1u16.to_le_bytes())?; // audio format = PCM
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&bits_per_sample.to_le_bytes())?;
    // data chunk
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    f.write_all(pcm)?;
    f.flush()?;
    Ok(())
}

/// Peak absolute sample and RMS for mono s16le PCM.
pub fn pcm16_levels(pcm: &[u8]) -> (i32, f64) {
    if pcm.len() < 2 {
        return (0, 0.0);
    }
    let n = pcm.len() / 2;
    let mut peak: i32 = 0;
    let mut sum_sq: f64 = 0.0;
    for i in 0..n {
        let s = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]) as i32;
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (s as f64) * (s as f64);
    }
    let rms = (sum_sq / n as f64).sqrt();
    (peak, rms)
}

/// Soft peak normalize toward ~50% full-scale. Helps quiet mics and hard clipping.
pub fn pcm16_normalize(pcm: &[u8]) -> Vec<u8> {
    let (peak, _) = pcm16_levels(pcm);
    if peak < 200 {
        // essentially silence — leave as-is
        return pcm.to_vec();
    }
    let target = 16_000.0_f64; // ~50% of 32767
    let gain = (target / peak as f64).clamp(0.15, 8.0);
    if (gain - 1.0).abs() < 0.08 {
        return pcm.to_vec();
    }
    let n = pcm.len() / 2;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]) as f64;
        let v = (s * gain).round().clamp(-32767.0, 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// First-order high-pass (~120 Hz @ 16 kHz) to cut AC hum / mic bias.
pub fn pcm16_highpass(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let n = pcm.len() / 2;
    if n < 8 {
        return pcm.to_vec();
    }
    let fc = 120.0_f64;
    let dt = 1.0 / sample_rate as f64;
    let rc = 1.0 / (2.0 * std::f64::consts::PI * fc);
    let alpha = rc / (rc + dt);

    let mut out = Vec::with_capacity(n * 2);
    let mut prev_x = 0.0_f64;
    let mut prev_y = 0.0_f64;
    for i in 0..n {
        let x = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]) as f64;
        let y = alpha * (prev_y + x - prev_x);
        prev_x = x;
        prev_y = y;
        let v = y.round().clamp(-32767.0, 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Prepare PCM for ASR: high-pass then normalize.
pub fn pcm16_prepare_for_asr(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let hp = pcm16_highpass(pcm, sample_rate);
    pcm16_normalize(&hp)
}
