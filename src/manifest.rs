//! `manifest.json` inside each channel-minute archive, the per-utterance
//! sidecar that carries the same record between encode and seal, the minute
//! marker and the receiver document. These are the formats Read5 reads.

use serde::{Deserialize, Serialize};

pub const FORMAT: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Utterance {
    /// Entry name inside the archive (`{startMs}-{durMs}.opus`).
    pub file: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub dur_ms: u64,
    /// How the times were found: `rtl_airband-udp` (from datagram arrival).
    pub timing: String,
    pub source_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_dbfs: Option<f32>,
    /// The conditioned variant, when the receiver produced one. Additive:
    /// readers that do not know it still get the raw `file`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub clean: Option<Clean>,
}

/// A conditioned (levelled) second encode of the same utterance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Clean {
    /// Entry name inside the archive (`{startMs}-{durMs}.clean.opus`).
    pub file: String,
    /// Chain identity, e.g. `hpf250+comp-24/3+lufs-18`.
    pub chain: String,
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_dbfs: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lufs: Option<f32>,
}

/// What the receiver's conditioning chain is, once per manifest and in the
/// receiver document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Processing {
    pub chain: String,
    pub version: u32,
}

/// Sidecar written beside each `.opus` in the spool. Superset of the manifest
/// entry so the seal step needs nothing but the directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sidecar {
    pub channel: String,
    pub frequency_khz: u32,
    pub label: String,
    #[serde(flatten)]
    pub utterance: Utterance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: u32,
    /// `2026-09-05T02:28Z`
    pub minute: String,
    pub channel: String,
    pub receiver: String,
    pub frequency_khz: u32,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub processing: Option<Processing>,
    pub utterances: Vec<Utterance>,
}

/// `manifest.json` inside the receiver's minute marker (`audio/_up/...`): the
/// receiver was running this minute, and these channels have an archive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpManifest {
    pub format: u32,
    /// `2026-09-05T02:28Z`
    pub minute: String,
    pub receiver: String,
    pub channels: Vec<String>,
    /// Channels whose input was down for some of the minute: silence there
    /// means "not listening", not "quiet".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub down: Vec<String>,
}

/// One channel as published in the receiver document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelDoc {
    pub id: String,
    pub khz: u32,
    pub label: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub service: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub sector: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub fir: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub description: String,
}

/// `receivers/{id}.json`: what this receiver records and where it is.
/// Overwritten whole at start and on clean stop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiverDoc {
    pub format: u32,
    pub receiver: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    pub software: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    pub channels: Vec<ChannelDoc>,
    pub audio: AudioDoc,
    /// Which archive layouts this receiver writes. Missing (older builds)
    /// means the per-channel layout only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub layouts: Option<Layouts>,
}

/// The archive layouts this receiver writes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Layouts {
    /// `audio/{channel}/...` and the `_up` marker.
    pub channel: bool,
    /// The receiver-minute layout; this uploader does not write it.
    pub minute: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDoc {
    pub container: String,
    pub codec: String,
    pub sample_rate: u32,
    /// Key template for archives.
    pub archive: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub processing: Option<Processing>,
}
