//! Small offline DSP blocks for the conditioning chain:
//! a high-pass biquad, a feed-forward compressor, a
//! look-ahead peak limiter and level measurement. Everything works on whole
//! utterances held in memory; nothing here is streaming.

use std::f64::consts::PI;

// ── level ────────────────────────────────────────────────────────────────

/// Peak in dBFS, or `None` for digital silence.
pub fn peak_dbfs(x: &[f32]) -> Option<f32> {
    let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
    if peak <= 0.0 {
        None
    } else {
        Some(20.0 * peak.log10())
    }
}

/// Integrated loudness in LUFS per EBU R128 (gated), or `None` when the
/// signal is too short or too quiet for the gate to pass any block.
pub fn lufs(x: &[f32], sample_rate: u32) -> Option<f32> {
    let mut m = ebur128::EbuR128::new(1, sample_rate, ebur128::Mode::I).ok()?;
    m.add_frames_f32(x).ok()?;
    let l = m.loudness_global().ok()?;
    if l.is_finite() {
        Some(l as f32)
    } else {
        None
    }
}

/// Speech-weighted level for short clips where the R128 gate cannot form a
/// block: mean power of the 100 ms windows within 20 dB of the loudest,
/// expressed in dBFS. `None` for silence.
pub fn active_rms_dbfs(x: &[f32], sample_rate: u32) -> Option<f32> {
    let win = (sample_rate as usize / 10).max(1);
    let mut powers: Vec<f32> = x
        .chunks(win)
        .filter(|c| c.len() >= win / 2)
        .map(|c| c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32)
        .collect();
    if powers.is_empty() {
        powers.push(x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32);
    }
    let max = powers.iter().cloned().fold(0f32, f32::max);
    if max <= 0.0 {
        return None;
    }
    let floor = max / 100.0; // -20 dB
    let (sum, n) = powers
        .iter()
        .filter(|p| **p >= floor)
        .fold((0f32, 0usize), |(s, n), p| (s + p, n + 1));
    Some(10.0 * (sum / n as f32).log10())
}

/// Loudness in LUFS, falling back to the active-RMS estimate (which is
/// within a couple of dB of LUFS for narrowband speech) for short clips.
pub fn loudness(x: &[f32], sample_rate: u32) -> Option<f32> {
    lufs(x, sample_rate).or_else(|| active_rms_dbfs(x, sample_rate))
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn apply_gain(x: &mut [f32], gain: f32) {
    for v in x.iter_mut() {
        *v *= gain;
    }
}

// ── filters ──────────────────────────────────────────────────────────────

/// Second-order Butterworth high-pass (RBJ cookbook), applied in place.
pub fn highpass(x: &mut [f32], sample_rate: u32, cutoff_hz: f32) {
    if cutoff_hz <= 0.0 {
        return;
    }
    let w0 = 2.0 * PI * cutoff_hz as f64 / sample_rate as f64;
    let (sn, cs) = w0.sin_cos();
    let alpha = sn / (2.0 * std::f64::consts::FRAC_1_SQRT_2); // Q = 1/sqrt(2)
    let b0 = (1.0 + cs) / 2.0;
    let b1 = -(1.0 + cs);
    let b2 = (1.0 + cs) / 2.0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cs;
    let a2 = 1.0 - alpha;
    let (b0, b1, b2, a1, a2) = (b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0);
    let (mut x1, mut x2, mut y1, mut y2) = (0f64, 0f64, 0f64, 0f64);
    for v in x.iter_mut() {
        let x0 = *v as f64;
        let y0 = b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        x2 = x1;
        x1 = x0;
        y2 = y1;
        y1 = y0;
        *v = y0 as f32;
    }
}

// ── dynamics ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Compressor {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl Compressor {
    /// Feed-forward compressor with a soft knee; gain computed in the dB
    /// domain from the instantaneous level and smoothed with separate attack
    /// and release constants. No make-up gain: the loudness stage follows.
    pub fn apply(&self, x: &mut [f32], sample_rate: u32) {
        if self.ratio <= 1.0 {
            return;
        }
        let coef = |ms: f32| (-1.0 / (ms.max(0.01) * sample_rate as f32 / 1000.0)).exp();
        let a_att = coef(self.attack_ms);
        let a_rel = coef(self.release_ms);
        let slope = 1.0 - 1.0 / self.ratio;
        let knee = self.knee_db.max(0.0);
        let mut env_db = 0f32; // current gain reduction, <= 0
        for v in x.iter_mut() {
            let level_db = 20.0 * (v.abs() + 1e-9).log10();
            let over = level_db - self.threshold_db;
            let target = if over <= -knee / 2.0 {
                0.0
            } else if knee > 0.0 && over < knee / 2.0 {
                -slope * (over + knee / 2.0).powi(2) / (2.0 * knee)
            } else {
                -slope * over
            };
            env_db = if target < env_db {
                a_att * env_db + (1.0 - a_att) * target
            } else {
                a_rel * env_db + (1.0 - a_rel) * target
            };
            *v *= db_to_gain(env_db);
        }
    }
}

/// Look-ahead peak limiter: the gain needed to keep every sample within the
/// next `lookahead_ms` under `ceiling` is applied ahead of the peak and
/// released slowly afterwards. A final hard clip guarantees the ceiling.
pub fn limit(x: &mut [f32], sample_rate: u32, ceiling_dbfs: f32, lookahead_ms: f32, release_ms: f32) {
    let ceiling = db_to_gain(ceiling_dbfs);
    let n = x.len();
    if n == 0 {
        return;
    }
    let la = ((lookahead_ms * sample_rate as f32 / 1000.0) as usize).max(1);
    // Required gain per sample, then a sliding minimum over [i, i+la).
    let req: Vec<f32> = x.iter().map(|v| (ceiling / v.abs().max(1e-12)).min(1.0)).collect();
    let mut gmin = vec![1f32; n];
    let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    // Process from the end so the window looks forward.
    for i in (0..n).rev() {
        while let Some(&back) = dq.back() {
            if req[back] >= req[i] {
                dq.pop_back();
            } else {
                break;
            }
        }
        dq.push_back(i);
        while let Some(&front) = dq.front() {
            if front >= i + la {
                dq.pop_front();
            } else {
                break;
            }
        }
        gmin[i] = req[*dq.front().unwrap()];
    }
    let a_att = (-1.0 / (la as f32 / 4.0)).exp();
    let a_rel = (-1.0 / (release_ms.max(1.0) * sample_rate as f32 / 1000.0)).exp();
    let mut g = 1f32;
    for (v, target) in x.iter_mut().zip(gmin) {
        g = if target < g {
            a_att * g + (1.0 - a_att) * target
        } else {
            a_rel * g + (1.0 - a_rel) * target
        };
        *v = (*v * g).clamp(-ceiling, ceiling);
    }
}

// ── conversion ───────────────────────────────────────────────────────────

pub fn to_f32(x: &[i16]) -> Vec<f32> {
    x.iter().map(|s| *s as f32 / 32768.0).collect()
}

pub fn to_i16(x: &[f32]) -> Vec<i16> {
    x.iter()
        .map(|v| (v * 32767.0).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, rate: u32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| amp * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn highpass_removes_hum_keeps_voice() {
        let mut hum = tone(50.0, 8000, 1.0, 0.5);
        let mut voice = tone(1000.0, 8000, 1.0, 0.5);
        highpass(&mut hum, 8000, 250.0);
        highpass(&mut voice, 8000, 250.0);
        assert!(peak_dbfs(&hum[4000..]).unwrap() < -30.0);
        assert!(peak_dbfs(&voice[4000..]).unwrap() > -7.0); // -6 dBFS tone, ~0.1 dB loss
    }

    #[test]
    fn compressor_reduces_loud_more_than_quiet() {
        let c = Compressor { threshold_db: -24.0, ratio: 3.0, knee_db: 6.0, attack_ms: 5.0, release_ms: 150.0 };
        let mut loud = tone(1000.0, 8000, 1.0, 0.5); // -6 dBFS
        let mut quiet = tone(1000.0, 8000, 1.0, 0.02); // -34 dBFS
        c.apply(&mut loud, 8000);
        c.apply(&mut quiet, 8000);
        let l = peak_dbfs(&loud[4000..]).unwrap();
        let q = peak_dbfs(&quiet[4000..]).unwrap();
        assert!(l < -12.0 && l > -20.0, "loud -> {l}");
        assert!((q - (-34.0)).abs() < 1.0, "quiet -> {q}");
    }

    #[test]
    fn limiter_holds_ceiling() {
        let mut x = tone(1000.0, 8000, 0.5, 2.0);
        limit(&mut x, 8000, -1.0, 5.0, 50.0);
        assert!(peak_dbfs(&x).unwrap() <= -0.99);
    }

    #[test]
    fn loudness_of_tone_is_sane() {
        let x = tone(1000.0, 8000, 2.0, 0.1); // -20 dBFS peak, -23 dBFS rms
        let l = loudness(&x, 8000).unwrap();
        assert!(l > -27.0 && l < -19.0, "{l}");
        let short = tone(1000.0, 8000, 0.2, 0.1);
        assert!(loudness(&short, 8000).is_some());
        assert!(loudness(&vec![0.0; 800], 8000).is_none());
    }
}
