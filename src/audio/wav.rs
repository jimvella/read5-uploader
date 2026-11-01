//! Minimal RIFF/WAVE reader.
//!
//! Only what the uploader needs: 16-bit PCM samples (downmixed to mono), the
//! sample rate, and the `LIST/INFO` metadata tags. Written by hand rather than
//! via `hound` because sdrtrunk pads its INFO sub-chunks to four bytes (the
//! RIFF spec says two), which a strict walker reads as garbage after the first
//! odd-length tag. This walker skips zero bytes between chunks instead.

use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("not a RIFF/WAVE file")]
    NotWave,
    #[error("file is truncated: chunk {chunk} claims {claimed} bytes but only {available} remain")]
    Truncated {
        chunk: String,
        claimed: usize,
        available: usize,
    },
    #[error("no fmt chunk")]
    NoFmt,
    #[error("no data chunk")]
    NoData,
    #[error("unsupported format: tag {format_tag}, {bits} bits per sample (only 16-bit PCM is supported)")]
    Unsupported { format_tag: u16, bits: u16 },
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // the format fields are kept for the log and for tests
pub struct Wav {
    pub sample_rate: u32,
    /// Channel count of the source file (samples below are already mono).
    pub source_channels: u16,
    pub bits_per_sample: u16,
    /// Byte length of the data chunk as declared in the file.
    pub data_bytes: usize,
    /// Mono samples. Multi-channel sources are averaged.
    pub samples: Vec<i16>,
    /// `LIST/INFO` tags keyed by four-character id (`ICMT`, `INAM`, ...).
    pub info: BTreeMap<String, String>,
}

impl Wav {
    pub fn duration_ms(&self) -> u64 {
        (self.samples.len() as u64 * 1000) / self.sample_rate as u64
    }

    /// Peak level in dBFS, or `None` for a silent file.
    pub fn peak_dbfs(&self) -> Option<f32> {
        let peak = self
            .samples
            .iter()
            .map(|s| (*s as i32).unsigned_abs())
            .max()
            .unwrap_or(0);
        if peak == 0 {
            None
        } else {
            Some(20.0 * (peak as f32 / 32768.0).log10())
        }
    }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Skip padding. RIFF pads odd chunks with one byte; sdrtrunk pads INFO tags
/// to four. A chunk id never begins with a NUL, so eating NULs is safe.
fn skip_padding(bytes: &[u8], mut pos: usize, end: usize) -> usize {
    while pos < end && bytes[pos] == 0 {
        pos += 1;
    }
    pos
}

/// Parse a complete WAV file held in memory.
pub fn parse(bytes: &[u8]) -> Result<Wav, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }
    let riff_end = (8usize + u32_at(bytes, 4) as usize).min(bytes.len());

    let mut fmt: Option<(u16, u16, u32, u16)> = None; // tag, channels, rate, bits
    let mut data: Option<(usize, usize)> = None; // offset, len
    let mut info = BTreeMap::new();

    let mut pos = 12usize;
    while pos + 8 <= riff_end {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(bytes, pos + 4) as usize;
        let body = pos + 8;
        if body + size > bytes.len() {
            return Err(WavError::Truncated {
                chunk: String::from_utf8_lossy(id).into_owned(),
                claimed: size,
                available: bytes.len().saturating_sub(body),
            });
        }
        match id {
            b"fmt " if size >= 16 => {
                fmt = Some((
                    u16_at(bytes, body),
                    u16_at(bytes, body + 2),
                    u32_at(bytes, body + 4),
                    u16_at(bytes, body + 14),
                ));
            }
            b"data" => data = Some((body, size)),
            b"LIST" if size >= 4 && &bytes[body..body + 4] == b"INFO" => {
                let end = body + size;
                let mut p = body + 4;
                loop {
                    p = skip_padding(bytes, p, end);
                    if p + 8 > end {
                        break;
                    }
                    let tid = &bytes[p..p + 4];
                    let tsize = u32_at(bytes, p + 4) as usize;
                    let tbody = p + 8;
                    if tbody + tsize > end {
                        break;
                    }
                    let raw = &bytes[tbody..tbody + tsize];
                    let text = String::from_utf8_lossy(raw)
                        .trim_end_matches('\0')
                        .to_string();
                    info.insert(String::from_utf8_lossy(tid).into_owned(), text);
                    p = tbody + tsize;
                }
            }
            _ => {}
        }
        pos = skip_padding(bytes, body + size, riff_end);
    }

    let (format_tag, channels, sample_rate, bits) = fmt.ok_or(WavError::NoFmt)?;
    let (doff, dlen) = data.ok_or(WavError::NoData)?;
    if format_tag != 1 || bits != 16 || channels == 0 {
        return Err(WavError::Unsupported { format_tag, bits });
    }

    let frame_bytes = 2 * channels as usize;
    let frames = dlen / frame_bytes;
    let mut samples = Vec::with_capacity(frames);
    let d = &bytes[doff..doff + frames * frame_bytes];
    if channels == 1 {
        samples.extend(d.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
    } else {
        for frame in d.chunks_exact(frame_bytes) {
            let sum: i32 = frame
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
                .sum();
            samples.push((sum / channels as i32) as i16);
        }
    }

    Ok(Wav {
        sample_rate,
        source_channels: channels,
        bits_per_sample: bits,
        data_bytes: dlen,
        samples,
        info,
    })
}

/// Serialise mono 16-bit PCM as a minimal RIFF/WAVE file (for the CLI, for
/// listening tests, and for the rtl_airband UDP input's hand-off).
#[cfg(test)]
pub fn write_pcm16(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    write_pcm16_info(samples, sample_rate, &[])
}

/// As [`write_pcm16`], with a `LIST/INFO` chunk after the data holding these
/// tags (four-character ids such as `ICMT`), as sdrtrunk writes them.
pub fn write_pcm16_info(samples: &[i16], sample_rate: u32, info: &[(&str, &str)]) -> Vec<u8> {
    let mut list = Vec::new();
    if !info.is_empty() {
        list.extend_from_slice(b"INFO");
        for (id, val) in info {
            assert_eq!(id.len(), 4, "INFO ids are four characters");
            let mut v = val.as_bytes().to_vec();
            v.push(0);
            list.extend_from_slice(id.as_bytes());
            list.extend_from_slice(&(v.len() as u32).to_le_bytes());
            list.extend_from_slice(&v);
            if v.len() % 2 == 1 {
                list.push(0);
            }
        }
    }
    let data_len = (samples.len() * 2) as u32;
    let list_len = if list.is_empty() { 0 } else { 8 + list.len() as u32 };
    let mut out = Vec::with_capacity(44 + data_len as usize + list_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len + list_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    if !list.is_empty() {
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&(list.len() as u32).to_le_bytes());
        out.extend_from_slice(&list);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_parse_round_trips() {
        let pcm: Vec<i16> = (0..800).map(|i| (i * 37 % 2000 - 1000) as i16).collect();
        let w = parse(&write_pcm16(&pcm, 8000)).unwrap();
        assert_eq!(w.sample_rate, 8000);
        assert_eq!(w.samples, pcm);
    }

    #[test]
    fn info_tags_round_trip() {
        let pcm: Vec<i16> = vec![1, -2, 3];
        let icmt = "Frequency:118100000;Start:1800000000123;End:1800000002123;Timing:rtl_airband-udp;";
        let bytes = write_pcm16_info(&pcm, 8000, &[("ICMT", icmt), ("INAM", "Tower")]);
        let w = parse(&bytes).unwrap();
        assert_eq!(w.samples, pcm);
        assert_eq!(w.info["ICMT"], icmt);
        assert_eq!(w.info["INAM"], "Tower");
        assert_eq!(w.data_bytes, 6);
    }

    fn le16(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }
    fn le32(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    /// Build a WAV the way sdrtrunk does: INFO tags padded to 4 bytes.
    fn synth(samples: &[i16], tags: &[(&str, &str)]) -> Vec<u8> {
        let mut list = Vec::new();
        list.extend_from_slice(b"INFO");
        for (id, val) in tags {
            let mut v = val.as_bytes().to_vec();
            v.push(0);
            list.extend_from_slice(id.as_bytes());
            list.extend_from_slice(&le32(v.len() as u32));
            list.extend_from_slice(&v);
            while list.len() % 4 != 0 {
                list.push(0);
            }
        }
        let mut data = Vec::new();
        for s in samples {
            data.extend_from_slice(&le16(*s as u16));
        }
        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&le32(16));
        body.extend_from_slice(&le16(1));
        body.extend_from_slice(&le16(1));
        body.extend_from_slice(&le32(8000));
        body.extend_from_slice(&le32(16000));
        body.extend_from_slice(&le16(2));
        body.extend_from_slice(&le16(16));
        body.extend_from_slice(b"data");
        body.extend_from_slice(&le32(data.len() as u32));
        body.extend_from_slice(&data);
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&le32(list.len() as u32));
        body.extend_from_slice(&list);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&le32(body.len() as u32));
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn parses_padded_info_tags() {
        let bytes = synth(
            &[0, 1000, -1000, 5],
            &[
                ("IPRD", "PH CEN 135.25 KALAMUNDA"),
                ("INAM", "1\"PH CEN 135.25 KALAMUNDA\""),
                ("ICMT", "Date:2026-09-06 21:03:47.735;Frequency:135250000;Start:1;End:2;"),
                ("ISFT", "sdrtrunk v0.6.2-beta-1"),
            ],
        );
        let w = parse(&bytes).unwrap();
        assert_eq!(w.sample_rate, 8000);
        assert_eq!(w.samples, vec![0, 1000, -1000, 5]);
        assert_eq!(w.info["IPRD"], "PH CEN 135.25 KALAMUNDA");
        assert_eq!(w.info["ISFT"], "sdrtrunk v0.6.2-beta-1");
        assert!(w.info["ICMT"].contains("Start:1;End:2;"));
        assert_eq!(w.duration_ms(), 0);
    }

    #[test]
    fn truncated_data_is_reported() {
        let mut bytes = synth(&[1; 4000], &[]);
        bytes.truncate(bytes.len() - 100);
        match parse(&bytes) {
            Err(WavError::Truncated { chunk, .. }) => assert_eq!(chunk, "data"),
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn peak_dbfs() {
        let bytes = synth(&[0, 16384, -100], &[]);
        let w = parse(&bytes).unwrap();
        let p = w.peak_dbfs().unwrap();
        assert!((p - -6.02).abs() < 0.05, "{p}");
    }
}
