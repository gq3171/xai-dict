//! Simple energy-based VAD that turns a PCM16 mono stream into speech segments.
//!
//! Used for "type-as-you-speak": each finished phrase is ASR'd and committed
//! immediately, instead of waiting for the whole utterance.

/// Defaults tuned for 16 kHz Chinese dictation with Qwen3 (~1–3 s phrases).
pub struct VadConfig {
    pub sample_rate: u32,
    /// Frame length for RMS (ms).
    pub frame_ms: u32,
    /// RMS above this → speech (s16 scale).
    pub speech_rms: f64,
    /// How long silence must last to close a segment (ms).
    pub min_silence_ms: u32,
    /// Drop segments shorter than this (ms).
    pub min_speech_ms: u32,
    /// Force-cut long continuous speech (ms).
    pub max_segment_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            frame_ms: 30,
            speech_rms: 700.0,
            min_silence_ms: 480,
            min_speech_ms: 320,
            max_segment_ms: 3_500,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpeechSegment {
    /// PCM16 mono bytes of one speech phrase (may include a little trailing silence).
    pub pcm: Vec<u8>,
}

enum Phase {
    Silence,
    Speech {
        start_byte: usize,
        /// Consecutive silence frames while still in speech.
        silence_frames: u32,
    },
}

/// Online segmenter: push PCM chunks, pop completed speech segments.
pub struct Segmenter {
    cfg: VadConfig,
    /// All PCM seen so far (bytes).
    buf: Vec<u8>,
    /// Next frame start in bytes.
    cursor: usize,
    phase: Phase,
    frame_bytes: usize,
    min_silence_frames: u32,
    min_speech_bytes: usize,
    max_segment_bytes: usize,
}

impl Segmenter {
    pub fn new(cfg: VadConfig) -> Self {
        let frame_samples = (cfg.sample_rate as usize * cfg.frame_ms as usize) / 1000;
        let frame_bytes = frame_samples * 2;
        let min_silence_frames =
            ((cfg.min_silence_ms as f64) / (cfg.frame_ms as f64)).ceil() as u32;
        let min_speech_bytes =
            (cfg.sample_rate as usize * 2 * cfg.min_speech_ms as usize) / 1000;
        let max_segment_bytes =
            (cfg.sample_rate as usize * 2 * cfg.max_segment_ms as usize) / 1000;
        Self {
            cfg,
            buf: Vec::new(),
            cursor: 0,
            phase: Phase::Silence,
            frame_bytes: frame_bytes.max(2),
            min_silence_frames: min_silence_frames.max(1),
            min_speech_bytes,
            max_segment_bytes: max_segment_bytes.max(frame_bytes * 2),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SpeechSegment> {
        self.buf.extend_from_slice(chunk);
        self.poll()
    }

    /// Flush any open speech as a final segment (call on stop).
    pub fn flush(&mut self) -> Option<SpeechSegment> {
        match self.phase {
            Phase::Speech { start_byte, .. } => {
                let end = self.buf.len();
                self.phase = Phase::Silence;
                self.cursor = end;
                self.make_segment(start_byte, end)
            }
            Phase::Silence => None,
        }
    }

    fn poll(&mut self) -> Vec<SpeechSegment> {
        let mut out = Vec::new();
        while self.cursor + self.frame_bytes <= self.buf.len() {
            let frame = &self.buf[self.cursor..self.cursor + self.frame_bytes];
            let rms = frame_rms(frame);
            let is_speech = rms >= self.cfg.speech_rms;
            let frame_end = self.cursor + self.frame_bytes;

            match self.phase {
                Phase::Silence => {
                    if is_speech {
                        self.phase = Phase::Speech {
                            start_byte: self.cursor,
                            silence_frames: 0,
                        };
                    }
                }
                Phase::Speech {
                    start_byte,
                    silence_frames,
                } => {
                    let speech_len = frame_end.saturating_sub(start_byte);
                    if is_speech {
                        self.phase = Phase::Speech {
                            start_byte,
                            silence_frames: 0,
                        };
                        // Force cut long monologues so ASR stays snappy.
                        if speech_len >= self.max_segment_bytes {
                            if let Some(seg) = self.make_segment(start_byte, frame_end) {
                                out.push(seg);
                            }
                            self.phase = Phase::Silence;
                        }
                    } else {
                        let sf = silence_frames + 1;
                        if sf >= self.min_silence_frames {
                            // End of phrase: cut before this silence run.
                            let silence_bytes = (sf as usize) * self.frame_bytes;
                            let end = frame_end.saturating_sub(silence_bytes);
                            let end = end.max(start_byte);
                            if let Some(seg) = self.make_segment(start_byte, end) {
                                out.push(seg);
                            }
                            self.phase = Phase::Silence;
                        } else {
                            self.phase = Phase::Speech {
                                start_byte,
                                silence_frames: sf,
                            };
                        }
                    }
                }
            }
            self.cursor = frame_end;
        }
        out
    }

    fn make_segment(&self, start: usize, end: usize) -> Option<SpeechSegment> {
        if end <= start {
            return None;
        }
        let len = end - start;
        if len < self.min_speech_bytes {
            return None;
        }
        // Peak check — skip near-silence.
        let slice = &self.buf[start..end];
        let (peak, _) = crate::wav::pcm16_levels(slice);
        if peak < 400 {
            return None;
        }
        Some(SpeechSegment {
            pcm: slice.to_vec(),
        })
    }
}

fn frame_rms(frame: &[u8]) -> f64 {
    if frame.len() < 2 {
        return 0.0;
    }
    let n = frame.len() / 2;
    let mut sum_sq = 0.0;
    for i in 0..n {
        let s = i16::from_le_bytes([frame[i * 2], frame[i * 2 + 1]]) as f64;
        sum_sq += s * s;
    }
    (sum_sq / n as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(samples: usize, amp: i16) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            // crude square-ish
            let s = if (i / 40) % 2 == 0 { amp } else { -amp };
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn silence(samples: usize) -> Vec<u8> {
        vec![0u8; samples * 2]
    }

    #[test]
    fn segments_on_silence() {
        let mut s = Segmenter::new(VadConfig {
            sample_rate: 16_000,
            frame_ms: 30,
            speech_rms: 500.0,
            min_silence_ms: 300,
            min_speech_ms: 200,
            max_segment_ms: 10_000,
        });
        // 0.5s speech + 0.5s silence
        let segs = s.push(&tone(8000, 8000));
        assert!(segs.is_empty(), "no segment until silence");
        let segs = s.push(&silence(8000));
        assert_eq!(segs.len(), 1);
        assert!(segs[0].pcm.len() > 1000);
    }
}
