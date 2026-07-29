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

/// Apply a linear gain to mono s16le PCM (clamped).
pub fn pcm16_apply_gain(pcm: &[u8], gain: f64) -> Vec<u8> {
    if pcm.len() < 2 || (gain - 1.0).abs() < 0.02 {
        return pcm.to_vec();
    }
    // Allow attenuation below 1.0 for hot/clipping mics.
    let gain = gain.clamp(0.05, 64.0);
    let n = pcm.len() / 2;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]) as f64;
        let v = (s * gain).round().clamp(-32767.0, 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Soft peak normalize toward ~50% full-scale. Helps quiet mics and hard clipping.
///
/// Quiet-but-alive mics (peak 80–2000) get strong boost; near-silence (<80) is left alone
/// so we don't invent speech from electrical noise.
pub fn pcm16_normalize(pcm: &[u8]) -> Vec<u8> {
    let (peak, _) = pcm16_levels(pcm);
    if peak < 80 {
        // essentially silence — leave as-is (caller may still try ASR after stream AGC)
        return pcm.to_vec();
    }
    let target = 16_000.0_f64; // ~50% of 32767
    // Up to ~40× for very quiet headset mics (peak ~400 → ~16k).
    let gain = (target / peak as f64).clamp(0.15, 40.0);
    if (gain - 1.0).abs() < 0.08 {
        return pcm.to_vec();
    }
    pcm16_apply_gain(pcm, gain)
}

/// Streaming AGC so VAD / Zipformer see usable levels on quiet USB mics.
///
/// Tracks a short peak EMA and slowly raises gain toward `target_peak`.
/// Fast release when the signal is already loud (avoids clipping laptop mics).
#[derive(Debug, Clone)]
pub struct StreamAgc {
    peak_ema: f64,
    gain: f64,
    target_peak: f64,
    max_gain: f64,
    /// Chunks seen (for logging once).
    chunks: u32,
}

impl Default for StreamAgc {
    fn default() -> Self {
        Self::with_max_gain(4.0)
    }
}

impl StreamAgc {
    pub fn with_max_gain(max_gain: f64) -> Self {
        Self {
            peak_ema: 0.0,
            gain: 1.0,
            // Aim below full-scale; many laptop mics already clip at 32767.
            target_peak: 12_000.0,
            max_gain: max_gain.clamp(1.0, 32.0),
            chunks: 0,
        }
    }

    pub fn process(&mut self, chunk: &[u8]) -> Vec<u8> {
        let (peak, _) = pcm16_levels(chunk);
        self.chunks = self.chunks.saturating_add(1);
        if peak > 0 {
            let p = peak as f64;
            self.peak_ema = if self.peak_ema <= 1.0 {
                p
            } else {
                0.85 * self.peak_ema + 0.15 * p
            };
            // Hot mic: actively attenuate so ASR is not fed hard-clipped PCM.
            let desired = if self.peak_ema > 28_000.0 {
                (12_000.0 / self.peak_ema).clamp(0.15, 1.0)
            } else {
                (self.target_peak / self.peak_ema.max(1.0)).clamp(1.0, self.max_gain)
            };
            if desired < self.gain {
                self.gain = desired; // fast release / attenuation
            } else {
                self.gain = 0.9 * self.gain + 0.1 * desired;
            }
            if self.chunks == 8
                || (self.chunks % 50 == 0 && ((self.gain - 1.0).abs() > 0.15 || peak > 30000))
            {
                tracing::info!(
                    peak_chunk = peak,
                    peak_ema = format!("{:.0}", self.peak_ema),
                    gain = format!("{:.2}", self.gain),
                    "mic AGC"
                );
            }
        }
        pcm16_apply_gain(chunk, self.gain)
    }
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
