//! The pipeline over the spool: claim, encode, seal, upload, delete. Every
//! step is idempotent and every transition is a rename, so a crash anywhere
//! resumes from a directory walk.
//!
//! Recording is continuous. Minutes from process start onward are sealed
//! contiguously. A channel gets an archive only for a minute it has audio in;
//! every sealed minute gets the receiver's marker (`_up`), so "quiet" (marker,
//! no archive) is distinguishable from "receiver down" (neither). Utterances
//! left over from before this process started are sealed in their own
//! minutes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, error, info, warn};

use crate::audio::{encode_ogg_opus, Conditioner, EncodeParams, WavError, CHAIN_VERSION};
use crate::config::Config;
use crate::ingest::Ingest;
use crate::manifest::{
    self, AudioDoc, ChannelDoc, Clean, Layouts, Location, Manifest, Processing, ReceiverDoc, Sidecar,
    UpManifest, Utterance,
};
use crate::spool::layout::UP_CHANNEL;
use crate::spool::time::{floor_minute, iso_ms, minute_iso, minute_key, minute_start_ms, now_ms, MINUTE_MS};
use crate::spool::zipw::{build_stored_zip, write_stored_zip, ZipEntry};
use crate::spool::{atomic_write, Driver, Spool};

/// How the UDP input marks the times it writes into each WAV.
pub const TIMING: &str = "rtl_airband-udp";

pub struct Pipeline {
    cfg: Config,
    spool: Spool,
    driver: Driver,
    started_ms: i64,
    /// Last minute sealed by the contiguous schedule (>= process start).
    last_sealed: Option<i64>,
    encode: EncodeParams,
    /// Present when `[process] enabled`; produces the `.clean.opus` variant.
    conditioner: Option<Conditioner>,
    /// Sample rate of the audio, for the receiver document: rtl_airband's
    /// build decides it (8 or 16 kHz), and we learn it from the first WAV.
    sample_rate: u32,
    /// Channels whose input was down during a minute.
    down_source: Option<DownSource>,
    pub stats: Stats,
}

pub type DownSource = std::sync::Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Stats {
    pub claimed: u64,
    pub encoded: u64,
    pub failed: u64,
    pub conditioned: u64,
    pub condition_failed: u64,
    /// Wall time inside the conditioner, summed.
    pub condition_ms: u64,
    pub sealed_minutes: u64,
    pub last_error: Option<String>,
}

impl Pipeline {
    pub fn new(cfg: Config) -> Result<Self> {
        let spool = Spool::new(&cfg.spool.dir);
        fs::create_dir_all(spool.root()).context("create spool dir")?;
        fs::create_dir_all(spool.failed_dir())?;
        fs::create_dir_all(spool.incoming_dir())?;
        let store = if cfg.run.dry_run { None } else { Some(Ingest::new(&cfg.upload.url, &cfg.upload.key)) };
        let encode = EncodeParams {
            bitrate_bps: (cfg.encode.bitrate_kbps * 1000) as i32,
            complexity: cfg.encode.complexity,
        };
        let conditioner = cfg.process.enabled.then(|| {
            let c = Conditioner::new(cfg.process.to_params());
            info!(chain = c.chain(), version = CHAIN_VERSION, "conditioning enabled");
            c
        });
        let mut driver = Driver::new(
            spool.clone(),
            store,
            cfg.receiver.id.clone(),
            Box::new(|ch, mk, r| Spool::audio_key(ch, mk, r)),
            cfg.upload.concurrency,
            true,
        );
        // A reader takes the marker to mean the minute's archives are all there.
        driver.upload_last(UP_CHANNEL);
        let sample_rate = previous_sample_rate(&spool).unwrap_or(8000);
        let mut p = Self {
            cfg,
            spool,
            driver,
            started_ms: now_ms(),
            last_sealed: None,
            encode,
            conditioner,
            sample_rate,
            down_source: None,
            stats: Stats::default(),
        };
        p.write_receiver_doc_local(None)?;
        p.driver.set_doc(Spool::receiver_key(&p.cfg.receiver.id), p.spool.receiver_doc_path(), "application/json");
        info!(receiver = %p.cfg.receiver.id, channels = p.cfg.channels.len(), "pipeline ready");
        Ok(p)
    }

    /// Name the channels that were not listening for some of a minute; the
    /// marker lists them so their silence isn't read as a quiet frequency.
    pub fn set_down_source(&mut self, f: DownSource) {
        self.down_source = Some(f);
    }

    pub fn receiver_id(&self) -> &str {
        &self.cfg.receiver.id
    }

    pub fn spool(&self) -> &Spool {
        &self.spool
    }

    fn processing(&self) -> Option<Processing> {
        self.conditioner.as_ref().map(|c| Processing { chain: c.chain().to_string(), version: CHAIN_VERSION })
    }

    // ── 1+2. claim and encode ─────────────────────────────────────────────

    /// Claim and encode every finished WAV in `incoming/`.
    pub fn scan_and_claim(&mut self) -> Result<()> {
        if self.backlog_minutes()? > self.cfg.spool.max_backlog_minutes {
            warn!("backlog exceeds max_backlog_minutes; not claiming new utterances");
            return Ok(());
        }
        let dir = self.spool.incoming_dir();
        let mut names: Vec<PathBuf> = fs::read_dir(&dir)
            .with_context(|| format!("read_dir {}", dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().map(|x| x.eq_ignore_ascii_case("wav")).unwrap_or(false))
            .collect();
        names.sort();
        for path in names {
            match self.claim_one(&path) {
                Ok(true) => {}
                Ok(false) => debug!(file = %path.display(), "not ready; will retry"),
                Err(e) => {
                    self.stats.failed += 1;
                    self.stats.last_error = Some(format!("{}: {e:#}", path.display()));
                    error!(file = %path.display(), error = %format!("{e:#}"), "claim failed; moving to failed/");
                    let _ = self.move_to_failed(&path, &format!("{e:#}"));
                }
            }
        }
        Ok(())
    }

    /// Ok(false): retry later (still being written). Ok(true): claimed and
    /// encoded. Err: unprocessable.
    fn claim_one(&mut self, path: &Path) -> Result<bool> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
            Err(e) => return Err(e).context("read wav"),
        };
        let wav = match crate::audio::wav::parse(&bytes) {
            Ok(w) => w,
            Err(WavError::Truncated { .. }) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let fields = wav.info.get("ICMT").map(|c| parse_comment(c)).unwrap_or_default();
        let khz = fields
            .get("Frequency")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|hz| ((hz + 500) / 1000) as u32)
            .with_context(|| format!("no Frequency in {name}"))?;
        let Some(ch) = self.cfg.channel_for_khz(khz) else {
            bail!("{khz} kHz has no [[channels]] entry");
        };
        let (channel, label) = (ch.id.clone(), ch.label.clone());
        let dur_ms = wav.duration_ms();
        let start_ms = fields
            .get("Start")
            .and_then(|v| v.parse::<i64>().ok())
            .with_context(|| format!("no Start in {name}"))?;
        let end_ms = fields
            .get("End")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|e| *e >= start_ms)
            .unwrap_or(start_ms + dur_ms as i64);

        if wav.sample_rate != self.sample_rate {
            self.sample_rate = wav.sample_rate;
            self.write_receiver_doc_local(None)?;
            self.driver.mark_doc_dirty(&Spool::receiver_key(&self.cfg.receiver.id));
        }

        // Minute choice: end_ms's minute unless already sealed, else now.
        let now = now_ms();
        if end_ms > now + MINUTE_MS {
            warn!(file = %name, end = %iso_ms(end_ms), "utterance timestamp is in the future; clock skew?");
        }
        let mut mk = minute_key(end_ms);
        if self.spool.is_sealed(&mk) {
            mk = minute_key(now);
            debug!(file = %name, minute = %mk, "target minute already sealed; using current minute");
        }
        let dir = self.spool.wav_channel_dir(&mk, &channel);
        fs::create_dir_all(&dir)?;

        // Claim: atomic rename into the spool.
        let claimed = unique_path(&dir.join(&name));
        fs::rename(path, &claimed).with_context(|| format!("claim {} -> {}", path.display(), claimed.display()))?;
        self.stats.claimed += 1;

        // Encode.
        let mut stem = format!("{start_ms}-{dur_ms}");
        let mut n = 1;
        while dir.join(format!("{stem}.opus")).exists() || dir.join(format!("{stem}.json")).exists() {
            stem = format!("{start_ms}-{dur_ms}-{n}");
            n += 1;
        }
        let tags = vec![
            ("START_MS".to_string(), start_ms.to_string()),
            ("END_MS".to_string(), end_ms.to_string()),
            ("DUR_MS".to_string(), dur_ms.to_string()),
            ("TIMING".to_string(), TIMING.to_string()),
            ("CHANNEL".to_string(), channel.clone()),
            ("RECEIVER".to_string(), self.cfg.receiver.id.clone()),
            ("FREQ_KHZ".to_string(), khz.to_string()),
            ("LABEL".to_string(), label.clone()),
            ("SOURCE".to_string(), name.clone()),
        ];
        let ogg = encode_ogg_opus(&wav.samples, wav.sample_rate, &self.encode, &tags)
            .with_context(|| format!("encode {name}"))?;
        atomic_write(&dir.join(format!("{stem}.opus")), &ogg)?;

        let mut side = Sidecar {
            channel: channel.clone(),
            frequency_khz: khz,
            label,
            utterance: Utterance {
                file: format!("{stem}.opus"),
                start_ms,
                end_ms,
                dur_ms,
                timing: TIMING.to_string(),
                source_name: name.clone(),
                peak_dbfs: wav.peak_dbfs().map(round1),
                clean: None,
            },
        };
        let side_path = dir.join(format!("{stem}.json"));
        atomic_write(&side_path, &serde_json::to_vec_pretty(&side)?)?;
        self.stats.encoded += 1;
        info!(
            file = %name, channel = %channel, minute = %mk, start = %iso_ms(start_ms), dur_ms,
            wav_bytes = bytes.len(), opus_bytes = ogg.len(), "claimed and encoded"
        );

        // Conditioned variant: best effort, after the raw record is durable.
        // A failure here ships the utterance raw only.
        if let Some(cond) = &self.conditioner {
            let t0 = std::time::Instant::now();
            let result = cond.condition(&wav.samples, wav.sample_rate).and_then(|c| {
                let mut ctags = tags.clone();
                ctags.push(("PROCESSING".to_string(), format!("{}@{}", cond.chain(), CHAIN_VERSION)));
                let ogg = encode_ogg_opus(&c.samples, wav.sample_rate, &self.encode, &ctags)?;
                Ok((c, ogg))
            });
            let ms = t0.elapsed().as_millis() as u64;
            match result {
                Ok((c, cogg)) => {
                    let clean_name = format!("{stem}.clean.opus");
                    atomic_write(&dir.join(&clean_name), &cogg)?;
                    side.utterance.clean = Some(Clean {
                        file: clean_name,
                        chain: cond.chain().to_string(),
                        version: CHAIN_VERSION,
                        peak_dbfs: c.peak_dbfs.map(round1),
                        lufs: c.lufs.map(round1),
                    });
                    atomic_write(&side_path, &serde_json::to_vec_pretty(&side)?)?;
                    self.stats.conditioned += 1;
                    self.stats.condition_ms += ms;
                    debug!(
                        file = %name, ms, clean_bytes = cogg.len(), input_lufs = ?c.input_lufs.map(round1),
                        lufs = ?c.lufs.map(round1), peak_dbfs = ?c.peak_dbfs.map(round1), "conditioned"
                    );
                }
                Err(e) => {
                    self.stats.condition_failed += 1;
                    self.stats.last_error = Some(format!("condition {name}: {e:#}"));
                    warn!(file = %name, error = %format!("{e:#}"), "conditioning failed; utterance ships raw only");
                }
            }
        }
        Ok(true)
    }

    fn move_to_failed(&self, path: &Path, err: &str) -> Result<()> {
        let dir = self.spool.failed_dir();
        fs::create_dir_all(&dir)?;
        let name = path.file_name().unwrap_or_default();
        let dest = unique_path(&dir.join(name));
        fs::rename(path, &dest)?;
        let mut errp = dest.clone().into_os_string();
        errp.push(".err");
        fs::write(errp, err)?;
        Ok(())
    }

    // ── 3. seal ───────────────────────────────────────────────────────────

    /// Seal every due minute: leftover minutes from before the start
    /// individually, then every minute from process start up to
    /// `now - 60s - grace` contiguously.
    pub fn seal_due(&mut self) -> Result<()> {
        let now = now_ms();
        let cutoff = floor_minute(now - (self.cfg.run.grace_secs as i64) * 1000) - MINUTE_MS;
        self.seal_through(cutoff)
    }

    /// Force-seal everything up to and including the current minute (shutdown).
    pub fn seal_all_now(&mut self) -> Result<()> {
        self.seal_through(floor_minute(now_ms()))
    }

    fn seal_through(&mut self, last: i64) -> Result<()> {
        let first = match self.last_sealed {
            Some(m) => m + MINUTE_MS,
            None => floor_minute(self.started_ms),
        };
        // Leftovers: utterances written before a restart land in minutes
        // before `first`. Seal those individually; the minutes between them
        // and the process start were not recorded, so no markers for them. A
        // minute an earlier run sealed without a marker gets one here.
        let mut backlog: BTreeSet<String> = self.spool.list_wav_minutes()?.into_iter().collect();
        backlog.extend(self.spool.list_out_minutes()?);
        for mk in backlog {
            let Some(ms) = minute_start_ms(&mk) else { continue };
            if ms < first && ms <= last && !minute_fully_sealed(&self.spool, &mk)? {
                self.seal_minute(&mk)?;
            }
        }
        let mut m = first;
        while m <= last {
            let mk = minute_key(m);
            if !minute_fully_sealed(&self.spool, &mk)? {
                self.seal_minute(&mk)?;
            }
            self.last_sealed = Some(m);
            m += MINUTE_MS;
        }
        Ok(())
    }

    /// One archive per channel with claimed utterances, then the marker. The
    /// marker is written last, so a minute that has it is complete.
    fn seal_minute(&mut self, mk: &str) -> Result<()> {
        let channels = self.spool.list_channels(mk)?;
        let mut existing: BTreeSet<String> =
            self.spool.list_archives(mk)?.into_iter().map(|(c, _)| c).collect();
        let have_up = existing.remove(UP_CHANNEL);
        let mut wrote = 0;
        for ch in channels {
            if existing.contains(&ch) {
                continue; // written exactly once
            }
            let (bytes, utterances) = self.build_channel_archive(mk, &ch)?;
            let path = self.spool.zip_path(mk, &ch);
            atomic_write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
            existing.insert(ch.clone());
            debug!(minute = mk, channel = %ch, utterances, "sealed archive");
            wrote += 1;
        }
        if have_up {
            return Ok(());
        }
        let down = self.down_source.as_ref().map(|f| f(mk)).unwrap_or_default();
        let up = UpManifest {
            format: manifest::FORMAT,
            minute: minute_iso(mk),
            receiver: self.cfg.receiver.id.clone(),
            channels: existing.into_iter().collect(),
            down,
        };
        let entries = [ZipEntry::stored("manifest.json", serde_json::to_vec_pretty(&up)?)];
        write_stored_zip(&self.spool.zip_path(mk, UP_CHANNEL), &entries)?;
        self.stats.sealed_minutes += 1;
        info!(minute = mk, archives = wrote, "sealed minute");
        Ok(())
    }

    /// One channel's archive for a minute: `manifest.json`, then each
    /// utterance's raw entry followed by its clean one. Returns the bytes and
    /// the number of utterances.
    fn build_channel_archive(&self, mk: &str, ch: &str) -> Result<(Vec<u8>, u32)> {
        let cfg_ch = self.cfg.channel_by_id(ch);
        let mut khz = cfg_ch.map(|c| c.khz).unwrap_or(0);
        let mut label = cfg_ch.map(|c| c.label.clone()).unwrap_or_default();
        let dir = self.spool.wav_channel_dir(mk, ch);
        let mut utts: Vec<(Utterance, PathBuf)> = Vec::new();
        if dir.is_dir() {
            for e in fs::read_dir(&dir)? {
                let p = e?.path();
                if p.extension().map(|x| x == "json").unwrap_or(false) {
                    let side: Sidecar = serde_json::from_slice(&fs::read(&p)?)
                        .with_context(|| format!("sidecar {}", p.display()))?;
                    if khz == 0 {
                        khz = side.frequency_khz;
                    }
                    if label.is_empty() {
                        label = side.label.clone();
                    }
                    let opus = dir.join(&side.utterance.file);
                    if !opus.exists() {
                        warn!(path = %opus.display(), "sidecar without opus; skipping entry");
                        continue;
                    }
                    let mut u = side.utterance;
                    if let Some(c) = &u.clean {
                        if !dir.join(&c.file).exists() {
                            warn!(path = %dir.join(&c.file).display(), "sidecar names a missing clean file; shipping raw only");
                            u.clean = None;
                        }
                    }
                    utts.push((u, opus));
                }
            }
        }
        utts.sort_by_key(|(u, _)| (u.start_ms, u.file.clone()));
        let manifest = Manifest {
            format: manifest::FORMAT,
            minute: minute_iso(mk),
            channel: ch.to_string(),
            receiver: self.cfg.receiver.id.clone(),
            frequency_khz: khz,
            label,
            processing: self.processing(),
            utterances: utts.iter().map(|(u, _)| u.clone()).collect(),
        };
        let mut entries = vec![ZipEntry::stored("manifest.json", serde_json::to_vec_pretty(&manifest)?)];
        for (u, p) in &utts {
            entries.push(ZipEntry::stored(u.file.clone(), fs::read(p)?));
            if let Some(c) = &u.clean {
                entries.push(ZipEntry::stored(c.file.clone(), fs::read(dir.join(&c.file))?));
            }
        }
        Ok((build_stored_zip(&entries)?, utts.len() as u32))
    }

    // ── 4+5. upload and delete ────────────────────────────────────────────

    pub async fn upload_due(&mut self) -> Result<()> {
        self.driver.upload_due().await
    }

    pub fn delete_done(&mut self) -> Result<()> {
        let spool = self.spool.clone();
        self.driver.delete_done(|mk| minute_fully_sealed(&spool, mk))
    }

    // ── receiver document / status ────────────────────────────────────────

    fn write_receiver_doc_local(&self, ended_ms: Option<i64>) -> Result<()> {
        let r = &self.cfg.receiver;
        let doc = ReceiverDoc {
            format: manifest::FORMAT,
            receiver: r.id.clone(),
            name: r.name.clone(),
            location: match (r.lat, r.lon) {
                (Some(lat), Some(lon)) => Some(Location { lat, lon, description: r.location.clone() }),
                _ => None,
            },
            software: format!("read5-uploader {}", env!("CARGO_PKG_VERSION")),
            started_at: iso_ms(self.started_ms),
            ended_at: ended_ms.map(iso_ms),
            channels: self
                .cfg
                .channels
                .iter()
                .map(|c| ChannelDoc {
                    id: c.id.clone(),
                    khz: c.khz,
                    label: c.label.clone(),
                    service: c.service.clone(),
                    sector: c.sector.clone(),
                    fir: c.fir.clone(),
                    description: c.description.clone(),
                })
                .collect(),
            audio: AudioDoc {
                container: "ogg".into(),
                codec: "opus".into(),
                sample_rate: self.sample_rate,
                archive: "audio/{channel}/{YYYYMMDD}/{HHMM}-{receiver}.zip".into(),
                processing: self.processing(),
            },
            layouts: Some(Layouts { channel: true, minute: false }),
        };
        atomic_write(&self.spool.receiver_doc_path(), &serde_json::to_vec_pretty(&doc)?)?;
        Ok(())
    }

    /// Mark the receiver stopped locally and schedule the final doc upload.
    pub fn end_run(&mut self) -> Result<()> {
        self.write_receiver_doc_local(Some(now_ms()))?;
        self.driver.mark_doc_dirty(&Spool::receiver_key(&self.cfg.receiver.id));
        Ok(())
    }

    pub fn receiver_doc_uploaded(&self) -> bool {
        self.driver.docs_uploaded()
    }

    /// Minutes of sealed-but-unuploaded output.
    pub fn backlog_minutes(&self) -> Result<u64> {
        self.driver.backlog_minutes()
    }

    pub fn status_json(&self) -> Result<serde_json::Value> {
        let waiting = fs::read_dir(self.spool.incoming_dir())
            .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().extension().map(|x| x == "wav").unwrap_or(false)).count())
            .unwrap_or(0);
        let backlog = self.backlog_minutes()?;
        let unsealed: Vec<String> =
            self.spool.list_wav_minutes()?.into_iter().filter(|m| !self.spool.is_sealed(m)).collect();
        let degraded = backlog > self.cfg.spool.max_backlog_minutes || self.driver.failing();
        let mut stats = serde_json::to_value(&self.stats)?;
        if let (Some(a), serde_json::Value::Object(b)) = (stats.as_object_mut(), serde_json::to_value(&self.driver.stats)?) {
            for (k, v) in b {
                if k == "last_error" && v.is_null() {
                    continue;
                }
                a.insert(k, v);
            }
        }
        Ok(serde_json::json!({
            "state": if degraded { "degraded" } else { "ok" },
            "receiver": self.cfg.receiver.id,
            "channels": self.cfg.channels.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            "startedAt": iso_ms(self.started_ms),
            "receiverDocUploaded": self.driver.docs_uploaded(),
            "utterancesWaiting": waiting,
            "unsealedMinutes": unsealed.len(),
            "oldestUnsealedMinute": unsealed.first(),
            "uploadBacklogMinutes": backlog,
            "dryRun": self.cfg.run.dry_run,
            "stats": stats,
            "updatedAt": iso_ms(now_ms()),
        }))
    }
}

/// Every channel with claimed utterances has an archive for the minute, and
/// the minute has the receiver's marker (written last).
fn minute_fully_sealed(spool: &Spool, mk: &str) -> Result<bool> {
    let have: BTreeSet<String> = spool.list_archives(mk)?.into_iter().map(|(c, _)| c).collect();
    let mut need: BTreeSet<String> = spool.list_channels(mk)?.into_iter().collect();
    need.insert(UP_CHANNEL.to_string());
    Ok(need.is_subset(&have))
}

/// The sample rate in the last receiver document, so a restart does not
/// announce 8 kHz until the first utterance.
fn previous_sample_rate(spool: &Spool) -> Option<u32> {
    let doc: ReceiverDoc = serde_json::from_slice(&fs::read(spool.receiver_doc_path()).ok()?).ok()?;
    Some(doc.audio.sample_rate).filter(|r| *r > 0)
}

/// The `Key:Value;` list the UDP input writes into each WAV's `ICMT` tag.
pub fn parse_comment(icmt: &str) -> BTreeMap<String, String> {
    icmt.split(';')
        .filter_map(|kv| {
            let (k, v) = kv.split_once(':')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

fn round1(v: f32) -> f32 {
    ((v * 10.0).round() / 10.0) + 0.0
}

/// `p`, or `p` with `-1`, `-2`, ... inserted before the extension if taken.
fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ext = p.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 1.. {
        let name = match &ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        };
        let cand = p.with_file_name(name);
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spool::zipw::read_zip;

    const A: &str = "ypjt-twr-118100";

    fn fresh_root() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("read5-pipe-{}-{}-{n}", std::process::id(), now_ms()))
    }

    fn pipeline(root: &Path) -> Pipeline {
        let toml = format!(
            r#"
[receiver]
id = "9"
[rtl_airband]
config = '/nonexistent.conf'
[spool]
dir = '{}'
[[channels]]
id = "{A}"
khz = 118100
label = "Jandakot Tower"
[run]
dry_run = true
"#,
            root.join("spool").display()
        );
        Pipeline::new(Config::parse(&toml).unwrap()).unwrap()
    }

    #[test]
    fn a_minute_is_sealed_by_its_marker_not_by_an_archive_per_channel() {
        let spool = Spool::new(fresh_root());
        let mk = "20261009T0146";
        fs::create_dir_all(spool.wav_channel_dir(mk, A)).unwrap();
        fs::create_dir_all(spool.out_minute_dir(mk)).unwrap();
        assert!(!minute_fully_sealed(&spool, mk).unwrap());
        fs::write(spool.zip_path(mk, A), b"zip").unwrap();
        assert!(!minute_fully_sealed(&spool, mk).unwrap());
        fs::write(spool.zip_path(mk, UP_CHANNEL), b"zip").unwrap();
        assert!(minute_fully_sealed(&spool, mk).unwrap());

        // A quiet minute: the marker alone.
        let quiet = "20261009T0147";
        fs::create_dir_all(spool.out_minute_dir(quiet)).unwrap();
        fs::write(spool.uploaded_path(quiet, UP_CHANNEL), b"zip").unwrap();
        assert!(minute_fully_sealed(&spool, quiet).unwrap());
        fs::remove_dir_all(spool.root()).unwrap();
    }

    /// A WAV as the UDP input writes it goes all the way to a sealed archive
    /// and marker, with the receiver document reporting its sample rate.
    #[test]
    fn an_utterance_is_claimed_encoded_and_sealed() {
        let root = fresh_root();
        let mut p = pipeline(&root);
        let start: i64 = 1_792_994_535_000; // 2026-10-26T06:02:15Z
        let pcm: Vec<i16> = (0..16000).map(|i| ((i as f32 * 0.3).sin() * 8000.0) as i16).collect();
        let icmt = format!("Frequency:118100000;Name:Jandakot Tower;Start:{start};End:{};Timing:rtl_airband-udp;Port:6101;", start + 1000);
        let wav = crate::audio::wav::write_pcm16_info(&pcm, 16000, &[("ICMT", &icmt)]);
        fs::write(p.spool.incoming_dir().join("rtl_airband_118100_x.wav"), wav).unwrap();
        // An utterance for a frequency we have no channel for is set aside.
        let other = "Frequency:119900000;Start:1;End:2;";
        let wav2 = crate::audio::wav::write_pcm16_info(&pcm, 16000, &[("ICMT", other)]);
        fs::write(p.spool.incoming_dir().join("rtl_airband_119900_y.wav"), wav2).unwrap();
        p.scan_and_claim().unwrap();
        assert_eq!((p.stats.claimed, p.stats.encoded, p.stats.conditioned, p.stats.failed), (1, 1, 1, 1));

        let mk = "20261026T0602";
        p.seal_minute(mk).unwrap();
        let z = read_zip(&fs::read(p.spool.zip_path(mk, A)).unwrap()).unwrap();
        let names: Vec<&str> = z.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["manifest.json", "1792994535000-1000.opus", "1792994535000-1000.clean.opus"]);
        let m: Manifest = serde_json::from_slice(&z[0].bytes).unwrap();
        assert_eq!((m.receiver.as_str(), m.frequency_khz, m.utterances[0].timing.as_str()), ("9", 118100, TIMING));
        assert_eq!(m.processing.unwrap().chain, "hpf250+comp-24/3+lufs-18");

        let up = read_zip(&fs::read(p.spool.zip_path(mk, UP_CHANNEL)).unwrap()).unwrap();
        let up: UpManifest = serde_json::from_slice(&up[0].bytes).unwrap();
        assert_eq!(up.channels, [A]);
        assert!(minute_fully_sealed(&p.spool, mk).unwrap());

        let doc: ReceiverDoc = serde_json::from_slice(&fs::read(p.spool.receiver_doc_path()).unwrap()).unwrap();
        assert_eq!(doc.audio.sample_rate, 16000);
        assert_eq!(doc.layouts, Some(Layouts { channel: true, minute: false }));
        drop(p);
        // A restart remembers the rate.
        let q = pipeline(&root);
        assert_eq!(q.sample_rate, 16000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn comment_fields() {
        let f = parse_comment("Frequency:118100000;Name:Jandakot Tower;Start:1000;End:3000;");
        assert_eq!(f["Frequency"], "118100000");
        assert_eq!(f["Name"], "Jandakot Tower");
        assert_eq!(f["End"], "3000");
    }
}
