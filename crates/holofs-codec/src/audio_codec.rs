//! Stage 9: codec for audio files.
//!
//! **Decode**: via the `symphonia` crate we accept any format — WAV, MP3, FLAC,
//! OGG/Vorbis, AAC, M4A, AIFF. Any sample rate and bit depth is converted to
//! `f32` per-sample, mono or stereo (>2 channels are averaged down to stereo).
//!
//! **Storage encoding**: each channel is a 1D array of `f32` samples. We push
//! it through **1D Haar DWT** (see [`holofs_core::transform`]) to obtain
//! priority layers:
//! - L0 (LL) — lowest frequencies, ≈ bass and overall envelope
//! - L1, L2 — mid frequencies
//! - L3 (HH) — highest frequencies, fine detail
//!
//! Each layer is encoded via RLNC with its own redundancy from `RED` — exactly
//! as for images. Node-loss degradation yields **muffled audio** instead of
//! "all or nothing": the upper layers drop first → details and high
//! frequencies disappear, while the bass remains recognisable.
//!
//! **Output**: always RIFF/WAV 16-bit PCM (a universal container). The encoder
//! is hand-rolled — symphonia is not used on the encode path.

use std::io::Cursor;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};

/// Decoded audio in RAM. Channels (1 or 2) are held as `Vec<f32>` samples
/// in roughly the range [-1, 1]; sample_rate in Hz.
pub struct DecodedAudio {
    pub channels: Vec<Vec<f32>>,
    pub sample_rate: u32,
    /// Total number of samples per channel (identical across channels).
    pub sample_count: u32,
}

impl DecodedAudio {
    pub fn n_channels(&self) -> u8 {
        self.channels.len() as u8
    }
}

/// Decode audio from in-memory bytes. Any format supported by `symphonia`
/// (see features in `Cargo.toml`).
pub fn decode_audio_from_bytes(bytes: &[u8]) -> Result<DecodedAudio, String> {
    let cursor = Box::new(Cursor::new(bytes.to_vec()));
    let mss = MediaSourceStream::new(cursor, Default::default());

    let probed = get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("audio format not recognised: {e}"))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no audio track in file".to_string())?
        .clone();
    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| "no sample_rate in metadata".to_string())?;
    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("decoder: {e}"))?;

    // Channel buffer: up to 2 channels. Extras are pairwise-averaged into stereo.
    let mut left: Vec<f32> = Vec::new();
    let mut right: Vec<f32> = Vec::new();
    let mut output_channels: usize = 1; // updated after the first packet

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                return Err("decoder requires reset (multi-stream streams not supported)".into())
            }
            Err(e) => return Err(format!("read packet: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|e| format!("decode: {e}"))?;
        let spec = *decoded.spec();
        let chans = spec.channels.count();
        output_channels = chans.min(2).max(1);
        append_samples(decoded, &mut left, &mut right, chans);
    }

    let channels = if output_channels >= 2 {
        vec![left, right]
    } else {
        vec![left]
    };
    let sample_count = channels[0].len() as u32;
    Ok(DecodedAudio {
        channels,
        sample_rate,
        sample_count,
    })
}

/// Append a frame buffer into `left`/`right`. Mixes N>2 channels down to
/// stereo by averaging odd channels into L and even channels into R; mono is
/// simply expanded into L (R stays empty).
fn append_samples(
    buf: AudioBufferRef<'_>,
    left: &mut Vec<f32>,
    right: &mut Vec<f32>,
    chans: usize,
) {
    macro_rules! convert {
        ($buf:expr) => {{
            let n = $buf.frames();
            if chans == 1 {
                let plane = $buf.chan(0);
                left.extend(plane.iter().take(n).map(|s| to_f32(*s)));
            } else if chans == 2 {
                let l = $buf.chan(0);
                let r = $buf.chan(1);
                left.extend(l.iter().take(n).map(|s| to_f32(*s)));
                right.extend(r.iter().take(n).map(|s| to_f32(*s)));
            } else {
                // 5.1/7.1 → coarse stereo downmix:
                // L = mean(channel 0, 2, 4, …), R = mean(channel 1, 3, 5, …).
                let mut lhi = Vec::with_capacity(n);
                let mut rhi = Vec::with_capacity(n);
                for f in 0..n {
                    let mut lacc = 0f32;
                    let mut racc = 0f32;
                    let (mut lc, mut rc) = (0u32, 0u32);
                    for ch in 0..chans {
                        let s = to_f32($buf.chan(ch)[f]);
                        if ch % 2 == 0 {
                            lacc += s;
                            lc += 1;
                        } else {
                            racc += s;
                            rc += 1;
                        }
                    }
                    lhi.push(lacc / lc.max(1) as f32);
                    rhi.push(racc / rc.max(1) as f32);
                }
                left.extend(lhi);
                right.extend(rhi);
            }
        }};
    }
    match buf {
        AudioBufferRef::F32(b) => convert!(b),
        AudioBufferRef::F64(b) => convert!(b),
        AudioBufferRef::S8(b) => convert!(b),
        AudioBufferRef::S16(b) => convert!(b),
        AudioBufferRef::S24(b) => convert!(b),
        AudioBufferRef::S32(b) => convert!(b),
        AudioBufferRef::U8(b) => convert!(b),
        AudioBufferRef::U16(b) => convert!(b),
        AudioBufferRef::U24(b) => convert!(b),
        AudioBufferRef::U32(b) => convert!(b),
    }
}

/// Convert any symphonia sample to `f32 ∈ [-1, 1]`.
trait ToF32 {
    fn to_f32_norm(self) -> f32;
}
impl ToF32 for f32 {
    fn to_f32_norm(self) -> f32 {
        self
    }
}
impl ToF32 for f64 {
    fn to_f32_norm(self) -> f32 {
        self as f32
    }
}
impl ToF32 for i8 {
    fn to_f32_norm(self) -> f32 {
        self as f32 / i8::MAX as f32
    }
}
impl ToF32 for i16 {
    fn to_f32_norm(self) -> f32 {
        self as f32 / i16::MAX as f32
    }
}
impl ToF32 for i32 {
    fn to_f32_norm(self) -> f32 {
        self as f32 / i32::MAX as f32
    }
}
impl ToF32 for u8 {
    fn to_f32_norm(self) -> f32 {
        (self as f32 - 128.0) / 128.0
    }
}
impl ToF32 for u16 {
    fn to_f32_norm(self) -> f32 {
        (self as f32 - 32768.0) / 32768.0
    }
}
impl ToF32 for u32 {
    fn to_f32_norm(self) -> f32 {
        (self as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32
    }
}
impl ToF32 for symphonia::core::sample::i24 {
    fn to_f32_norm(self) -> f32 {
        self.0 as f32 / ((1 << 23) as f32)
    }
}
impl ToF32 for symphonia::core::sample::u24 {
    fn to_f32_norm(self) -> f32 {
        (self.0 as f32 - (1 << 23) as f32) / ((1 << 23) as f32)
    }
}

fn to_f32<S: ToF32>(s: S) -> f32 {
    s.to_f32_norm()
}

// === WAV encoder for output ================================================

/// Encode `f32 ∈ [-1, 1]` channels as RIFF/WAV 16-bit PCM.
/// A universal container, readable everywhere.
pub fn encode_wav_16bit(channels: &[Vec<f32>], sample_rate: u32) -> Vec<u8> {
    let n_chans = channels.len().max(1) as u16;
    let frames = channels.first().map(|c| c.len()).unwrap_or(0);
    let bytes_per_sample = 2u16;
    let block_align = n_chans * bytes_per_sample;
    let byte_rate = sample_rate * block_align as u32;
    let data_size = (frames * block_align as usize) as u32;

    let mut out = Vec::with_capacity(44 + data_size as usize);
    // RIFF header.
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_size).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    // fmt subchunk.
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM = 16-byte subchunk
    out.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
    out.extend_from_slice(&n_chans.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&(bytes_per_sample * 8).to_le_bytes());
    // data subchunk.
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for f in 0..frames {
        for ch in 0..n_chans as usize {
            let s = channels[ch][f].clamp(-1.0, 1.0);
            let i = (s * i16::MAX as f32) as i16;
            out.extend_from_slice(&i.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal WAV round-trip: build a 16-bit WAV via encode_wav_16bit, then
    /// decode it back through symphonia and check that amplitudes match.
    #[test]
    fn wav_roundtrip_mono_sine() {
        let sr = 8000;
        let n = 1024;
        let chan: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let bytes = encode_wav_16bit(&[chan.clone()], sr);
        // header (44) + 16-bit mono = 44 + 1024*2
        assert_eq!(bytes.len(), 44 + n * 2);
        let dec = decode_audio_from_bytes(&bytes).expect("decode wav");
        assert_eq!(dec.sample_rate, sr);
        assert_eq!(dec.n_channels(), 1);
        assert_eq!(dec.sample_count, n as u32);
        // RMS difference < 1% (16-bit quantisation is acceptable).
        let err: f32 = chan
            .iter()
            .zip(dec.channels[0].iter())
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            / n as f32;
        assert!(err.sqrt() < 0.001, "RMS error {err}");
    }

    #[test]
    fn wav_stereo_roundtrip() {
        let sr = 8000;
        let n = 512;
        let l: Vec<f32> = (0..n).map(|i| (i as f32 * 0.03).sin() * 0.4).collect();
        let r: Vec<f32> = (0..n).map(|i| (i as f32 * 0.07).cos() * 0.3).collect();
        let bytes = encode_wav_16bit(&[l.clone(), r.clone()], sr);
        let dec = decode_audio_from_bytes(&bytes).expect("decode wav");
        assert_eq!(dec.n_channels(), 2);
        assert_eq!(dec.sample_count, n as u32);
        let drift_l: f32 = l
            .iter()
            .zip(dec.channels[0].iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        let drift_r: f32 = r
            .iter()
            .zip(dec.channels[1].iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(drift_l / (n as f32) < 0.001);
        assert!(drift_r / (n as f32) < 0.001);
    }

    #[test]
    fn invalid_bytes_return_error() {
        let res = decode_audio_from_bytes(b"not audio at all");
        assert!(res.is_err());
    }

    #[test]
    fn empty_bytes_return_error() {
        assert!(decode_audio_from_bytes(&[]).is_err());
    }

    #[test]
    fn encode_clamps_out_of_range_samples() {
        // Values outside [-1, 1] must be clamped before 16-bit quantisation
        // — otherwise `(s * i16::MAX) as i16` wraps and we get inverted
        // garbage at the peaks. Round-trip a deliberately oversize input
        // and assert the decoded peak is at the i16 saturation point.
        let sr = 8000;
        let oversize: Vec<f32> = vec![5.0; 32];
        let bytes = encode_wav_16bit(&[oversize], sr);
        let dec = decode_audio_from_bytes(&bytes).expect("decode wav");
        let peak = dec.channels[0].iter().cloned().fold(f32::MIN, f32::max);
        // After clamp+quantise, the peak rounds to ≥ 0.999 (one quant
        // step below 1.0 due to 16-bit truncation).
        assert!(peak >= 0.999, "expected saturated peak ≈ 1.0, got {peak}");
    }

    #[test]
    fn encode_with_empty_channel_list_produces_valid_header() {
        // 0 frames → header-only WAV (44 bytes). Decoding it back should
        // still parse (some symphonia versions may reject; the assertion
        // is on the encoder side, not the decoder).
        let bytes = encode_wav_16bit(&[], 8000);
        assert_eq!(bytes.len(), 44, "header-only WAV expected, got {} bytes", bytes.len());
        assert!(bytes.starts_with(b"RIFF"));
        assert_eq!(&bytes[8..12], b"WAVE");
    }

    #[test]
    fn n_channels_matches_channel_vec_len_under_u8() {
        let dec = DecodedAudio {
            sample_rate: 8000,
            sample_count: 1,
            channels: vec![vec![0.0]; 5],
        };
        assert_eq!(dec.n_channels(), 5);
    }
}
