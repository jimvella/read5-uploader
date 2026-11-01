use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::ingest::key_receiver;

pub const DEFAULT_UPLOAD_URL: &str = "https://read5.net/api/ingest";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub receiver: Receiver,
    pub rtl_airband: Airband,
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub upload: Upload,
    pub spool: SpoolCfg,
    #[serde(default)]
    pub encode: Encode,
    #[serde(default)]
    pub process: Process,
    #[serde(default)]
    pub run: Run,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receiver {
    /// The receiver id Read5 issued. Optional: it is also the part of the
    /// upload key between `r5_` and the dot. (env: READ5_RECEIVER_ID)
    #[serde(default)]
    pub id: String,
    /// A short name for the receiver. Published in the receiver document.
    #[serde(default)]
    pub name: String,
    /// Published in the receiver document, so leave them out unless you want
    /// the receiver's position public. A suburb is plenty.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    #[serde(default)]
    pub location: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Airband {
    /// rtl_airband's config, read for its `udp_stream` outputs.
    #[serde(default)]
    pub config: Option<PathBuf>,
    /// Explicit streams, used instead of reading `config` (for example when
    /// rtl_airband runs in Docker).
    #[serde(default)]
    pub streams: Vec<AirbandStream>,
    /// Address to receive on. rtl_airband's `dest_address` must reach it.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Fixed delay from a sample's reception to its batch being sent, beyond
    /// the batch itself. The default is rtl_airband's 12.5 ms AGC look-ahead.
    #[serde(default = "default_latency_ms")]
    pub latency_ms: f64,
    /// Silence this long ends an utterance.
    #[serde(default = "default_split_gap")]
    pub split_gap_ms: u32,
    /// Utterances shorter than this are dropped as blips.
    #[serde(default = "default_min_ms")]
    pub min_ms: u32,
    /// Utterances longer than this are cut.
    #[serde(default = "default_max_ms")]
    pub max_ms: u32,
    /// A stream silent this long is down (rtl_airband stopped or deaf).
    #[serde(default = "default_down_after")]
    pub down_after_secs: u64,
    /// Hold uploads while the system clock reports it is not NTP-synced.
    #[serde(default = "default_true")]
    pub require_ntp: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AirbandStream {
    pub port: u16,
    pub khz: u32,
    #[serde(default)]
    pub label: String,
}

/// A published service on a frequency. The `id` is issued by Read5 and the
/// upload key may only write channels it was issued for.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /// Slug, e.g. `ypjt-twr-118100`. Lowercase letters, digits, `-`.
    pub id: String,
    pub khz: u32,
    pub label: String,
    /// CEN, APP, DEP, TWR, GND, ATIS, CTAF, FIA, ...
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub sector: String,
    /// YMMM or YBBB.
    #[serde(default)]
    pub fir: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    /// (env: READ5_UPLOAD_URL)
    #[serde(default = "default_upload_url")]
    pub url: String,
    /// The receiver's key, `r5_<receiver>.<secret>`. Better kept out of this
    /// file, in READ5_UPLOAD_KEY (the systemd unit reads /etc/read5/read5.env).
    #[serde(default)]
    pub key: String,
    /// Uploads in flight at once.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

impl Default for Upload {
    fn default() -> Self {
        Self { url: default_upload_url(), key: String::new(), concurrency: default_concurrency() }
    }
}

impl std::fmt::Debug for Upload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upload")
            .field("url", &self.url)
            .field("key", &if self.key.is_empty() { "" } else { "<redacted>" })
            .field("concurrency", &self.concurrency)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpoolCfg {
    /// Working directory for utterances and archives until they are uploaded.
    pub dir: PathBuf,
    /// Stop taking new utterances once this many minutes wait for upload.
    #[serde(default = "default_backlog")]
    pub max_backlog_minutes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Encode {
    #[serde(default = "default_bitrate")]
    pub bitrate_kbps: u32,
    #[serde(default = "default_complexity")]
    pub complexity: u8,
}

impl Default for Encode {
    fn default() -> Self {
        Self { bitrate_kbps: default_bitrate(), complexity: default_complexity() }
    }
}

/// A second, "clean" Opus per utterance: levelled and compressed, so every
/// receiver sounds alike. The raw encode is always uploaded as well.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_highpass")]
    pub highpass_hz: f32,
    #[serde(default = "default_comp_threshold")]
    pub comp_threshold_db: f32,
    /// 1 disables the compressor.
    #[serde(default = "default_comp_ratio")]
    pub comp_ratio: f32,
    #[serde(default = "default_comp_attack")]
    pub comp_attack_ms: f32,
    #[serde(default = "default_comp_release")]
    pub comp_release_ms: f32,
    #[serde(default = "default_target_lufs")]
    pub target_lufs: f32,
    #[serde(default = "default_peak_limit")]
    pub peak_limit_dbfs: f32,
}

impl Default for Process {
    fn default() -> Self {
        Self {
            enabled: true,
            highpass_hz: default_highpass(),
            comp_threshold_db: default_comp_threshold(),
            comp_ratio: default_comp_ratio(),
            comp_attack_ms: default_comp_attack(),
            comp_release_ms: default_comp_release(),
            target_lufs: default_target_lufs(),
            peak_limit_dbfs: default_peak_limit(),
        }
    }
}

impl Process {
    pub fn to_params(&self) -> crate::audio::ConditionParams {
        crate::audio::ConditionParams {
            highpass_hz: self.highpass_hz,
            comp_threshold_db: self.comp_threshold_db,
            comp_ratio: self.comp_ratio,
            comp_attack_ms: self.comp_attack_ms,
            comp_release_ms: self.comp_release_ms,
            target_lufs: self.target_lufs,
            peak_limit_dbfs: self.peak_limit_dbfs,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// Record and archive, but upload and delete nothing.
    #[serde(default)]
    pub dry_run: bool,
    /// A JSON status document, rewritten every `status_secs`.
    pub status_file: Option<PathBuf>,
    /// Daily log files here, as well as stderr (the journal under systemd).
    pub log_dir: Option<PathBuf>,
    #[serde(default = "default_status_secs")]
    pub status_secs: u64,
    /// How long after a minute ends before it is sealed, for late utterances.
    #[serde(default = "default_grace")]
    pub grace_secs: u64,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            dry_run: false,
            status_file: None,
            log_dir: None,
            status_secs: default_status_secs(),
            grace_secs: default_grace(),
        }
    }
}

fn default_upload_url() -> String {
    DEFAULT_UPLOAD_URL.to_string()
}
fn default_concurrency() -> usize {
    4
}
fn default_backlog() -> u64 {
    72 * 60
}
fn default_bitrate() -> u32 {
    12
}
fn default_complexity() -> u8 {
    10
}
fn default_grace() -> u64 {
    15
}
fn default_highpass() -> f32 {
    250.0
}
fn default_comp_threshold() -> f32 {
    -24.0
}
fn default_comp_ratio() -> f32 {
    3.0
}
fn default_comp_attack() -> f32 {
    5.0
}
fn default_comp_release() -> f32 {
    150.0
}
fn default_target_lufs() -> f32 {
    -18.0
}
fn default_peak_limit() -> f32 {
    -1.0
}
fn default_true() -> bool {
    true
}
fn default_status_secs() -> u64 {
    10
}
fn default_listen() -> String {
    "127.0.0.1".to_string()
}
fn default_latency_ms() -> f64 {
    12.5
}
fn default_split_gap() -> u32 {
    300
}
fn default_min_ms() -> u32 {
    150
}
fn default_max_ms() -> u32 {
    120_000
}
fn default_down_after() -> u64 {
    5
}

impl Config {
    /// Load the TOML config, then apply environment overrides
    /// (READ5_UPLOAD_KEY, READ5_UPLOAD_URL, READ5_RECEIVER_ID).
    pub fn load(path: &Path) -> Result<Self> {
        let cfg = Self::load_offline(path)?;
        cfg.validate_upload()?;
        Ok(cfg)
    }

    /// As `load`, without requiring an upload key: for subcommands that only
    /// read the config (`streams`).
    pub fn load_offline(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        let mut cfg = Self::parse(&text).with_context(|| format!("config {}", path.display()))?;
        cfg.apply_env();
        cfg.resolve_receiver()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    fn apply_env(&mut self) {
        fn s(name: &str) -> Option<String> {
            std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        }
        if let Some(v) = s("READ5_UPLOAD_KEY") {
            self.upload.key = v;
        }
        if let Some(v) = s("READ5_UPLOAD_URL") {
            self.upload.url = v;
        }
        if let Some(v) = s("READ5_RECEIVER_ID") {
            self.receiver.id = v;
        }
    }

    /// The receiver id, from the config or else from the key.
    fn resolve_receiver(&mut self) -> Result<()> {
        let key = self.upload.key.trim();
        if key.is_empty() {
            return Ok(());
        }
        let Some(from_key) = key_receiver(key) else {
            bail!("the upload key should look like r5_<receiver>.<secret>; check READ5_UPLOAD_KEY");
        };
        if self.receiver.id.is_empty() {
            self.receiver.id = from_key.to_string();
        } else if self.receiver.id != from_key {
            bail!(
                "receiver.id is {:?} but the upload key is for receiver {:?}; remove receiver.id or fix it",
                self.receiver.id,
                from_key
            );
        }
        Ok(())
    }

    fn validate_upload(&self) -> Result<()> {
        if self.run.dry_run {
            return Ok(());
        }
        if self.upload.url.trim().is_empty() {
            bail!("upload.url is empty");
        }
        if self.upload.key.trim().is_empty() {
            bail!("no upload key: set READ5_UPLOAD_KEY (in /etc/read5/read5.env under systemd)");
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        // With no key and no id, validate_upload says "no upload key", which
        // is the better message; a dry run with neither needs the id.
        if self.receiver.id.is_empty() && self.run.dry_run {
            bail!("receiver.id is required when there is no upload key to take it from");
        }
        if !self.receiver.id.is_empty() && !is_slug(&self.receiver.id) {
            bail!("receiver.id must be lowercase letters, digits or '-': got {:?}", self.receiver.id);
        }
        if self.spool.dir.as_os_str().is_empty() {
            bail!("spool.dir is required");
        }
        if self.channels.is_empty() {
            bail!("at least one [[channels]] entry is required");
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.channels {
            if !is_slug(&c.id) {
                bail!("channel id {:?} must be lowercase letters, digits or '-'", c.id);
            }
            if !seen.insert(&c.id) {
                bail!("duplicate channel id {:?}", c.id);
            }
            if self.channels.iter().filter(|o| o.khz == c.khz).count() > 1 {
                bail!("frequency {} kHz appears in more than one channel", c.khz);
            }
        }
        if self.upload.concurrency == 0 {
            bail!("upload.concurrency must be >= 1");
        }
        let a = &self.rtl_airband;
        if a.config.is_none() && a.streams.is_empty() {
            bail!("[rtl_airband] needs `config` (the path of rtl_airband.conf) or a [[rtl_airband.streams]] list");
        }
        if a.min_ms > a.max_ms || a.split_gap_ms == 0 {
            bail!("[rtl_airband]: need split_gap_ms > 0 and min_ms <= max_ms");
        }
        if a.listen.parse::<std::net::IpAddr>().is_err() {
            bail!("rtl_airband.listen must be an IP address, got {:?}", a.listen);
        }
        if self.process.comp_ratio < 1.0 {
            bail!("process.comp_ratio must be >= 1");
        }
        if self.process.peak_limit_dbfs > 0.0 {
            bail!("process.peak_limit_dbfs must be <= 0");
        }
        Ok(())
    }

    pub fn channel_for_khz(&self, khz: u32) -> Option<&Channel> {
        self.channels.iter().find(|c| c.khz == khz)
    }

    pub fn channel_by_id(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }
}

pub fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_parses() {
        let text = include_str!("../deploy/read5-uploader.toml");
        let c = Config::parse(text).unwrap();
        assert_eq!(c.upload.url, DEFAULT_UPLOAD_URL);
        assert!(c.process.enabled);
        assert!(!c.channels.is_empty());
    }

    fn minimal(extra: &str) -> Config {
        let mut c = Config::parse(&format!(
            "{extra}\n[rtl_airband]\nconfig = '/x.conf'\n[spool]\ndir = '/tmp/s'\n[[channels]]\nid = \"ypjt-twr-118100\"\nkhz = 118100\nlabel = \"Jandakot Tower\"\n"
        ))
        .unwrap();
        c.resolve_receiver().and_then(|_| c.validate()).map(|_| c).unwrap()
    }

    #[test]
    fn the_receiver_comes_from_the_key() {
        let c = minimal("[upload]\nkey = \"r5_7.secretsecret\"");
        assert_eq!(c.receiver.id, "7");
        let mut c = Config::parse(
            "[receiver]\nid = \"8\"\n[upload]\nkey = \"r5_7.s\"\n[rtl_airband]\nconfig = '/x'\n[spool]\ndir = '/s'\n[[channels]]\nid = \"a\"\nkhz = 1\nlabel = \"\"",
        )
        .unwrap();
        assert!(c.resolve_receiver().is_err());
    }

    #[test]
    fn the_key_is_not_in_debug_output() {
        let c = minimal("[upload]\nkey = \"r5_7.secretsecret\"");
        let d = format!("{c:?}");
        assert!(!d.contains("secretsecret"), "{d}");
    }

    #[test]
    fn rtl_airband_section_defaults() {
        let a: Airband = toml::from_str("config = '/usr/local/etc/rtl_airband.conf'").unwrap();
        assert_eq!(a.listen, "127.0.0.1");
        assert_eq!((a.split_gap_ms, a.min_ms, a.max_ms, a.down_after_secs), (300, 150, 120_000, 5));
        assert!(a.require_ntp);
        let a: Airband = toml::from_str("[[streams]]\nport = 6101\nkhz = 118100").unwrap();
        assert_eq!(a.streams[0].port, 6101);
    }

    #[test]
    fn slugs() {
        assert!(is_slug("3"));
        assert!(is_slug("ypjt-twr-118100"));
        assert!(!is_slug("Perth"));
        assert!(!is_slug("-x"));
        assert!(!is_slug(""));
    }
}
