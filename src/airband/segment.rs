//! Utterances from a stream whose squelch-closed samples are exactly zero.

#[derive(Debug, Clone, Copy)]
pub struct SegmentParams {
    /// A zero run this long (samples) ends an utterance; shorter gaps stay
    /// inside it as silence.
    pub split_gap: usize,
    /// Utterances shorter than this (samples) are dropped as squelch blips.
    pub min_len: usize,
    /// Longer than this (samples) is cut, and the rest continues as a new
    /// utterance (a stuck carrier).
    pub max_len: usize,
}

impl SegmentParams {
    pub fn from_ms(rate: u32, split_gap_ms: u32, min_ms: u32, max_ms: u32) -> Self {
        let s = |ms: u32| (ms as u64 * rate as u64 / 1000) as usize;
        Self { split_gap: s(split_gap_ms).max(1), min_len: s(min_ms), max_len: s(max_ms).max(1) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    /// Timeline index of the first sample (the first non-zero one).
    pub n: u64,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentStats {
    pub utterances: u64,
    pub blips: u64,
    pub cut: u64,
}

struct Open {
    n: u64,
    /// Samples up to the last non-zero one.
    buf: Vec<f32>,
    /// Zeros seen since then, appended only if the utterance continues.
    zeros: usize,
}

pub struct Segmenter {
    p: SegmentParams,
    open: Option<Open>,
    next_n: u64,
    pub stats: SegmentStats,
}

impl Segmenter {
    pub fn new(p: SegmentParams) -> Self {
        Self { p, open: None, next_n: 0, stats: SegmentStats::default() }
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn audio(&mut self, n: u64, samples: &[f32], out: &mut Vec<Utterance>) {
        self.align(n, out);
        for (i, &s) in samples.iter().enumerate() {
            let at = n + i as u64;
            if s != 0.0 {
                match &mut self.open {
                    Some(o) => {
                        if o.zeros > 0 {
                            o.buf.resize(o.buf.len() + o.zeros, 0.0);
                            o.zeros = 0;
                        }
                        o.buf.push(s);
                        if o.buf.len() >= self.p.max_len {
                            self.stats.cut += 1;
                            self.close(out);
                        }
                    }
                    None => self.open = Some(Open { n: at, buf: vec![s], zeros: 0 }),
                }
            } else if let Some(o) = &mut self.open {
                o.zeros += 1;
                if o.zeros >= self.p.split_gap {
                    self.close(out);
                }
            }
        }
        self.next_n = n + samples.len() as u64;
    }

    /// `count` samples that never arrived: silence.
    pub fn lost(&mut self, n: u64, count: u64, out: &mut Vec<Utterance>) {
        self.align(n, out);
        self.silence(count, out);
        self.next_n = n + count;
    }

    /// Close whatever is open (timeline break or shutdown).
    pub fn flush(&mut self, out: &mut Vec<Utterance>) {
        self.close(out);
        self.next_n = 0;
    }

    fn align(&mut self, n: u64, out: &mut Vec<Utterance>) {
        if n > self.next_n {
            self.silence(n - self.next_n, out);
        } else if n < self.next_n {
            self.close(out);
        }
    }

    fn silence(&mut self, count: u64, out: &mut Vec<Utterance>) {
        if let Some(o) = &mut self.open {
            o.zeros = o.zeros.saturating_add(count as usize);
            if o.zeros >= self.p.split_gap {
                self.close(out);
            }
        }
    }

    fn close(&mut self, out: &mut Vec<Utterance>) {
        if let Some(o) = self.open.take() {
            if o.buf.len() >= self.p.min_len {
                self.stats.utterances += 1;
                out.push(Utterance { n: o.n, samples: o.buf });
            } else {
                self.stats.blips += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg() -> Segmenter {
        // 8 kHz: split at 300 ms, drop under 150 ms, cut at 2 s.
        Segmenter::new(SegmentParams::from_ms(8000, 300, 150, 2000))
    }

    fn tone(len: usize) -> Vec<f32> {
        (0..len).map(|i| 0.3 * ((i as f32) * 0.7).sin() + 0.01).collect()
    }

    fn feed(s: &mut Segmenter, stream: &[f32], batch: usize) -> Vec<Utterance> {
        let mut out = Vec::new();
        for (k, chunk) in stream.chunks(batch).enumerate() {
            s.audio((k * batch) as u64, chunk, &mut out);
        }
        out
    }

    #[test]
    fn edges_are_exact_and_short_gaps_stay_inside() {
        let mut x = vec![0.0; 1234];
        x.extend(tone(4000));
        x.extend(vec![0.0; 800]); // 100 ms gap: kept
        x.extend(tone(3000));
        x.extend(vec![0.0; 4000]); // 500 ms: ends it
        x.extend(tone(2000));
        x.extend(vec![0.0; 3000]);
        let mut s = seg();
        let out = feed(&mut s, &x, 1000);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].n, 1234);
        assert_eq!(out[0].samples.len(), 4000 + 800 + 3000);
        assert_eq!(out[1].n, 1234 + 7800 + 4000);
        assert_eq!(out[1].samples.len(), 2000);
    }

    #[test]
    fn blips_are_dropped_and_counted() {
        let mut x = vec![0.0; 100];
        x.extend(tone(400)); // 50 ms
        x.extend(vec![0.0; 4000]);
        let mut s = seg();
        assert!(feed(&mut s, &x, 1000).is_empty());
        assert_eq!(s.stats.blips, 1);
    }

    #[test]
    fn a_stuck_carrier_is_cut_and_continues() {
        let mut x = tone(40_000); // 5 s
        x.extend(vec![0.0; 4000]);
        let mut s = seg();
        let out = feed(&mut s, &x, 1000);
        let lens: Vec<usize> = out.iter().map(|u| u.samples.len()).collect();
        assert_eq!(lens, vec![16_000, 16_000, 8_000]);
        assert_eq!(out[1].n, 16_000);
        assert_eq!(s.stats.cut, 2);
    }

    #[test]
    fn lost_samples_count_as_silence() {
        let mut s = seg();
        let mut out = Vec::new();
        s.audio(0, &tone(2000), &mut out);
        s.lost(2000, 1000, &mut out); // 125 ms: inside the utterance
        s.audio(3000, &tone(2000), &mut out);
        s.lost(5000, 8000, &mut out); // 1 s: ends it
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].samples.len(), 5000);
        assert_eq!(out[0].samples[2500], 0.0);
    }

    #[test]
    fn flush_closes_and_an_index_going_back_closes_too() {
        let mut s = seg();
        let mut out = Vec::new();
        s.audio(0, &tone(2000), &mut out);
        assert!(s.is_open());
        s.audio(0, &tone(2000), &mut out); // a new timeline
        assert_eq!(out.len(), 1);
        s.flush(&mut out);
        assert_eq!(out.len(), 2);
        assert!(!s.is_open());
    }
}
