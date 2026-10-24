//! Audio plumbing between a WhatsApp call and Gemini Live.
//!
//! Call audio (whatsapp-rust) is 16 kHz mono i16: we push 960-sample (60 ms) frames and receive
//! 320-sample (20 ms) frames. Gemini Live takes 16 kHz s16le and returns 24 kHz s16le.

use std::collections::VecDeque;
use std::sync::Mutex;

pub const FRAME_SAMPLES: usize = 960;

pub fn i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

pub fn le_bytes_to_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

const HALF_TAPS: usize = 16;

/// Streaming 24 kHz -> 16 kHz converter with a windowed-sinc low-pass.
///
/// Output sample n sits at input position 1.5n, so only two filter phases exist (fraction 0 and
/// 0.5) and both are precomputed.
pub struct Downsampler {
    buf: Vec<f32>,
    pos: f64, // input index of the next output sample, relative to buf
    phases: [[f32; 2 * HALF_TAPS]; 2],
}

impl Downsampler {
    pub fn new() -> Self {
        let cutoff = 7400.0 / 24000.0; // just under the 8 kHz Nyquist of the output
        let mut phases = [[0f32; 2 * HALF_TAPS]; 2];
        for (phase, frac) in [0.0f64, 0.5].into_iter().enumerate() {
            let mut w = [0f64; 2 * HALF_TAPS];
            for (k, wk) in w.iter_mut().enumerate() {
                let x = (k as f64 - HALF_TAPS as f64 + 1.0) - frac; // offset from the output position
                let sinc = if x == 0.0 {
                    2.0 * cutoff
                } else {
                    (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let hann = 0.5 + 0.5 * (std::f64::consts::PI * x / HALF_TAPS as f64).cos();
                *wk = sinc * hann;
            }
            let sum: f64 = w.iter().sum();
            for (tap, wk) in phases[phase].iter_mut().zip(w) {
                *tap = (wk / sum) as f32; // unity DC gain
            }
        }
        Self {
            buf: vec![0.0; HALF_TAPS],
            pos: HALF_TAPS as f64,
            phases,
        }
    }

    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        self.buf.extend(input.iter().map(|&s| s as f32));
        let mut out = Vec::with_capacity(input.len() * 2 / 3 + 1);
        loop {
            let base = self.pos as usize;
            if base + HALF_TAPS >= self.buf.len() {
                break;
            }
            let taps = &self.phases[usize::from(self.pos.fract() > 0.0)];
            let start = base + 1 - HALF_TAPS;
            let acc: f32 = self.buf[start..start + 2 * HALF_TAPS]
                .iter()
                .zip(taps)
                .map(|(x, h)| x * h)
                .sum();
            out.push(acc.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
            self.pos += 1.5;
        }
        let drop = (self.pos as usize).saturating_sub(HALF_TAPS);
        if drop > 0 {
            self.buf.drain(..drop);
            self.pos -= drop as f64;
        }
        out
    }
}

/// Streaming 16 kHz -> 24 kHz converter. Grok realtime PCM is 24 kHz; call audio is 16 kHz.
pub struct Upsampler {
    buf: Vec<f32>,
    pos: f64,
}

impl Upsampler {
    pub fn new() -> Self {
        Self {
            buf: vec![0.0],
            pos: 0.0,
        }
    }

    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        self.buf.extend(input.iter().map(|&s| s as f32));
        let step = 2.0 / 3.0;
        let mut out = Vec::with_capacity(input.len() * 3 / 2 + 1);
        while self.pos + 1.0 < self.buf.len() as f64 {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let sample = self.buf[i] * (1.0 - frac) + self.buf[i + 1] * frac;
            out.push(sample.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
            self.pos += step;
        }
        let drop = (self.pos as usize).saturating_sub(1);
        if drop > 0 {
            self.buf.drain(..drop);
            self.pos -= drop as f64;
        }
        out
    }
}

/// Model speech waiting to be played into the call, drained one 60 ms frame per tick.
#[derive(Default)]
pub struct Playout {
    queue: Mutex<VecDeque<i16>>,
}

impl Playout {
    pub fn push(&self, samples: &[i16]) {
        self.queue.lock().unwrap().extend(samples);
    }

    /// Drop queued speech, used when the caller barges in.
    pub fn clear(&self) {
        self.queue.lock().unwrap().clear();
    }

    pub fn buffered(&self) -> usize {
        self.queue.lock().unwrap().len()
    }

    /// The next frame, zero-padded on underrun so the call always gets a steady stream.
    pub fn next_frame(&self) -> Vec<i16> {
        let mut q = self.queue.lock().unwrap();
        let n = q.len().min(FRAME_SAMPLES);
        let mut frame: Vec<i16> = q.drain(..n).collect();
        frame.resize(FRAME_SAMPLES, 0);
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| (16384.0 * (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin()) as i16)
            .collect()
    }

    fn rms(s: &[i16]) -> f64 {
        (s.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
    }

    #[test]
    fn pcm_bytes_round_trip() {
        let s = [0i16, 1, -1, i16::MAX, i16::MIN, 12345];
        assert_eq!(le_bytes_to_i16(&i16_to_le_bytes(&s)), s);
    }

    #[test]
    fn downsampler_ratio_across_uneven_chunks() {
        let mut d = Downsampler::new();
        let input = sine(440.0, 24000.0, 24000);
        let out: Vec<i16> = input.chunks(731).flat_map(|c| d.process(c)).collect();
        assert!((15980..=16000).contains(&out.len()), "got {}", out.len());
    }

    #[test]
    fn downsampler_keeps_passband_and_cuts_aliases() {
        let pass = Downsampler::new().process(&sine(1000.0, 24000.0, 24000));
        let r = rms(&pass[1000..]);
        assert!((10900.0..12300.0).contains(&r), "1 kHz rms {r}"); // 16384/sqrt(2) ≈ 11585
        // 11 kHz would alias to 5 kHz at 16 kHz without the low-pass.
        let stop = Downsampler::new().process(&sine(11000.0, 24000.0, 24000));
        let r = rms(&stop[1000..]);
        assert!(r < 400.0, "11 kHz leaked with rms {r}");
    }

    #[test]
    fn upsampler_ratio_is_three_halves() {
        let mut up = Upsampler::new();
        let input = sine(440.0, 16000.0, 16000);
        let out: Vec<i16> = input.chunks(640).flat_map(|c| up.process(c)).collect();
        assert!((23900..=24010).contains(&out.len()), "got {}", out.len());
        let r = rms(&out[1000..]);
        assert!((10000.0..13000.0).contains(&r), "440 Hz rms {r}");
    }

    #[test]
    fn playout_pads_and_clears() {
        let p = Playout::default();
        assert_eq!(p.next_frame(), vec![0; FRAME_SAMPLES]);
        p.push(&vec![1; 1000]);
        assert_eq!(p.buffered(), 1000);
        assert_eq!(p.next_frame(), vec![1; FRAME_SAMPLES]);
        let tail = p.next_frame();
        assert_eq!(tail.len(), FRAME_SAMPLES);
        assert_eq!(&tail[..40], &[1; 40]);
        assert!(tail[40..].iter().all(|&s| s == 0));
        p.push(&vec![1; 5000]);
        p.clear();
        assert_eq!(p.buffered(), 0);
    }
}
