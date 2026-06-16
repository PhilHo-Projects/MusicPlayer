use std::sync::Arc;
use std::time::Duration;

use music_player::audio::{
    EqProcessor, EqSettings, PlaybackBuffer, PlaybackState, TransportState, remix_channels,
    resample_interleaved,
};

#[test]
fn remix_channels_averages_stereo_to_mono() {
    let stereo = vec![1.0, -1.0, 0.5, 0.25];

    let mono = remix_channels(&stereo, 2, 1);

    assert_eq!(mono, vec![0.0, 0.375]);
}

#[test]
fn remix_channels_duplicates_mono_to_stereo() {
    let mono = vec![0.25, -0.5];

    let stereo = remix_channels(&mono, 1, 2);

    assert_eq!(stereo, vec![0.25, 0.25, -0.5, -0.5]);
}

#[test]
fn resample_interleaved_preserves_audio_when_rates_match() {
    let samples = vec![0.0, 0.25, -0.25, 0.5];

    let resampled = resample_interleaved(&samples, 2, 44_100, 44_100).unwrap();

    assert_eq!(resampled, samples);
}

#[test]
fn resample_interleaved_changes_frame_count_when_rate_changes() {
    let samples = vec![0.0_f32; 44_100 * 2];

    let resampled = resample_interleaved(&samples, 2, 44_100, 48_000).unwrap();

    let frames = resampled.len() / 2;
    assert!((47_900..=48_100).contains(&frames));
}

#[test]
fn playback_state_replaces_track_and_clamps_seek() {
    let first = Arc::new(PlaybackBuffer::new(vec![0.0; 44_100 * 2], 44_100, 2));
    let second = Arc::new(PlaybackBuffer::new(vec![0.0; 48_000 * 2], 48_000, 2));
    let mut state = PlaybackState::default();

    state.load(first);
    state.play();
    state.seek_seconds(999.0);

    assert_eq!(state.transport(), TransportState::Playing);
    assert_eq!(state.position_seconds(), 1.0);

    state.load(second);

    assert_eq!(state.transport(), TransportState::Stopped);
    assert_eq!(state.position_seconds(), 0.0);
    assert_eq!(state.duration(), Duration::from_secs(1));
}

#[test]
fn eq_processor_bypasses_when_disabled() {
    let mut settings = EqSettings::default();
    settings.set_enabled(false);
    settings.set_gain(5, 12.0);
    let mut processor = EqProcessor::new(44_100, 2, settings);
    let mut frame = [0.25_f32, -0.5_f32];

    processor.process_frame(&mut frame);

    assert_eq!(frame, [0.25, -0.5]);
}
