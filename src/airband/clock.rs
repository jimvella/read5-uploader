//! Wall-clock time for every sample of a headerless UDP stream.
//!
//! rtl_airband sends one datagram per batch as soon as the batch is
//! demodulated, so a datagram's arrival can be late but never early. The
//! offset `arrival - samples_so_far / rate` is therefore smallest when
//! nothing delayed it. The anchor is a line through the smallest offset of
//! each second in the window: its level is the undelayed arrival, its slope
//! the sample clock's rate error (a dongle's crystal, or the file input's
//! pacing), which a plain minimum would trail.
//!
//! Datagrams are held for `hold` before release so that a lasting jump in the
//! offset (batches that were never sent) can be filled with silence at the
//! point it happened, before the audio reaches the segmenter.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct ClockParams {
    pub rate: u32,
    /// Window for the lower envelope.
    pub window_us: i64,
    /// How long a datagram is held before release.
    pub hold_us: i64,
    /// No datagram for this long: rtl_airband stopped; start a new timeline.
    pub restart_gap_us: i64,
    /// A jump larger than this is not lost batches (a clock step or a long
    /// stall): re-anchor instead of filling.
    pub max_fill_us: i64,
}

impl ClockParams {
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            window_us: 30_000_000,
            hold_us: 1_000_000,
            restart_gap_us: 2_000_000,
            max_fill_us: 10_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Samples starting at timeline index `n`.
    Audio { n: u64, samples: Vec<f32> },
    /// `samples` were never received, starting at `n`; treat them as silence.
    Lost { n: u64, samples: u64 },
    /// The timeline was broken (stream stopped, restarted, or the clock
    /// jumped); close anything open. Indices start again from 0 after a
    /// stop; after a jump they continue.
    Break,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockStats {
    pub datagrams: u64,
    pub lost_batches: u64,
    pub restarts: u64,
    pub reanchors: u64,
}

struct Pending {
    recv_us: i64,
    n: u64,
    samples: Vec<f32>,
}

pub struct Clock {
    p: ClockParams,
    n_next: u64,
    pending: VecDeque<Pending>,
    /// (arrival, offset) for every datagram in the window, oldest first. The
    /// last `pending.len()` entries belong to the pending datagrams.
    window: VecDeque<(i64, i64)>,
    last_recv: Option<i64>,
    stalled: bool,
    pub stats: ClockStats,
}

impl Clock {
    pub fn new(p: ClockParams) -> Self {
        Self {
            p,
            n_next: 0,
            pending: VecDeque::new(),
            window: VecDeque::new(),
            last_recv: None,
            stalled: false,
            stats: ClockStats::default(),
        }
    }

    fn samples_us(&self, n: u64) -> i64 {
        (n as i128 * 1_000_000 / self.p.rate as i128) as i64
    }

    /// Wall time (µs since the epoch) at which sample `n` of the current
    /// timeline was received, before latency correction.
    pub fn time_us(&self, n: u64) -> Option<i64> {
        let f = self.fit()?;
        let t = self.samples_us(n);
        // Arrival = stream time + offset(arrival); two rounds converge.
        let mut r = t + f.at(t + f.off0);
        r = t + f.at(r);
        Some(r)
    }

    /// The offset line through the decided datagrams (the held ones may be
    /// late because batches went missing, and must not bend it), or through
    /// everything while nothing is decided yet.
    fn fit(&self) -> Option<Fit> {
        let decided = self.window.len() - self.pending.len();
        let pts: Vec<(i64, i64)> = if decided > 0 {
            self.window.iter().take(decided).copied().collect()
        } else {
            self.window.iter().copied().collect()
        };
        fit_lower(&pts)
    }

    /// Spread of arrival delays over the window (95th percentile above the
    /// anchor line), in µs. Large values mean the timing is getting noisy.
    pub fn jitter_us(&self) -> Option<i64> {
        let f = self.fit()?;
        let mut d: Vec<i64> = self.window.iter().map(|(t, o)| o - f.at(*t)).collect();
        d.sort_unstable();
        Some(d[(d.len() * 95 / 100).min(d.len() - 1)].max(0))
    }

    /// The anchor line's slope in parts per million (sample clock rate error).
    pub fn drift_ppm(&self) -> Option<f64> {
        self.fit().map(|f| f.slope * 1e6)
    }

    pub fn push(&mut self, recv_us: i64, samples: Vec<f32>, out: &mut Vec<Event>) {
        if let Some(last) = self.last_recv {
            if recv_us - last > self.p.restart_gap_us {
                self.new_timeline(out);
            }
        }
        self.stalled = false;
        self.last_recv = Some(recv_us);
        self.stats.datagrams += 1;
        let n = self.n_next;
        self.n_next += samples.len() as u64;
        let offset = recv_us - self.samples_us(self.n_next);
        self.window.push_back((recv_us, offset));
        while let Some((t, _)) = self.window.front() {
            if recv_us - t > self.p.window_us && self.window.len() > self.pending.len() + 1 {
                self.window.pop_front();
            } else {
                break;
            }
        }
        self.pending.push_back(Pending { recv_us, n, samples });
        self.release(recv_us, out);
    }

    /// Call when no datagram arrived for a while: releases held datagrams and
    /// reports a stopped stream once.
    pub fn tick(&mut self, now_us: i64, out: &mut Vec<Event>) {
        self.release(now_us, out);
        if let Some(last) = self.last_recv {
            if !self.stalled && now_us - last > self.p.restart_gap_us {
                self.drain(out);
                out.push(Event::Break);
                self.stalled = true;
            }
        }
    }

    /// Release everything held (shutdown).
    pub fn drain(&mut self, out: &mut Vec<Event>) {
        while !self.pending.is_empty() {
            self.decide_front(out);
        }
    }

    fn new_timeline(&mut self, out: &mut Vec<Event>) {
        self.drain(out);
        if !self.stalled {
            out.push(Event::Break);
        }
        self.stats.restarts += 1;
        self.n_next = 0;
        self.window.clear();
    }

    fn release(&mut self, now_us: i64, out: &mut Vec<Event>) {
        while let Some(f) = self.pending.front() {
            if now_us - f.recv_us >= self.p.hold_us {
                self.decide_front(out);
            } else {
                break;
            }
        }
    }

    fn decide_front(&mut self, out: &mut Vec<Event>) {
        let fit = self.fit();
        let base = self.window.len() - self.pending.len();
        let batch = self.pending.front().map(|f| f.samples.len() as u64).unwrap_or(0);
        let batch_us = self.samples_us(batch).max(1);
        // Smallest excess among the held datagrams: if even the best of them
        // is late by half a batch or more, samples went missing before the
        // front one.
        let min_excess = match fit {
            Some(f) => self.window.iter().skip(base).map(|(t, o)| o - f.at(*t)).min().unwrap_or(0),
            None => 0,
        };
        if min_excess >= batch_us / 2 && batch > 0 {
            let k = ((min_excess as f64) / batch_us as f64).round() as u64;
            if self.samples_us(k * batch) <= self.p.max_fill_us {
                let shift = k * batch;
                let shift_us = self.samples_us(shift);
                let n_lost = self.pending.front().map(|f| f.n).unwrap_or(0);
                for p in self.pending.iter_mut() {
                    p.n += shift;
                }
                for w in self.window.iter_mut().skip(base) {
                    w.1 -= shift_us;
                }
                self.n_next += shift;
                self.stats.lost_batches += k;
                out.push(Event::Lost { n: n_lost, samples: shift });
            } else {
                // A clock step or a stall far beyond loss: forget the old
                // anchor and carry on from the held datagrams.
                for _ in 0..base {
                    self.window.pop_front();
                }
                self.stats.reanchors += 1;
                out.push(Event::Break);
            }
        }
        if let Some(f) = self.pending.pop_front() {
            out.push(Event::Audio { n: f.n, samples: f.samples });
        }
    }
}

/// `offset = off0 + slope * (t - t_ref)`.
#[derive(Debug, Clone, Copy)]
struct Fit {
    t_ref: i64,
    off0: i64,
    slope: f64,
}

impl Fit {
    fn at(&self, t: i64) -> i64 {
        self.off0 + (self.slope * (t - self.t_ref) as f64).round() as i64
    }
}

/// Slope by least squares through the smallest offset of each second, level
/// through the lowest of them. Fewer than
/// three seconds of data, or an implausible slope (over 1%), gives a flat
/// line at the minimum.
fn fit_lower(pts: &[(i64, i64)]) -> Option<Fit> {
    let (&(t0, _), &(t1, _)) = (pts.first()?, pts.last()?);
    let flat = || {
        let (t, o) = *pts.iter().min_by_key(|(_, o)| *o).unwrap();
        Fit { t_ref: t, off0: o, slope: 0.0 }
    };
    if t1 - t0 < 3_000_000 {
        return Some(flat());
    }
    let mut mins: Vec<(i64, i64)> = Vec::new();
    for &(t, o) in pts {
        let b = (t - t0) / 1_000_000;
        match mins.last_mut() {
            Some(m) if (m.0 - t0) / 1_000_000 == b => {
                if o < m.1 {
                    *m = (t, o);
                }
            }
            _ => mins.push((t, o)),
        }
    }
    if mins.len() < 3 {
        return Some(flat());
    }
    let n = mins.len() as f64;
    let t_ref = mins.iter().map(|m| m.0 as f64).sum::<f64>() / n;
    let o_mean = mins.iter().map(|m| m.1 as f64).sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (t, o) in &mins {
        let dt = *t as f64 - t_ref;
        sxy += dt * (*o as f64 - o_mean);
        sxx += dt * dt;
    }
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    if slope.abs() > 0.01 {
        return Some(flat());
    }
    // Least squares gives the slope; the level is the lowest point, since no
    // arrival is ever early and each second's minimum is still a little late.
    let low = mins
        .iter()
        .map(|(t, o)| *o as f64 - (o_mean + slope * (*t as f64 - t_ref)))
        .fold(f64::INFINITY, f64::min);
    Some(Fit { t_ref: t_ref.round() as i64, off0: (o_mean + low).round() as i64, slope })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 8000;
    const BATCH: usize = 1000;
    const BATCH_US: i64 = 125_000;
    const T0: i64 = 1_800_000_000_000_000;

    /// Deterministic pseudo-random delay in [0, max) µs.
    fn delay(i: u64, max: i64) -> i64 {
        // splitmix64
        let mut z = i.wrapping_add(0x9E3779B97F4A7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        (z % max as u64) as i64
    }

    fn run(arrivals: &[(i64, usize)]) -> (Clock, Vec<Event>) {
        let mut c = Clock::new(ClockParams::new(RATE));
        let mut out = Vec::new();
        for (t, len) in arrivals {
            c.push(*t, vec![0.5; *len], &mut out);
        }
        (c, out)
    }

    /// Batch k's last sample is received at T0 + (k+1) * 125 ms; it arrives
    /// `late` after that.
    fn arrival(k: i64, late: i64) -> i64 {
        T0 + (k + 1) * BATCH_US + late
    }

    #[test]
    fn anchor_finds_the_undelayed_arrival_despite_jitter() {
        let a: Vec<(i64, usize)> = (0..400).map(|k| (arrival(k, 3_000 + delay(k as u64, 60_000)), BATCH)).collect();
        let (c, out) = run(&a);
        // Sample 0 was received at T0, plus the smallest delay seen (3 ms + ~0).
        let t = c.time_us(0).unwrap();
        assert!((t - T0 - 3_000).abs() < 1_500, "{}", t - T0);
        assert!(c.jitter_us().unwrap() > 40_000, "{:?}", c.jitter_us());
        assert!(c.drift_ppm().unwrap().abs() < 100.0, "{:?}", c.drift_ppm());
        assert!(out.iter().all(|e| matches!(e, Event::Audio { .. })));
        assert_eq!(c.stats.lost_batches, 0);
    }

    #[test]
    fn rate_drift_is_tracked_not_taken_for_loss() {
        // The source runs 2000 ppm slow (rtl_airband's file input at
        // speedup 1 did about this): each batch arrives 250 µs later than
        // the last would predict. Over a minute that is 120 ms.
        let a: Vec<(i64, usize)> = (0..480)
            .map(|k| (T0 + ((k + 1) * BATCH_US) * 1002 / 1000 + 2_000 + delay(k as u64, 30_000), BATCH))
            .collect();
        let (mut c, mut out) = run(&a);
        c.drain(&mut out);
        assert_eq!(c.stats.lost_batches, 0);
        assert!((c.drift_ppm().unwrap() - 2000.0).abs() < 300.0, "{:?}", c.drift_ppm());
        // Sample 400,000 (batch 400) was received at T0 + 50.1 s.
        let t = c.time_us(400 * BATCH as u64).unwrap();
        let truth = T0 + 400 * BATCH_US * 1002 / 1000;
        assert!((t - truth - 2_000).abs() < 6_000, "{} µs", t - truth);
    }

    #[test]
    fn lost_batches_are_filled_where_they_were_lost() {
        // Batches 0..99, then 100 and 101 never sent, then 102.. arrive.
        let mut a = Vec::new();
        for k in (0..100).chain(102..200) {
            a.push((arrival(k, 2_000 + delay(k as u64, 20_000)), BATCH));
        }
        let (c, out) = run(&a);
        let lost: Vec<&Event> = out.iter().filter(|e| matches!(e, Event::Lost { .. })).collect();
        assert_eq!(lost, vec![&Event::Lost { n: 100 * BATCH as u64, samples: 2 * BATCH as u64 }]);
        assert_eq!(c.stats.lost_batches, 2);
        // The batch after the gap is placed at its true index.
        assert!(out.contains(&Event::Audio { n: 102 * BATCH as u64, samples: vec![0.5; BATCH] }));
        let t = c.time_us(102 * BATCH as u64).unwrap();
        assert!((t - (T0 + 102 * BATCH_US) - 2_000).abs() < 2_000, "{}", t - T0 - 102 * BATCH_US);
    }

    #[test]
    fn a_late_burst_is_not_loss() {
        // A 700 ms stall, then the backlog arrives together and the stream
        // is back on time.
        let mut a = Vec::new();
        for k in 0..100 {
            let mut t = arrival(k, 2_000);
            if (50..56).contains(&k) {
                t = arrival(56, 2_000);
            }
            a.push((t, BATCH));
        }
        let (c, out) = run(&a);
        assert_eq!(c.stats.lost_batches, 0);
        assert!(!out.iter().any(|e| matches!(e, Event::Lost { .. })));
    }

    #[test]
    fn silence_breaks_the_timeline_once() {
        let mut c = Clock::new(ClockParams::new(RATE));
        let mut out = Vec::new();
        for k in 0..20 {
            c.push(arrival(k, 1_000), vec![0.5; BATCH], &mut out);
        }
        c.tick(arrival(19, 1_000) + 3_000_000, &mut out);
        c.tick(arrival(19, 1_000) + 4_000_000, &mut out);
        assert_eq!(out.iter().filter(|e| **e == Event::Break).count(), 1);
        assert_eq!(out.iter().filter(|e| matches!(e, Event::Audio { .. })).count(), 20);
        // rtl_airband comes back: indices restart at 0, no second Break.
        let t1 = arrival(19, 0) + 10_000_000;
        out.clear();
        for k in 0..20 {
            c.push(t1 + k * BATCH_US, vec![0.5; BATCH], &mut out);
        }
        c.drain(&mut out);
        assert!(!out.contains(&Event::Break));
        assert_eq!(out.first(), Some(&Event::Audio { n: 0, samples: vec![0.5; BATCH] }));
        assert_eq!(c.stats.restarts, 1);
        assert!((c.time_us(0).unwrap() - (t1 - BATCH_US)).abs() < 1_000);
    }

    #[test]
    fn a_clock_step_forward_reanchors_instead_of_filling() {
        let mut a = Vec::new();
        for k in 0..100 {
            let step = if k >= 50 { 15_000_000 } else { 0 };
            a.push((arrival(k, 1_000) + step, BATCH));
        }
        // A 15 s jump would also read as a stopped stream; widen the restart
        // gap so this exercises the step path.
        let mut c = Clock::new(ClockParams { restart_gap_us: 60_000_000, ..ClockParams::new(RATE) });
        let mut out = Vec::new();
        for (t, len) in &a {
            c.push(*t, vec![0.5; *len], &mut out);
        }
        c.drain(&mut out);
        assert_eq!(c.stats.lost_batches, 0);
        assert_eq!(c.stats.reanchors, 1);
        // After the step, times follow the stepped clock.
        let t = c.time_us(60 * BATCH as u64).unwrap();
        assert!((t - (T0 + 60 * BATCH_US + 15_000_000)).abs() < 3_000, "{}", t - T0);
    }

    #[test]
    fn a_clock_step_back_is_followed_at_once() {
        let mut a = Vec::new();
        for k in 0..100 {
            let step = if k >= 50 { -400_000 } else { 0 };
            a.push((arrival(k, 1_000) + step, BATCH));
        }
        let (mut c, mut out) = run(&a);
        c.drain(&mut out);
        assert_eq!(c.stats.lost_batches, 0);
        let t = c.time_us(80 * BATCH as u64).unwrap();
        assert!((t - (T0 + 80 * BATCH_US - 400_000)).abs() < 3_000);
    }
}
