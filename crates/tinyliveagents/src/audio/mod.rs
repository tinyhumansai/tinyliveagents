//! PCM16 helpers: sample conversion, resampling, and durations.
//!
//! Every provider speaks 16-bit little-endian mono PCM, but not at the same
//! rate: Gemini answers at 24 kHz, ElevenLabs at whatever its agent is set to,
//! Sarvam at the rate requested. A host that plays audio at one rate, or a
//! provider adapter that must hand a chained stage a fixed rate, uses
//! [`resample_pcm16`]. The resampler is linear interpolation: cheap, allocation
//! bounded, and plenty for speech. Telephony-style agents (ElevenLabs with
//! `ulaw_8000`) speak G.711 μ-law, so [`ulaw_to_pcm16`] and [`pcm16_to_ulaw`]
//! convert it. Nothing here does I/O.

use bytes::Bytes;

use crate::error::{Error, Result};

/// The lowest sample rate [`resample_pcm16`] accepts.
pub const MIN_SAMPLE_RATE: u32 = 4_000;
/// The highest sample rate [`resample_pcm16`] accepts. Bounding both ends caps
/// the output at 48× the input, so a bogus rate cannot trigger a huge
/// allocation.
pub const MAX_SAMPLE_RATE: u32 = 192_000;

/// Decodes PCM16 little-endian bytes into samples. A trailing odd byte is
/// ignored.
#[must_use]
pub fn pcm16_to_samples(bytes: &[u8]) -> Vec<i16> {
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs.iter().map(|pair| i16::from_le_bytes(*pair)).collect()
}

/// Encodes samples as PCM16 little-endian bytes.
#[must_use]
pub fn samples_to_pcm16(samples: &[i16]) -> Bytes {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    Bytes::from(out)
}

/// Resamples PCM16 mono audio from `from_rate` to `to_rate` Hz.
///
/// Returns the input unchanged when the rates match.
///
/// # Errors
///
/// [`Error::InvalidConfig`] when either rate is outside
/// [`MIN_SAMPLE_RATE`]`..=`[`MAX_SAMPLE_RATE`].
pub fn resample_pcm16(bytes: &Bytes, from_rate: u32, to_rate: u32) -> Result<Bytes> {
    if from_rate == to_rate {
        return Ok(bytes.clone());
    }
    for rate in [from_rate, to_rate] {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&rate) {
            return Err(Error::InvalidConfig(format!(
                "sample rate {rate} is outside {MIN_SAMPLE_RATE}..={MAX_SAMPLE_RATE}"
            )));
        }
    }
    let input = pcm16_to_samples(bytes);
    if input.is_empty() {
        return Ok(Bytes::new());
    }
    let ratio = f64::from(from_rate) / f64::from(to_rate);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let out_len = ((input.len() as f64) / ratio).round().max(1.0) as usize;
    let last = input.len() - 1;
    let mut output = Vec::with_capacity(out_len);
    for index in 0..out_len {
        #[allow(clippy::cast_precision_loss)]
        let position = index as f64 * ratio;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let left = (position.floor() as usize).min(last);
        let right = (left + 1).min(last);
        #[allow(clippy::cast_precision_loss)]
        let fraction = position - left as f64;
        let value =
            f64::from(input[left]) + (f64::from(input[right]) - f64::from(input[left])) * fraction;
        #[allow(clippy::cast_possible_truncation)]
        output.push(
            value
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16,
        );
    }
    Ok(samples_to_pcm16(&output))
}

/// Decodes G.711 μ-law bytes into PCM16 little-endian bytes (one sample per
/// byte, same rate).
#[must_use]
pub fn ulaw_to_pcm16(ulaw: &[u8]) -> Bytes {
    let samples: Vec<i16> = ulaw.iter().map(|byte| ulaw_decode(*byte)).collect();
    samples_to_pcm16(&samples)
}

/// Encodes PCM16 little-endian bytes as G.711 μ-law (same rate).
#[must_use]
pub fn pcm16_to_ulaw(pcm16: &[u8]) -> Bytes {
    pcm16_to_samples(pcm16)
        .into_iter()
        .map(ulaw_encode)
        .collect::<Vec<u8>>()
        .into()
}

const ULAW_BIAS: i32 = 0x84;
const ULAW_CLIP: i32 = 32_635;

fn ulaw_encode(sample: i16) -> u8 {
    let mut value = i32::from(sample);
    let sign = if value < 0 {
        value = -value;
        0x80
    } else {
        0
    };
    value = value.min(ULAW_CLIP) + ULAW_BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && value & mask == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (value >> (exponent + 3)) & 0x0F;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let encoded = !(sign | (exponent << 4) | mantissa) as u8;
    encoded
}

fn ulaw_decode(byte: u8) -> i16 {
    let byte = i32::from(!byte);
    let sign = byte & 0x80;
    let exponent = (byte >> 4) & 0x07;
    let mantissa = byte & 0x0F;
    let magnitude = (((mantissa << 3) + ULAW_BIAS) << exponent) - ULAW_BIAS;
    #[allow(clippy::cast_possible_truncation)]
    let sample = if sign == 0 { magnitude } else { -magnitude } as i16;
    sample
}

/// Milliseconds of audio in `byte_len` bytes of PCM16 mono at `sample_rate`.
#[must_use]
pub fn duration_ms(byte_len: usize, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    (byte_len as u64 / 2) * 1000 / u64::from(sample_rate)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
