//! Ogg Opus encoding of one utterance.
//!
//! Mono, VoIP application, VBR, 20 ms frames, packets packed ~50 per page.
//! The layout of OpusHead/OpusTags follows RFC 7845. The input sample rate
//! (8 or 16 kHz from rtl_airband) is encoded natively.

use audiopus::coder::Encoder;
use audiopus::{Application, Bitrate, Channels, SampleRate, Signal};
use ogg::writing::{PacketWriteEndInfo, PacketWriter};

#[derive(Debug, thiserror::Error)]
pub enum OpusError {
    #[error("unsupported sample rate {0} Hz (Opus accepts 8000, 12000, 16000, 24000, 48000)")]
    SampleRate(u32),
    #[error("opus encoder: {0}")]
    Encoder(String),
    #[error("ogg writer: {0}")]
    Ogg(String),
}

#[derive(Debug, Clone)]
pub struct EncodeParams {
    pub bitrate_bps: i32,
    pub complexity: u8,
}

impl Default for EncodeParams {
    fn default() -> Self {
        Self {
            bitrate_bps: 12_000,
            complexity: 10,
        }
    }
}

fn sample_rate(hz: u32) -> Result<SampleRate, OpusError> {
    Ok(match hz {
        8000 => SampleRate::Hz8000,
        12000 => SampleRate::Hz12000,
        16000 => SampleRate::Hz16000,
        24000 => SampleRate::Hz24000,
        48000 => SampleRate::Hz48000,
        other => return Err(OpusError::SampleRate(other)),
    })
}

/// Encode mono PCM to a complete Ogg Opus file in memory.
///
/// `tags` become OpusTags user comments (`KEY=value`), so the file is
/// self-describing if separated from its manifest.
pub fn encode_ogg_opus(
    samples: &[i16],
    sample_rate_hz: u32,
    params: &EncodeParams,
    tags: &[(String, String)],
) -> Result<Vec<u8>, OpusError> {
    let rate = sample_rate(sample_rate_hz)?;
    let mut enc = Encoder::new(rate, Channels::Mono, Application::Voip)
        .map_err(|e| OpusError::Encoder(e.to_string()))?;
    enc.set_bitrate(Bitrate::BitsPerSecond(params.bitrate_bps))
        .map_err(|e| OpusError::Encoder(e.to_string()))?;
    enc.set_vbr(true).map_err(|e| OpusError::Encoder(e.to_string()))?;
    enc.set_complexity(params.complexity)
        .map_err(|e| OpusError::Encoder(e.to_string()))?;
    enc.set_signal(Signal::Voice)
        .map_err(|e| OpusError::Encoder(e.to_string()))?;
    let lookahead = enc.lookahead().map_err(|e| OpusError::Encoder(e.to_string()))? as u32;

    // Everything in the Ogg layer is expressed at 48 kHz.
    let to48 = 48_000 / sample_rate_hz;
    let pre_skip: u16 = (lookahead * to48).min(u16::MAX as u32) as u16;
    let frame = (sample_rate_hz / 50) as usize; // 20 ms
    let frame48: u64 = 960;

    let mut writer = PacketWriter::new(Vec::<u8>::new());
    let serial: u32 = 0x5235_4f50; // "R5OP"

    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(1); // mono
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&sample_rate_hz.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    writer
        .write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
        .map_err(|e| OpusError::Ogg(e.to_string()))?;

    let vendor = b"read5-audio-uploader";
    let mut tagp = Vec::new();
    tagp.extend_from_slice(b"OpusTags");
    tagp.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tagp.extend_from_slice(vendor);
    tagp.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for (k, v) in tags {
        let s = format!("{k}={v}");
        tagp.extend_from_slice(&(s.len() as u32).to_le_bytes());
        tagp.extend_from_slice(s.as_bytes());
    }
    writer
        .write_packet(tagp, serial, PacketWriteEndInfo::EndPage, 0)
        .map_err(|e| OpusError::Ogg(e.to_string()))?;

    // The codec delays audio by `lookahead` samples, so to reproduce every
    // input sample the stream must run `lookahead` samples past the end, and
    // the final granule position is (input + lookahead) in 48 kHz units,
    // i.e. input*to48 + pre_skip (RFC 7845 §4.4).
    let total = samples.len() + lookahead as usize;
    let n_frames = total.div_ceil(frame).max(1);
    let mut buf = vec![0u8; 4000];
    let mut pcm = vec![0i16; frame];
    let mut granule: u64 = 0;
    for i in 0..n_frames {
        let start = i * frame;
        let end = (start + frame).min(samples.len());
        pcm.fill(0);
        if start < samples.len() {
            pcm[..end - start].copy_from_slice(&samples[start..end]);
        }
        let n = enc
            .encode(&pcm, &mut buf)
            .map_err(|e| OpusError::Encoder(e.to_string()))?;
        let last = i + 1 == n_frames;
        granule += frame48;
        let gp = if last {
            // Trim the zero padding on the final frame (RFC 7845 §4.4).
            (total as u64) * to48 as u64
        } else {
            granule
        };
        let info = if last {
            PacketWriteEndInfo::EndStream
        } else if (i + 1) % 50 == 0 {
            PacketWriteEndInfo::EndPage
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        writer
            .write_packet(buf[..n].to_vec(), serial, info, gp)
            .map_err(|e| OpusError::Ogg(e.to_string()))?;
    }

    Ok(writer.into_inner())
}

#[cfg(test)]
/// A decoded Ogg Opus file: what [`encode_ogg_opus`] was given, less the
/// codec's loss.
#[derive(Debug, Clone)]
pub struct DecodedOpus {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    /// OpusTags user comments, in file order.
    pub tags: Vec<(String, String)>,
}

#[cfg(test)]
/// Decode a mono Ogg Opus file written by [`encode_ogg_opus`] back to PCM at
/// its original input rate, with pre-skip and end trim applied so the sample
/// count matches the source (RFC 7845 §4.4).
pub fn decode_ogg_opus(bytes: &[u8]) -> Result<DecodedOpus, OpusError> {
    use audiopus::coder::Decoder;
    use ogg::reading::PacketReader;

    let ogg_err = |e: ogg::OggReadError| OpusError::Ogg(e.to_string());
    let mut rd = PacketReader::new(std::io::Cursor::new(bytes));
    let head = rd.read_packet_expected().map_err(ogg_err)?;
    if head.data.len() < 19 || !head.data.starts_with(b"OpusHead") {
        return Err(OpusError::Ogg("missing OpusHead".into()));
    }
    if head.data[9] != 1 {
        return Err(OpusError::Ogg(format!("{} channels; expected mono", head.data[9])));
    }
    let pre_skip48 = u16::from_le_bytes([head.data[10], head.data[11]]) as u64;
    let sample_rate_hz = u32::from_le_bytes([head.data[12], head.data[13], head.data[14], head.data[15]]);
    let rate = sample_rate(sample_rate_hz)?;
    let to48 = (48_000 / sample_rate_hz) as u64;

    let tagp = rd.read_packet_expected().map_err(ogg_err)?;
    if !tagp.data.starts_with(b"OpusTags") {
        return Err(OpusError::Ogg("missing OpusTags".into()));
    }
    let tags = parse_tags(&tagp.data[8..]);

    let mut dec = Decoder::new(rate, Channels::Mono).map_err(|e| OpusError::Encoder(e.to_string()))?;
    let mut frame = vec![0i16; 5760];
    let mut samples: Vec<i16> = Vec::new();
    let mut last_granule = 0u64;
    while let Some(p) = rd.read_packet().map_err(ogg_err)? {
        let pkt = audiopus::packet::Packet::try_from(&p.data).map_err(|e| OpusError::Encoder(e.to_string()))?;
        let sig = audiopus::MutSignals::try_from(&mut frame).map_err(|e| OpusError::Encoder(e.to_string()))?;
        let n = dec
            .decode(Some(pkt), sig, false)
            .map_err(|e| OpusError::Encoder(e.to_string()))?;
        samples.extend_from_slice(&frame[..n]);
        last_granule = p.absgp_page();
    }
    let skip = ((pre_skip48 / to48) as usize).min(samples.len());
    samples.drain(..skip);
    let keep = (last_granule.saturating_sub(pre_skip48) / to48) as usize;
    samples.truncate(keep);
    Ok(DecodedOpus {
        samples,
        sample_rate: sample_rate_hz,
        tags,
    })
}

#[cfg(test)]
/// OpusTags body after the magic: vendor string, then `KEY=value` comments.
/// Malformed input yields what parsed cleanly.
fn parse_tags(mut b: &[u8]) -> Vec<(String, String)> {
    fn take<'a>(b: &mut &'a [u8]) -> Option<&'a [u8]> {
        let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
        let s = b.get(4..4 + n)?;
        *b = &b[4 + n..];
        Some(s)
    }
    let mut out = Vec::new();
    if take(&mut b).is_none() {
        return out;
    }
    let Some(count) = b.get(..4).map(|c| u32::from_le_bytes(c.try_into().unwrap())) else {
        return out;
    };
    b = &b[4..];
    for _ in 0..count {
        let Some(s) = take(&mut b) else { break };
        let s = String::from_utf8_lossy(s);
        if let Some((k, v)) = s.split_once('=') {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_restores_length_tags_and_tone() {
        let rate = 8000u32;
        let n_in = rate as usize * 2 + 123;
        let samples: Vec<i16> = (0..n_in)
            .map(|i| ((2.0 * std::f32::consts::PI * 700.0 * i as f32 / rate as f32).sin() * 12000.0) as i16)
            .collect();
        let tags = vec![("START_MS".to_string(), "42".to_string()), ("SOURCE".to_string(), "a=b.wav".to_string())];
        let bytes = encode_ogg_opus(&samples, rate, &EncodeParams::default(), &tags).unwrap();
        let d = decode_ogg_opus(&bytes).unwrap();
        assert_eq!(d.sample_rate, rate);
        assert_eq!(d.samples.len(), n_in);
        assert_eq!(d.tags, tags);
        // Aligned with the input: strong correlation in the steady middle.
        let (a, b) = (&samples[4000..12000], &d.samples[4000..12000]);
        let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
        let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        assert!(dot / (na * nb) > 0.9, "correlation {}", dot / (na * nb));
    }

    #[test]
    fn encodes_a_tone_to_ogg_opus() {
        let rate = 8000u32;
        let samples: Vec<i16> = (0..rate as usize * 2 + 37)
            .map(|i| ((i as f32 * 0.3).sin() * 8000.0) as i16)
            .collect();
        let tags = vec![("START_MS".to_string(), "1".to_string())];
        let bytes = encode_ogg_opus(&samples, rate, &EncodeParams::default(), &tags).unwrap();
        assert_eq!(&bytes[0..4], b"OggS");
        assert!(bytes.windows(8).any(|w| w == b"OpusHead"));
        assert!(bytes.windows(8).any(|w| w == b"OpusTags"));
        assert!(bytes.windows(10).any(|w| w == b"START_MS=1"));
        // ~12 kbps for 2 s should be a few KB, well under the raw 32 KB.
        assert!(bytes.len() < 8000, "{}", bytes.len());
        assert!(bytes.len() > 1000, "{}", bytes.len());
    }

    /// Decode what we wrote with libopus via the `ogg` reader and check the
    /// sample count survives (minus pre-skip, plus end trim).
    #[test]
    fn roundtrip_decodes_to_same_length() {
        use audiopus::coder::Decoder;
        use ogg::reading::PacketReader;
        use std::io::Cursor;

        let rate = 8000u32;
        let n_in = rate as usize * 3 + 123; // not a multiple of the 20 ms frame
        let samples: Vec<i16> = (0..n_in)
            .map(|i| ((i as f32 * 0.05).sin() * 12000.0) as i16)
            .collect();
        let bytes = encode_ogg_opus(&samples, rate, &EncodeParams::default(), &[]).unwrap();

        let mut rd = PacketReader::new(Cursor::new(bytes));
        let head = rd.read_packet_expected().unwrap();
        assert!(head.data.starts_with(b"OpusHead"));
        let pre_skip48 = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;
        let tags = rd.read_packet_expected().unwrap();
        assert!(tags.data.starts_with(b"OpusTags"));

        let mut dec = Decoder::new(SampleRate::Hz8000, Channels::Mono).unwrap();
        let mut out = vec![0i16; 5760];
        let mut decoded = 0usize;
        let mut last_granule = 0u64;
        while let Some(p) = rd.read_packet().unwrap() {
            let pkt = audiopus::packet::Packet::try_from(&p.data).unwrap();
            let sig = audiopus::MutSignals::try_from(&mut out).unwrap();
            let n = dec.decode(Some(pkt), sig, false).unwrap();
            decoded += n;
            last_granule = p.absgp_page();
        }
        // Final granule (48 kHz units) should equal input length + pre-skip.
        assert_eq!(last_granule as usize, n_in * 6 + pre_skip48);
        // Decoder output covers every frame including the padded tail.
        let frames = (n_in + pre_skip48 / 6).div_ceil(160);
        assert_eq!(decoded, frames * 160);
    }

    #[test]
    fn rejects_odd_rates() {
        assert!(matches!(
            encode_ogg_opus(&[0; 100], 11025, &EncodeParams::default(), &[]),
            Err(OpusError::SampleRate(11025))
        ));
    }
}
