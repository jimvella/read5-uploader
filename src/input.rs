//! The rtl_airband UDP input: one thread per `udp_stream` port turns
//! datagrams into timed utterances and drops each as a WAV into the spool's
//! `incoming/`, where the pipeline claims it. The WAV on disk makes each
//! utterance durable before it is encoded.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crate::airband::{decode_floats, rate_for_len, streams, Call, Recorder, RecorderParams, RecorderStats};
use crate::spool::time::{floor_minute, iso_ms, minute_start_ms, now_ms};
use crate::spool::Spool;
use tracing::{error, info, warn};

use crate::config::{Airband as AirbandCfg, Config};

/// One port we listen on, and what it carries.
#[derive(Debug, Clone)]
pub struct StreamSpec {
    pub port: u16,
    pub khz: u32,
    pub label: String,
    /// Channel id from `[[channels]]`.
    pub channel: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamState {
    port: u16,
    khz: u32,
    channel: String,
    up: bool,
    rate: Option<u32>,
    kernel_timestamps: bool,
    last_datagram_at: Option<String>,
    #[serde(skip)]
    last_datagram_ms: Option<i64>,
    bad_datagrams: u64,
    calls_written: u64,
    write_errors: u64,
    in_call: bool,
    jitter_ms: Option<f64>,
    drift_ppm: Option<f64>,
    #[serde(flatten)]
    recorder: RecorderStats,
    /// Minute starts (ms) during which this stream was down at some point.
    #[serde(skip)]
    down_minutes: BTreeSet<i64>,
    /// When the thread last went round its loop.
    #[serde(skip)]
    heartbeat_ms: i64,
}

pub struct Airband {
    states: Vec<Arc<Mutex<StreamState>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    require_ntp: bool,
    ntp: Mutex<(i64, Option<bool>)>,
}

/// Resolve the ports to listen on: explicit streams, else rtl_airband.conf.
/// A stream on a frequency with no `[[channels]]` entry is skipped: Read5
/// would refuse its uploads, since the key only covers issued channels.
pub fn resolve(cfg: &Config, a: &AirbandCfg) -> Result<Vec<StreamSpec>> {
    let mut found: Vec<(u16, u32, String)> = Vec::new();
    if !a.streams.is_empty() {
        for s in &a.streams {
            found.push((s.port, s.khz, s.label.clone()));
        }
    } else {
        let path = a.config.as_ref().expect("validated");
        let v = crate::airband::libconfig::parse_file(path).with_context(|| format!("read {}", path.display()))?;
        let parsed = streams::find(&v);
        for s in &parsed.skipped {
            warn!(config = %path.display(), line = s.line, "skipping rtl_airband output: {}", s.reason);
        }
        let listen: IpAddr = a.listen.parse().expect("validated");
        for s in parsed.streams {
            if listen.is_loopback() && !s.is_loopback() {
                warn!(
                    line = s.line, port = s.port, dest = %s.dest_address,
                    "rtl_airband sends this stream to {}, but we listen on {}; set rtl_airband.listen to match", s.dest_address, a.listen
                );
            }
            found.push((s.port, s.khz(), s.label.clone()));
        }
    }
    let mut out: Vec<StreamSpec> = Vec::new();
    for (port, khz, label) in found {
        match cfg.channel_for_khz(khz) {
            Some(c) => out.push(StreamSpec { port, khz, label, channel: c.id.clone() }),
            None => warn!(
                port, khz,
                "skipping rtl_airband stream: {khz} kHz has no [[channels]] entry (add it with the channel id Read5 issued)"
            ),
        }
    }
    let mut ports = BTreeSet::new();
    for s in &out {
        if !ports.insert(s.port) {
            bail!("port {} is listed twice", s.port);
        }
    }
    if out.is_empty() {
        bail!(
            "no usable rtl_airband udp_stream outputs. Each channel needs, in rtl_airband.conf, an output              {{ type = \"udp_stream\"; dest_address = \"127.0.0.1\"; dest_port = <unique port>; continuous = true; }}              and a [[channels]] entry here for its frequency"
        );
    }
    Ok(out)
}

impl Airband {
    /// Bind every port (failing fast if one is taken) and start the threads.
    pub fn start(cfg: &Config) -> Result<Self> {
        let a = cfg.rtl_airband.clone();
        let incoming = Spool::new(&cfg.spool.dir).incoming_dir();
        std::fs::create_dir_all(&incoming).with_context(|| format!("create {}", incoming.display()))?;
        let specs = resolve(cfg, &a)?;
        let listen: IpAddr = a.listen.parse().expect("validated");
        let stop = Arc::new(AtomicBool::new(false));
        let mut states = Vec::new();
        let mut threads = Vec::new();
        for spec in specs {
            let (sock, kernel_ts) = bind(SocketAddr::new(listen, spec.port))?;
            info!(
                port = spec.port, khz = spec.khz, channel = %spec.channel, label = %spec.label,
                kernel_timestamps = kernel_ts, "listening for rtl_airband udp_stream"
            );
            let state = Arc::new(Mutex::new(StreamState {
                port: spec.port,
                khz: spec.khz,
                channel: spec.channel.clone(),
                kernel_timestamps: kernel_ts,
                heartbeat_ms: now_ms(),
                ..Default::default()
            }));
            states.push(state.clone());
            let worker = Worker {
                spec,
                cfg: a.clone(),
                incoming: incoming.clone(),
                sock,
                state,
                stop: stop.clone(),
            };
            threads.push(
                std::thread::Builder::new()
                    .name(format!("udp-{}", worker.spec.port))
                    .spawn(move || worker.run())
                    .context("spawn udp thread")?,
            );
        }
        Ok(Self { states, threads: Mutex::new(threads), stop, require_ntp: a.require_ntp, ntp: Mutex::new((0, None)) })
    }

    /// Stop the threads; each writes its open utterance first.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let threads: Vec<JoinHandle<()>> = self.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }

    /// Channels that were down at some point during this minute.
    pub fn down_channels(&self, minute_key: &str) -> Vec<String> {
        let Some(m) = minute_start_ms(minute_key) else { return Vec::new() };
        let mut out: Vec<String> = self
            .states
            .iter()
            .filter_map(|s| {
                let s = s.lock().unwrap();
                s.down_minutes.contains(&m).then(|| s.channel.clone())
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Forget down-minute records older than this (they have been sealed).
    pub fn prune_before(&self, ms: i64) {
        for s in &self.states {
            s.lock().unwrap().down_minutes.retain(|m| *m >= ms);
        }
    }

    /// Every receive thread went round its loop recently (for the watchdog).
    pub fn threads_alive(&self) -> bool {
        let now = now_ms();
        self.states.iter().all(|s| now - s.lock().unwrap().heartbeat_ms < 30_000)
    }

    /// Whether uploads should wait: the clock says it is not NTP-synced.
    pub fn hold_uploads(&self) -> bool {
        self.require_ntp && self.clock_synced() == Some(false)
    }

    /// The kernel's view, refreshed once a minute. `None` where it can't be
    /// asked (not Linux): then nothing is held.
    pub fn clock_synced(&self) -> Option<bool> {
        let mut g = self.ntp.lock().unwrap();
        let now = now_ms();
        if now - g.0 > 60_000 {
            let was = g.1;
            *g = (now, ntp_synchronized());
            if g.1 != was && g.1 == Some(false) {
                warn!("system clock is not NTP-synchronised; holding uploads (rtl_airband.require_ntp)");
            } else if g.1 != was && was == Some(false) {
                info!("system clock is NTP-synchronised again");
            }
        }
        g.1
    }

    pub fn status_json(&self) -> serde_json::Value {
        let streams: Vec<serde_json::Value> = self
            .states
            .iter()
            .map(|s| serde_json::to_value(&*s.lock().unwrap()).unwrap_or_default())
            .collect();
        serde_json::json!({
            "clockSynced": self.clock_synced(),
            "holdingUploads": self.hold_uploads(),
            "threadsAlive": self.threads_alive(),
            "streams": streams,
        })
    }

    pub fn any_down(&self) -> bool {
        self.states.iter().any(|s| !s.lock().unwrap().up)
    }
}

impl Drop for Airband {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "linux")]
fn ntp_synchronized() -> Option<bool> {
    // What `timedatectl show -p NTPSynchronized` reports (systemd's
    // `ntp_synced`): the kernel's maximum clock error is under its 16 s
    // ceiling, where only a running NTP client keeps it. Asked of the kernel
    // directly, so it also works in a container and without systemd. Reading
    // (`modes` = 0) needs no privilege.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    if unsafe { libc::adjtimex(&mut tx) } < 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // c_long is 32-bit on armv7
    Some((tx.maxerror as i64) < 16_000_000)
}

#[cfg(not(target_os = "linux"))]
fn ntp_synchronized() -> Option<bool> {
    None
}

/// A UDP socket with a large receive buffer and a short read timeout, plus
/// kernel receive timestamps where the platform has them.
fn bind(addr: SocketAddr) -> Result<(UdpSocket, bool)> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // Eight seconds of 8 kHz float audio; the kernel may cap it lower.
    if let Err(e) = sock.set_recv_buffer_size(1 << 20) {
        warn!(error = %e, "could not raise the UDP receive buffer");
    }
    sock.bind(&addr.into()).with_context(|| format!("bind udp {addr} (is another uploader already running?)"))?;
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    let sock: UdpSocket = sock.into();
    let kernel = recv::enable_timestamps(&sock);
    Ok((sock, kernel))
}

struct Worker {
    spec: StreamSpec,
    cfg: AirbandCfg,
    incoming: PathBuf,
    sock: UdpSocket,
    state: Arc<Mutex<StreamState>>,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn run(self) {
        let mut buf = vec![0u8; 65536];
        let mut rec: Option<Recorder> = None;
        let down_after_ms = (self.cfg.down_after_secs * 1000) as i64;
        let mut was_up = false;
        let mut logged = RecorderStats::default();
        loop {
            let stopping = self.stop.load(Ordering::SeqCst);
            let got = if stopping { None } else { recv::recv(&self.sock, &mut buf) };
            let now = now_ms();
            let mut calls: Vec<Call> = Vec::new();
            match got {
                Some((len, recv_us)) => {
                    let rate = rate_for_len(len);
                    let mut st = self.state.lock().unwrap();
                    let mismatch = matches!((&rec, rate), (Some(cur), Some(r)) if cur.rate() != r);
                    match rate {
                        None => {
                            st.bad_datagrams += 1;
                            if st.bad_datagrams == 1 {
                                warn!(port = self.spec.port, bytes = len, "datagram is not one mono rtl_airband batch (4000 or 8000 bytes); ignoring such datagrams");
                            }
                        }
                        Some(_) if mismatch => st.bad_datagrams += 1,
                        Some(r) => {
                            if rec.is_none() {
                                rec = Some(Recorder::new(RecorderParams {
                                    rate: r,
                                    latency_us: (self.cfg.latency_ms * 1000.0).round() as i64,
                                    split_gap_ms: self.cfg.split_gap_ms,
                                    min_ms: self.cfg.min_ms,
                                    max_ms: self.cfg.max_ms,
                                }));
                                st.rate = Some(r);
                                info!(port = self.spec.port, rate = r, "first datagram");
                            }
                            st.last_datagram_ms = Some(now);
                            drop(st);
                            if let Some(r) = rec.as_mut() {
                                calls = r.push(recv_us, decode_floats(&buf[..len]));
                            }
                        }
                    }
                }
                None => {
                    if let Some(r) = rec.as_mut() {
                        calls = if stopping { r.finish() } else { r.tick(now * 1000) };
                    }
                }
            }
            for c in &calls {
                self.write_call(c);
            }

            let mut st = self.state.lock().unwrap();
            st.heartbeat_ms = now;
            st.up = st.last_datagram_ms.map(|t| now - t < down_after_ms).unwrap_or(false);
            if !st.up {
                st.down_minutes.insert(floor_minute(now));
            }
            if st.up != was_up {
                if st.up {
                    info!(port = self.spec.port, channel = %self.spec.channel, "stream up");
                } else {
                    warn!(port = self.spec.port, channel = %self.spec.channel, "stream down: no datagrams for {} s (is rtl_airband running?)", self.cfg.down_after_secs);
                }
                was_up = st.up;
            }
            if let Some(r) = &rec {
                let s = r.stats();
                if s.clock.lost_batches > logged.clock.lost_batches {
                    warn!(port = self.spec.port, batches = s.clock.lost_batches - logged.clock.lost_batches, "batches missing from the stream; filled with silence");
                }
                if s.clock.restarts > logged.clock.restarts {
                    info!(port = self.spec.port, "stream restarted; new timeline");
                }
                if s.clock.reanchors > logged.clock.reanchors {
                    warn!(port = self.spec.port, "arrival times jumped (clock step?); re-anchored");
                }
                logged = s.clone();
                st.recorder = s;
                st.jitter_ms = r.jitter_us().map(|j| (j as f64 / 100.0).round() / 10.0);
                st.in_call = r.in_call();
                st.drift_ppm = r.drift_ppm().map(|d| d.round());
            }
            st.last_datagram_at = st.last_datagram_ms.map(iso_ms);
            drop(st);
            if stopping {
                break;
            }
        }
    }

    fn write_call(&self, c: &Call) {
        let start = c.start_ms();
        let stem = format!("rtl_airband_{}_{}", self.spec.khz, start);
        let icmt = format!(
            "Frequency:{};Name:{};Start:{};End:{};Timing:rtl_airband-udp;Port:{};",
            self.spec.khz as u64 * 1000,
            self.spec.label.replace(';', ","),
            start,
            c.end_ms(),
            self.spec.port
        );
        let mut info: Vec<(&str, &str)> = vec![("ICMT", &icmt), ("ISFT", "read5-uploader rtl_airband udp")];
        if !self.spec.label.is_empty() {
            info.push(("INAM", &self.spec.label));
        }
        let bytes = crate::audio::wav::write_pcm16_info(&c.pcm16(), c.rate, &info);
        match write_atomic(&self.incoming, &stem, &bytes) {
            Ok(p) => {
                self.state.lock().unwrap().calls_written += 1;
                info!(port = self.spec.port, channel = %self.spec.channel, start = %iso_ms(start), dur_ms = c.dur_ms(), file = %p.display(), "utterance");
            }
            Err(e) => {
                self.state.lock().unwrap().write_errors += 1;
                error!(port = self.spec.port, error = %format!("{e:#}"), "could not write utterance WAV");
            }
        }
    }
}

/// `{stem}.wav.tmp`, then rename to `.wav`: the scanner only takes `.wav`.
fn write_atomic(dir: &Path, stem: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut name = format!("{stem}.wav");
    let mut n = 1;
    while dir.join(&name).exists() {
        name = format!("{stem}-{n}.wav");
        n += 1;
    }
    let fin = dir.join(&name);
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &fin).with_context(|| format!("rename {}", tmp.display()))?;
    Ok(fin)
}

/// Receiving with the kernel's arrival timestamp (Linux), or the time
/// `recv` returned (elsewhere).
mod recv {
    use std::net::UdpSocket;

    fn now_us() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0)
    }

    #[cfg(target_os = "linux")]
    pub fn enable_timestamps(sock: &UdpSocket) -> bool {
        use nix::sys::socket::{setsockopt, sockopt};
        setsockopt(sock, sockopt::ReceiveTimestampns, &true).is_ok()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn enable_timestamps(_sock: &UdpSocket) -> bool {
        false
    }

    /// `(bytes, arrival µs)`, or `None` on timeout or error.
    #[cfg(target_os = "linux")]
    pub fn recv(sock: &UdpSocket, buf: &mut [u8]) -> Option<(usize, i64)> {
        use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags, SockaddrStorage};
        use std::io::IoSliceMut;
        use std::os::fd::AsRawFd;
        let mut iov = [IoSliceMut::new(buf)];
        let mut space = nix::cmsg_space!(nix::sys::time::TimeSpec);
        let msg = recvmsg::<SockaddrStorage>(sock.as_raw_fd(), &mut iov, Some(&mut space), MsgFlags::empty()).ok()?;
        let mut t = None;
        if let Ok(cmsgs) = msg.cmsgs() {
            for c in cmsgs {
                if let ControlMessageOwned::ScmTimestampns(ts) = c {
                    t = Some(ts.tv_sec() as i64 * 1_000_000 + ts.tv_nsec() as i64 / 1000);
                }
            }
        }
        Some((msg.bytes, t.unwrap_or_else(now_us)))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn recv(sock: &UdpSocket, buf: &mut [u8]) -> Option<(usize, i64)> {
        match sock.recv(buf) {
            Ok(n) => Some((n, now_us())),
            Err(_) => None,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn the_kernel_answers_the_clock_question() {
        let synced = super::ntp_synchronized();
        println!("ntp_synchronized: {synced:?}");
        assert!(synced.is_some());
    }
}
