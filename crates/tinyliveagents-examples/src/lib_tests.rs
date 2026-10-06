//! Tests for the WAV helpers.

use super::*;

fn fmt_chunk(format: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let mut out = b"fmt ".to_vec();
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&format.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out
}

fn wav(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"RIFF\0\0\0\0WAVE".to_vec();
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    out
}

fn data_chunk(declared: u32, body: &[u8]) -> Vec<u8> {
    let mut out = b"data".to_vec();
    out.extend_from_slice(&declared.to_le_bytes());
    out.extend_from_slice(body);
    out
}

#[test]
fn round_trips_through_wav_bytes() {
    let bytes = wav_bytes(16_000, &[1, 2, 3, 4]).unwrap();
    assert_eq!(parse_wav(&bytes).unwrap(), (16_000, vec![1, 2, 3, 4]));
    assert!(wav_bytes(u32::MAX, &[]).is_err());
}

#[test]
fn rejects_malformed_files_without_panicking() {
    assert!(parse_wav(b"nope").is_err());
    assert!(parse_wav(&wav(&[])).is_err());
    // A fmt chunk declaring fewer than 16 bytes.
    let mut short = b"fmt ".to_vec();
    short.extend_from_slice(&0_u32.to_le_bytes());
    assert!(parse_wav(&wav(&[short])).is_err());
    // A fmt chunk cut off mid-body.
    let mut cut = fmt_chunk(1, 1, 16_000, 16);
    cut.truncate(12);
    assert!(parse_wav(&wav(&[cut])).is_err());
    // A chunk header cut off.
    assert!(parse_wav(&wav(&[b"fmt".to_vec()])).is_err());
    // Float, stereo, 8-bit.
    for (format, channels, bits) in [(3, 1, 16), (1, 2, 16), (1, 1, 8)] {
        let file = wav(&[
            fmt_chunk(format, channels, 16_000, bits),
            data_chunk(2, &[0, 0]),
        ]);
        assert!(parse_wav(&file).is_err());
    }
    // Data before fmt, and data shorter than declared.
    assert!(parse_wav(&wav(&[data_chunk(2, &[0, 0])])).is_err());
    let truncated = wav(&[fmt_chunk(1, 1, 16_000, 16), data_chunk(100, &[0; 10])]);
    assert!(parse_wav(&truncated).is_err());
    // A chunk size that overflows when advancing.
    let mut huge = b"LIST".to_vec();
    huge.extend_from_slice(&(u32::MAX - 1).to_le_bytes());
    assert!(parse_wav(&wav(&[fmt_chunk(1, 1, 16_000, 16), huge])).is_err());
}

#[test]
fn streaming_data_sizes_run_to_the_end() {
    for declared in [0, u32::MAX] {
        let file = wav(&[fmt_chunk(1, 1, 8_000, 16), data_chunk(declared, &[5, 6])]);
        assert_eq!(parse_wav(&file).unwrap(), (8_000, vec![5, 6]));
    }
}
