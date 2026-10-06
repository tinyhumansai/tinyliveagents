//! Unit tests for the PCM16 helpers.

use super::*;
use crate::error::Error;

#[test]
fn samples_round_trip_through_bytes() {
    let samples = [0_i16, 1, -1, i16::MAX, i16::MIN];
    let bytes = samples_to_pcm16(&samples);
    assert_eq!(bytes.len(), 10);
    assert_eq!(pcm16_to_samples(&bytes), samples);
}

#[test]
fn a_trailing_odd_byte_is_ignored() {
    assert_eq!(pcm16_to_samples(&[1, 0, 9]), vec![1]);
}

#[test]
fn equal_rates_pass_through_and_extreme_rates_are_refused() {
    let bytes = samples_to_pcm16(&[1, 2, 3]);
    assert_eq!(resample_pcm16(&bytes, 16_000, 16_000).unwrap(), bytes);
    for (from, to) in [(0, 16_000), (16_000, 0), (1, u32::MAX), (16_000, 192_001)] {
        assert!(matches!(
            resample_pcm16(&bytes, from, to),
            Err(Error::InvalidConfig(_))
        ));
    }
}

#[test]
fn empty_input_resamples_to_empty() {
    assert!(
        resample_pcm16(&Bytes::new(), 24_000, 16_000)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ulaw_round_trips_within_quantization_error() {
    let samples = [
        0_i16,
        100,
        -100,
        1_000,
        -1_000,
        12_345,
        -12_345,
        i16::MAX,
        i16::MIN,
    ];
    let ulaw = pcm16_to_ulaw(&samples_to_pcm16(&samples));
    assert_eq!(ulaw.len(), samples.len());
    let back = pcm16_to_samples(&ulaw_to_pcm16(&ulaw));
    for (original, decoded) in samples.iter().zip(back) {
        let error = (i32::from(*original) - i32::from(decoded)).abs();
        // μ-law keeps a few percent of relative precision; quiet samples
        // stay within a few steps.
        assert!(
            error <= (i32::from(*original).abs() / 16).max(8),
            "{original} -> {decoded}"
        );
    }
    // Known G.711 code points.
    assert_eq!(pcm16_to_ulaw(&samples_to_pcm16(&[0]))[0], 0xFF);
    assert_eq!(pcm16_to_samples(&ulaw_to_pcm16(&[0xFF])), vec![0]);
    assert_eq!(pcm16_to_samples(&ulaw_to_pcm16(&[0x7F])), vec![0]);
}

#[test]
fn downsampling_scales_length_and_keeps_shape() {
    let ramp: Vec<i16> = (0..2400).map(|i| i16::try_from(i).unwrap()).collect();
    let out = pcm16_to_samples(&resample_pcm16(&samples_to_pcm16(&ramp), 24_000, 16_000).unwrap());
    assert_eq!(out.len(), 1600);
    assert_eq!(out[0], 0);
    // A linear ramp stays linear: sample i maps to 1.5 * i.
    assert_eq!(out[100], 150);
    assert!(out.windows(2).all(|w| w[1] >= w[0]));
}

#[test]
fn upsampling_interpolates_between_samples() {
    let out =
        pcm16_to_samples(&resample_pcm16(&samples_to_pcm16(&[0, 100]), 8_000, 16_000).unwrap());
    assert_eq!(out, vec![0, 50, 100, 100]);
}

#[test]
fn durations_are_computed_from_byte_length() {
    assert_eq!(duration_ms(32_000, 16_000), 1000);
    assert_eq!(duration_ms(3_200, 16_000), 100);
    assert_eq!(duration_ms(100, 0), 0);
}
