//! Which `udp_stream` outputs in an `rtl_airband.conf` we can record, and why
//! the others are skipped.

use std::collections::BTreeMap;

use super::libconfig::{Kind, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub port: u16,
    pub dest_address: String,
    pub freq_hz: u64,
    pub label: String,
    pub modulation: String,
    /// Index of the device and channel in the config, for messages.
    pub device: usize,
    pub channel: usize,
    /// Line of the output block.
    pub line: u32,
}

impl Stream {
    pub fn khz(&self) -> u32 {
        ((self.freq_hz + 500) / 1000) as u32
    }

    /// Whether rtl_airband sends to this machine's loopback address.
    pub fn is_loopback(&self) -> bool {
        let a = self.dest_address.trim();
        a == "localhost" || a == "::1" || a.starts_with("127.")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub line: u32,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Found {
    pub streams: Vec<Stream>,
    pub skipped: Vec<Skipped>,
}

/// rtl_airband's `parse_anynum2int`: an int is hertz, a float megahertz, a
/// string a number with an optional k/M/G suffix.
pub fn freq_hz(v: &Value) -> Option<u64> {
    match &v.kind {
        Kind::Int(i) if *i > 0 => Some(*i as u64),
        Kind::Float(f) if *f > 0.0 => Some((f * 1e6).round() as u64),
        Kind::Str(s) => {
            let s = s.trim();
            let (num, mult) = match s.chars().last()? {
                'k' | 'K' => (&s[..s.len() - 1], 1e3),
                'm' | 'M' => (&s[..s.len() - 1], 1e6),
                'g' | 'G' => (&s[..s.len() - 1], 1e9),
                _ => (s, 1.0),
            };
            let f: f64 = num.trim().parse().ok()?;
            (f > 0.0).then(|| (f * mult).round() as u64)
        }
        _ => None,
    }
}

fn disabled(v: &Value) -> bool {
    v.get("disable").and_then(|d| d.as_bool()).unwrap_or(false)
}

fn is_udp(out: &Value) -> bool {
    // rtl_airband compares the first six characters only.
    out.get("type").and_then(|t| t.as_str()).map(|t| t.starts_with("udp_st")).unwrap_or(false)
}

fn port(v: &Value) -> Option<u16> {
    match &v.kind {
        Kind::Int(i) => u16::try_from(*i).ok().filter(|p| *p > 0),
        Kind::Str(s) => s.trim().parse::<u16>().ok().filter(|p| *p > 0),
        _ => None,
    }
}

pub fn find(cfg: &Value) -> Found {
    let mut f = Found::default();
    let skip = |f: &mut Found, line: u32, reason: String| f.skipped.push(Skipped { line, reason });

    for (di, dev) in cfg.get("devices").map(|d| d.items()).unwrap_or(&[]).iter().enumerate() {
        if disabled(dev) {
            continue;
        }
        let scan = dev.get("mode").and_then(|m| m.as_str()).map(|m| m.starts_with("scan")).unwrap_or(false);
        for (ci, ch) in dev.get("channels").map(|c| c.items()).unwrap_or(&[]).iter().enumerate() {
            if disabled(ch) {
                continue;
            }
            for out in ch.get("outputs").map(|o| o.items()).unwrap_or(&[]) {
                if !is_udp(out) || disabled(out) {
                    continue;
                }
                let line = out.line;
                if scan {
                    skip(&mut f, line, format!("devices[{di}] is in scan mode; its UDP datagrams don't say which frequency they carry"));
                    continue;
                }
                let Some(freq) = ch.get("freq").and_then(freq_hz) else {
                    skip(&mut f, line, format!("devices[{di}].channels[{ci}] has no usable freq"));
                    continue;
                };
                let Some(p) = out.get("dest_port").and_then(port) else {
                    skip(&mut f, line, "udp_stream without a valid dest_port".into());
                    continue;
                };
                if !out.get("continuous").and_then(|c| c.as_bool()).unwrap_or(false) {
                    skip(&mut f, line, format!("udp_stream to port {p} needs `continuous = true` (the steady stream is the uploader's clock)"));
                    continue;
                }
                f.streams.push(Stream {
                    port: p,
                    dest_address: out.get("dest_address").and_then(|a| a.as_str()).unwrap_or("").to_string(),
                    freq_hz: freq,
                    label: ch.get("label").and_then(|l| l.as_str()).unwrap_or("").to_string(),
                    modulation: ch.get("modulation").and_then(|m| m.as_str()).unwrap_or("am").to_string(),
                    device: di,
                    channel: ci,
                    line,
                });
            }
        }
    }

    for (name, mixer) in groups(cfg.get("mixers")) {
        for out in mixer.get("outputs").map(|o| o.items()).unwrap_or(&[]) {
            if is_udp(out) && !disabled(out) {
                skip(&mut f, out.line, format!("udp_stream on mixer {name:?}: a mixer combines channels, so its audio can't be attributed to one"));
            }
        }
    }

    // Two outputs on one port can't be told apart; keep the first.
    let mut seen: BTreeMap<u16, u32> = BTreeMap::new();
    let mut kept = Vec::new();
    for s in std::mem::take(&mut f.streams) {
        if let Some(first) = seen.get(&s.port) {
            f.skipped.push(Skipped {
                line: s.line,
                reason: format!("port {} is already used by the output at line {first}", s.port),
            });
        } else {
            seen.insert(s.port, s.line);
            kept.push(s);
        }
    }
    f.streams = kept;
    f.skipped.sort_by_key(|s| s.line);
    f
}

fn groups(v: Option<&Value>) -> Vec<(String, &Value)> {
    match v.map(|v| &v.kind) {
        Some(Kind::Group(s)) => s.iter().map(|s| (s.name.clone(), &s.value)).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airband::libconfig::parse;

    const READ5: &str = r#"
devices: ({
  type = "rtlsdr"; index = 0; gain = 30; centerfreq = 118.7;
  channels: (
    { freq = 118.1; label = "Jandakot Tower";
      outputs: ( { type = "icecast"; server = "x"; port = 8000; mountpoint = "a.mp3"; },
                 { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6101; continuous = true; } ); },
    { freq = 119900000; modulation = "am";
      outputs: ( { type = "udp_stream"; dest_address = "localhost"; dest_port = "6102"; continuous = true; } ); },
    { freq = "126.0M";
      outputs: ( { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6103; } ); },
    { freq = 120.5;
      outputs: ( { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6101; continuous = true; } ); },
    { freq = 121.0; disable = true;
      outputs: ( { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6109; continuous = true; } ); }
  );
},
{
  type = "rtlsdr"; index = 1; mode = "scan";
  channels: ( { freqs = ( 118.1, 119.9 );
      outputs: ( { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6200; continuous = true; } ); } );
});
mixers: {
  mix1: { outputs: ( { type = "udp_stream"; dest_address = "127.0.0.1"; dest_port = 6300; continuous = true; } ); };
};
"#;

    #[test]
    fn finds_usable_streams_and_says_why_others_are_skipped() {
        let f = find(&parse(READ5).unwrap());
        let got: Vec<(u16, u32, &str)> = f.streams.iter().map(|s| (s.port, s.khz(), s.label.as_str())).collect();
        assert_eq!(got, vec![(6101, 118100, "Jandakot Tower"), (6102, 119900, "")]);
        assert!(f.streams.iter().all(|s| s.is_loopback()));
        let reasons: Vec<&str> = f.skipped.iter().map(|s| s.reason.as_str()).collect();
        assert_eq!(reasons.len(), 4, "{reasons:#?}");
        assert!(reasons.iter().any(|r| r.contains("continuous = true")));
        assert!(reasons.iter().any(|r| r.contains("already used")));
        assert!(reasons.iter().any(|r| r.contains("scan mode")));
        assert!(reasons.iter().any(|r| r.contains("mixer \"mix1\"")));
    }

    #[test]
    fn frequencies_follow_rtl_airband() {
        let v = parse("a = 118100000; b = 118.1; c = \"118.1M\"; d = \"118100k\";").unwrap();
        for k in ["a", "b", "c", "d"] {
            assert_eq!(freq_hz(v.get(k).unwrap()), Some(118_100_000), "{k}");
        }
    }

    #[test]
    fn upstream_examples_have_no_udp_streams() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rtl_airband");
        let v = crate::airband::libconfig::parse_file(&dir.join("two_dongles_multiple_outputs.conf")).unwrap();
        let f = find(&v);
        assert!(f.streams.is_empty() && f.skipped.is_empty());
    }
}
