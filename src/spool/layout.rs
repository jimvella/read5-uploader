//! Paths and directory walks over the spool, and the object key scheme.
//!
//! Recording is continuous, so there are no sessions. Identity is a
//! **channel** (a published service on a frequency, e.g. `ypjt-twr-118100`)
//! recorded by a **receiver** (an id Read5 issues, such as `3`). Objects:
//!
//! ```text
//! receivers/{receiver}.json
//! audio/{channel}/{YYYYMMDD}/{HHMM}-{receiver}.zip
//! audio/_up/{YYYYMMDD}/{HHMM}-{receiver}.zip
//! ```
//!
//! A channel-minute with no audio has no object. `_up` is the receiver's
//! minute marker: it says the receiver was running that minute, so a channel
//! without an archive was quiet rather than unrecorded.
//!
//! Spool:
//!
//! ```text
//! {root}/incoming/                           utterance WAVs from the UDP input
//! {root}/wav/{minute}/{channel}/...          claimed WAVs and their Opus encodes
//! {root}/out/{minute}/{channel}.zip          sealed archive awaiting upload
//! {root}/out/{minute}/{channel}.zip.uploaded upload confirmed
//! {root}/receiver.json                       local copy of the receiver document
//! {root}/failed/                             inputs that could not be processed
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const ZIP_EXT: &str = "zip";
pub const UPLOADED_SUFFIX: &str = ".zip.uploaded";
/// The receiver's minute marker, kept in the spool and keyed as if it were a
/// channel. Not a valid channel slug, so it cannot collide.
pub const UP_CHANNEL: &str = "_up";

#[derive(Debug, Clone)]
pub struct Spool {
    root: PathBuf,
}

impl Spool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the UDP input drops finished utterances for the pipeline.
    pub fn incoming_dir(&self) -> PathBuf {
        self.root.join("incoming")
    }

    pub fn failed_dir(&self) -> PathBuf {
        self.root.join("failed")
    }

    pub fn receiver_doc_path(&self) -> PathBuf {
        self.root.join("receiver.json")
    }

    pub fn wav_root(&self) -> PathBuf {
        self.root.join("wav")
    }

    pub fn out_root(&self) -> PathBuf {
        self.root.join("out")
    }

    pub fn wav_minute_dir(&self, minute: &str) -> PathBuf {
        self.wav_root().join(minute)
    }

    pub fn wav_channel_dir(&self, minute: &str, channel: &str) -> PathBuf {
        self.wav_minute_dir(minute).join(channel)
    }

    pub fn out_minute_dir(&self, minute: &str) -> PathBuf {
        self.out_root().join(minute)
    }

    pub fn zip_path(&self, minute: &str, channel: &str) -> PathBuf {
        self.out_minute_dir(minute).join(format!("{channel}.{ZIP_EXT}"))
    }

    pub fn uploaded_path(&self, minute: &str, channel: &str) -> PathBuf {
        self.out_minute_dir(minute)
            .join(format!("{channel}{UPLOADED_SUFFIX}"))
    }

    /// `audio/{channel}/{YYYYMMDD}/{HHMM}-{receiver}.zip` from a
    /// `YYYYMMDDTHHMM` minute key.
    pub fn audio_key(channel: &str, minute: &str, receiver: &str) -> String {
        let (day, hhmm) = split_minute(minute);
        format!("audio/{channel}/{day}/{hhmm}-{receiver}.{ZIP_EXT}")
    }

    pub fn receiver_key(receiver: &str) -> String {
        format!("receivers/{receiver}.json")
    }

    /// Minutes with claimed sources, ascending.
    pub fn list_wav_minutes(&self) -> Result<Vec<String>> {
        let mut v = list_dirs(&self.wav_root())?;
        v.sort();
        Ok(v)
    }

    /// Minutes with sealed output (zips or markers), ascending.
    pub fn list_out_minutes(&self) -> Result<Vec<String>> {
        let mut v = list_dirs(&self.out_root())?;
        v.sort();
        Ok(v)
    }

    /// Channel directories under a wav minute.
    pub fn list_channels(&self, minute: &str) -> Result<Vec<String>> {
        let mut v = list_dirs(&self.wav_minute_dir(minute))?;
        v.sort();
        Ok(v)
    }

    /// `(channel, uploaded?)` for every archive in an out minute.
    pub fn list_archives(&self, minute: &str) -> Result<Vec<(String, bool)>> {
        let dir = self.out_minute_dir(minute);
        let mut v = Vec::new();
        if !dir.exists() {
            return Ok(v);
        }
        for e in fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(ch) = name.strip_suffix(UPLOADED_SUFFIX) {
                v.push((ch.to_string(), true));
            } else if let Some(ch) = name.strip_suffix(".zip") {
                v.push((ch.to_string(), false));
            }
        }
        v.sort();
        Ok(v)
    }

    /// A minute is sealed once its out directory exists. Late arrivals for a
    /// sealed minute are redirected to the current minute by the caller.
    pub fn is_sealed(&self, minute: &str) -> bool {
        self.out_minute_dir(minute).is_dir()
    }

    /// Remove a minute's sources and outputs after every archive is confirmed.
    pub fn delete_minute(&self, minute: &str) -> Result<()> {
        for d in [self.wav_minute_dir(minute), self.out_minute_dir(minute)] {
            if d.exists() {
                fs::remove_dir_all(&d).with_context(|| format!("remove {}", d.display()))?;
            }
        }
        Ok(())
    }
}

/// `YYYYMMDDTHHMM` -> (`YYYYMMDD`, `HHMM`). Falls back to the whole key as
/// the day if malformed, so a bad key is visible rather than silently mapped.
pub fn split_minute(minute: &str) -> (&str, &str) {
    match minute.split_once('T') {
        Some((d, t)) if d.len() == 8 && t.len() == 4 => (d, t),
        _ => (minute, "0000"),
    }
}

fn list_dirs(dir: &Path) -> Result<Vec<String>> {
    let mut v = Vec::new();
    if !dir.exists() {
        return Ok(v);
    }
    for e in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let e = e?;
        if e.file_type()?.is_dir() {
            v.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(
            Spool::audio_key("ypjt-twr-118100", "20260907T0146", "3"),
            "audio/ypjt-twr-118100/20260907/0146-3.zip"
        );
        assert_eq!(
            Spool::audio_key(UP_CHANNEL, "20260907T0146", "3"),
            "audio/_up/20260907/0146-3.zip"
        );
        assert_eq!(Spool::receiver_key("3"), "receivers/3.json");
        assert_eq!(split_minute("garbage"), ("garbage", "0000"));
    }
}
