//! Unit tests for the PCM16 helpers.

use super::*;

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
fn equal_rates_and_zero_rates_pass_through() {
    let bytes = samples_to_pcm16(&[1, 2, 3]);
    assert_eq!(resample_pcm16(&bytes, 16_000, 16_000), bytes);
    assert_eq!(resample_pcm16(&bytes, 0, 16_000), bytes);
    assert_eq!(resample_pcm16(&bytes, 16_000, 0), bytes);
}

#[test]
fn empty_input_resamples_to_empty() {
    assert!(resample_pcm16(&Bytes::new(), 24_000, 16_000).is_empty());
}

#[test]
fn downsampling_scales_length_and_keeps_shape() {
    let ramp: Vec<i16> = (0..2400).map(|i| i16::try_from(i).unwrap()).collect();
    let out = pcm16_to_samples(&resample_pcm16(&samples_to_pcm16(&ramp), 24_000, 16_000));
    assert_eq!(out.len(), 1600);
    assert_eq!(out[0], 0);
    // A linear ramp stays linear: sample i maps to 1.5 * i.
    assert_eq!(out[100], 150);
    assert!(out.windows(2).all(|w| w[1] >= w[0]));
}

#[test]
fn upsampling_interpolates_between_samples() {
    let out = pcm16_to_samples(&resample_pcm16(&samples_to_pcm16(&[0, 100]), 8_000, 16_000));
    assert_eq!(out, vec![0, 50, 100, 100]);
}

#[test]
fn durations_are_computed_from_byte_length() {
    assert_eq!(duration_ms(32_000, 16_000), 1000);
    assert_eq!(duration_ms(3_200, 16_000), 100);
    assert_eq!(duration_ms(100, 0), 0);
}
