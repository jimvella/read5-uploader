//! Conditioning: the "clean" variant of an utterance.
//!
//! Chain, on the whole utterance: a pre-gain to a consistent level,
//! high-pass, compressor, loudness normalisation, look-ahead limiter. The raw
//! encode is untouched; this only ever adds a second file.

use anyhow::Result;

use super::dsp;

/// Bump when the chain changes in a way that makes old `clean` entries not
/// comparable with new ones. Recorded in the manifest and OpusTags.
pub const CHAIN_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct ConditionParams {
    /// Level the utterance is brought to before the dynamics, in LUFS.
    pub pre_gain_lufs: f32,
    pub highpass_hz: f32,
    pub comp_threshold_db: f32,
    pub comp_ratio: f32,
    pub comp_attack_ms: f32,
    pub comp_release_ms: f32,
    pub target_lufs: f32,
    pub peak_limit_dbfs: f32,
}

impl Default for ConditionParams {
    fn default() -> Self {
        Self {
            pre_gain_lufs: -20.0,
            highpass_hz: 250.0,
            comp_threshold_db: -24.0,
            comp_ratio: 3.0,
            comp_attack_ms: 5.0,
            comp_release_ms: 150.0,
            target_lufs: -18.0,
            peak_limit_dbfs: -1.0,
        }
    }
}

impl ConditionParams {
    /// Short human-readable identity of the chain, e.g.
    /// `hpf250+comp-24/3+lufs-18`. Stored with every clean file.
    pub fn chain(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.highpass_hz > 0.0 {
            parts.push(format!("hpf{}", fmt_num(self.highpass_hz)));
        }
        if self.comp_ratio > 1.0 {
            parts.push(format!("comp{}/{}", fmt_num(self.comp_threshold_db), fmt_num(self.comp_ratio)));
        }
        parts.push(format!("lufs{}", fmt_num(self.target_lufs)));
        parts.join("+")
    }
}

fn fmt_num(v: f32) -> String {
    if (v - v.round()).abs() < 1e-3 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.2}").trim_end_matches('0').to_string()
    }
}

/// The result of conditioning one utterance, at the input sample rate.
#[derive(Debug, Clone)]
pub struct Conditioned {
    pub samples: Vec<i16>,
    pub peak_dbfs: Option<f32>,
    pub lufs: Option<f32>,
    /// Loudness of the input, for the log.
    pub input_lufs: Option<f32>,
}

pub struct Conditioner {
    params: ConditionParams,
    chain: String,
}

impl Conditioner {
    pub fn new(params: ConditionParams) -> Self {
        Self { chain: params.chain(), params }
    }

    pub fn chain(&self) -> &str {
        &self.chain
    }

    /// Condition one mono utterance.
    pub fn condition(&self, pcm: &[i16], sample_rate: u32) -> Result<Conditioned> {
        if sample_rate == 0 {
            anyhow::bail!("sample rate 0");
        }
        let p = &self.params;
        let mut x = dsp::to_f32(pcm);
        let input_lufs = dsp::loudness(&x, sample_rate);
        let Some(in_l) = input_lufs else {
            // Digital silence: nothing to condition.
            return Ok(Conditioned { samples: vec![0; pcm.len()], peak_dbfs: None, lufs: None, input_lufs: None });
        };

        // 1. Pre-gain to a consistent level, never clipping.
        let mut pre_db = (p.pre_gain_lufs - in_l).clamp(-20.0, 30.0);
        if let Some(pk) = dsp::peak_dbfs(&x) {
            pre_db = pre_db.min(-1.0 - pk);
        }
        dsp::apply_gain(&mut x, dsp::db_to_gain(pre_db));

        // 2. Tone and dynamics.
        dsp::highpass(&mut x, sample_rate, p.highpass_hz);
        dsp::Compressor {
            threshold_db: p.comp_threshold_db,
            ratio: p.comp_ratio,
            knee_db: 6.0,
            attack_ms: p.comp_attack_ms,
            release_ms: p.comp_release_ms,
        }
        .apply(&mut x, sample_rate);

        // 3. Loudness to target, then hold the ceiling.
        if let Some(l) = dsp::loudness(&x, sample_rate) {
            let g = (p.target_lufs - l).clamp(-30.0, 40.0);
            dsp::apply_gain(&mut x, dsp::db_to_gain(g));
        }
        dsp::limit(&mut x, sample_rate, p.peak_limit_dbfs, 5.0, 50.0);

        Ok(Conditioned {
            peak_dbfs: dsp::peak_dbfs(&x),
            lufs: dsp::loudness(&x, sample_rate),
            samples: dsp::to_i16(&x),
            input_lufs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_names() {
        assert_eq!(ConditionParams::default().chain(), "hpf250+comp-24/3+lufs-18");
        let p = ConditionParams { comp_ratio: 1.0, highpass_hz: 0.0, ..Default::default() };
        assert_eq!(p.chain(), "lufs-18");
    }

    #[test]
    fn chain_normalises_level() {
        let c = Conditioner::new(ConditionParams::default());
        let quiet: Vec<i16> = (0..16000)
            .map(|i| (300.0 * (2.0 * std::f32::consts::PI * 800.0 * i as f32 / 8000.0).sin()) as i16)
            .collect();
        let out = c.condition(&quiet, 8000).unwrap();
        assert_eq!(out.samples.len(), quiet.len());
        let l = out.lufs.unwrap();
        assert!((l - -18.0).abs() < 2.0, "lufs {l}");
        assert!(out.peak_dbfs.unwrap() <= -0.9);
        let silent = c.condition(&vec![0; 8000], 8000).unwrap();
        assert!(silent.peak_dbfs.is_none());
    }
}
