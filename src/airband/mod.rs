//! rtl_airband's `udp_stream` output turned into timed utterances. No I/O
//! here: [`crate::input`] owns the sockets and the files, this module owns
//! the meaning of the bytes.
//!
//! - [`libconfig`] and [`streams`]: which UDP outputs `rtl_airband.conf` sets up.
//! - [`clock`]: wall-clock time for each sample from datagram arrival times.
//! - [`segment`]: utterances from squelch-closed (exactly zero) samples.
//! - [`Recorder`]: the two together for one stream.

pub mod clock;
pub mod libconfig;
pub mod segment;
pub mod streams;

use clock::{Clock, ClockParams, ClockStats, Event};
use segment::{SegmentParams, SegmentStats, Segmenter};

/// The sample rate a mono datagram of this size implies: 8 kHz builds send
/// 4,000 bytes, NFM builds (16 kHz) 8,000.
pub fn rate_for_len(bytes: usize) -> Option<u32> {
    match bytes {
        4000 => Some(8000),
        8000 => Some(16000),
        _ => None,
    }
}

/// Native-endian 32-bit floats, as rtl_airband `send()`s them. Every Pi and
/// x86 machine is little-endian.
pub fn decode_floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct RecorderParams {
    pub rate: u32,
    /// Fixed delay between a sample's reception and its batch being sent,
    /// beyond the batch itself, µs.
    pub latency_us: i64,
    pub split_gap_ms: u32,
    pub min_ms: u32,
    pub max_ms: u32,
}

/// One utterance, timed.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// Wall time of the first sample, µs since the Unix epoch.
    pub start_us: i64,
    pub rate: u32,
    pub samples: Vec<f32>,
}

impl Call {
    pub fn start_ms(&self) -> i64 {
        self.start_us.div_euclid(1000)
    }
    pub fn dur_ms(&self) -> i64 {
        (self.samples.len() as i64 * 1000) / self.rate as i64
    }
    pub fn end_ms(&self) -> i64 {
        self.start_ms() + self.dur_ms()
    }
    /// 16-bit PCM for the WAV hand-off.
    pub fn pcm16(&self) -> Vec<i16> {
        self.samples.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16).collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStats {
    #[serde(flatten)]
    pub clock: ClockStats,
    #[serde(flatten)]
    pub segment: SegmentStats,
    /// Utterances finished before the clock had an anchor (never expected).
    pub untimed: u64,
}

pub struct Recorder {
    p: RecorderParams,
    clock: Clock,
    seg: Segmenter,
    events: Vec<Event>,
    untimed: u64,
}

impl Recorder {
    pub fn new(p: RecorderParams) -> Self {
        Self {
            p,
            clock: Clock::new(ClockParams::new(p.rate)),
            seg: Segmenter::new(SegmentParams::from_ms(p.rate, p.split_gap_ms, p.min_ms, p.max_ms)),
            events: Vec::new(),
            untimed: 0,
        }
    }

    pub fn rate(&self) -> u32 {
        self.p.rate
    }

    /// One datagram's samples, received at `recv_us`.
    pub fn push(&mut self, recv_us: i64, samples: Vec<f32>) -> Vec<Call> {
        let mut ev = std::mem::take(&mut self.events);
        self.clock.push(recv_us, samples, &mut ev);
        self.apply(ev)
    }

    /// Nothing arrived; let held audio through and notice a stopped stream.
    pub fn tick(&mut self, now_us: i64) -> Vec<Call> {
        let mut ev = std::mem::take(&mut self.events);
        self.clock.tick(now_us, &mut ev);
        self.apply(ev)
    }

    /// Shutdown: everything held, and the open utterance.
    pub fn finish(&mut self) -> Vec<Call> {
        let mut ev = std::mem::take(&mut self.events);
        self.clock.drain(&mut ev);
        ev.push(Event::Break);
        self.apply(ev)
    }

    pub fn jitter_us(&self) -> Option<i64> {
        self.clock.jitter_us()
    }

    /// Sample clock rate error the anchor line has found, ppm.
    pub fn drift_ppm(&self) -> Option<f64> {
        self.clock.drift_ppm()
    }

    pub fn in_call(&self) -> bool {
        self.seg.is_open()
    }

    pub fn stats(&self) -> RecorderStats {
        RecorderStats { clock: self.clock.stats.clone(), segment: self.seg.stats.clone(), untimed: self.untimed }
    }

    fn apply(&mut self, mut ev: Vec<Event>) -> Vec<Call> {
        let mut utts = Vec::new();
        let mut calls = Vec::new();
        for e in ev.drain(..) {
            match e {
                Event::Audio { n, samples } => self.seg.audio(n, &samples, &mut utts),
                Event::Lost { n, samples } => self.seg.lost(n, samples, &mut utts),
                Event::Break => self.seg.flush(&mut utts),
            }
            // Time each utterance as it closes, against the anchor of the
            // timeline it belongs to (a Break may reset it next).
            for u in utts.drain(..) {
                match self.clock.time_us(u.n) {
                    Some(t) => calls.push(Call { start_us: t - self.p.latency_us, rate: self.p.rate, samples: u.samples }),
                    None => self.untimed += 1,
                }
            }
        }
        self.events = ev;
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_800_000_000_000_000;

    #[test]
    fn datagram_sizes_and_floats() {
        assert_eq!(rate_for_len(4000), Some(8000));
        assert_eq!(rate_for_len(8000), Some(16000));
        assert_eq!(rate_for_len(4001), None);
        let b: Vec<u8> = [0.5f32, -1.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        assert_eq!(decode_floats(&b), vec![0.5, -1.0]);
    }

    /// A transmission from 2.000 s to 4.500 s of stream time, batches
    /// arriving with jitter: the call is timed to the sample.
    #[test]
    fn a_call_is_timed_from_its_first_sample() {
        let mut r = Recorder::new(RecorderParams { rate: 8000, latency_us: 12_500, split_gap_ms: 300, min_ms: 150, max_ms: 120_000 });
        let mut calls = Vec::new();
        for k in 0..80u64 {
            let samples: Vec<f32> = (0..1000u64)
                .map(|i| {
                    let n = k * 1000 + i;
                    if (16_000..36_000).contains(&n) { 0.25 } else { 0.0 }
                })
                .collect();
            // Batch k ends at T0 + (k+1)/8 s; 12.5 ms look-ahead plus jitter.
            let jitter = ((k * 7919) % 40) as i64 * 1000;
            let recv = T0 + (k as i64 + 1) * 125_000 + 12_500 + jitter;
            calls.extend(r.push(recv, samples));
        }
        calls.extend(r.finish());
        assert_eq!(calls.len(), 1);
        let c = &calls[0];
        assert_eq!(c.samples.len(), 20_000);
        assert_eq!(c.dur_ms(), 2500);
        let err = c.start_us - (T0 + 2_000_000);
        assert!(err.abs() < 1_000, "start off by {err} µs");
        assert_eq!(r.stats().segment.utterances, 1);
    }
}
