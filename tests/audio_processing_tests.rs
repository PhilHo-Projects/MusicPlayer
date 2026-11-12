use std::sync::Arc;
use std::time::Duration;

use music_player::audio::{
    EqProcessor, EqSettings, PlaybackBuffer, PlaybackState, TransportState, remix_channels,
    resample_interleaved,
};

#[test]
fn remix_channels_averages_stereo_to_mono() {
    let stereo = vec![1.0, -1.0, 0.5, 0.25];

    let mono = remix_channels(stereo, 2, 1);

    assert_eq!(mono, vec![0.0, 0.375]);
}

#[test]
fn remix_channels_duplicates_mono_to_stereo() {
    let mono = vec![0.25, -0.5];

    let stereo = remix_channels(mono, 1, 2);

    assert_eq!(stereo, vec![0.25, 0.25, -0.5, -0.5]);
}

#[test]
fn stereo_on_a_surround_device_plays_only_through_the_front_pair() {
    assert_eq!(
        remix_channels(vec![0.25, -0.5], 2, 6),
        vec![0.25, -0.5, 0.0, 0.0, 0.0, 0.0]
    );
}

#[test]
fn resample_interleaved_preserves_audio_when_rates_match() {
    let samples = vec![0.0, 0.25, -0.25, 0.5];

    let resampled = resample_interleaved(samples.clone(), 2, 44_100, 44_100).unwrap();

    assert_eq!(resampled, samples);
}

#[test]
fn resample_interleaved_changes_frame_count_when_rate_changes() {
    let samples = vec![0.0_f32; 44_100 * 2];

    let resampled = resample_interleaved(samples, 2, 44_100, 48_000).unwrap();

    let frames = resampled.len() / 2;
    assert!((47_900..=48_100).contains(&frames));
}

#[test]
fn resampling_short_clips_preserves_the_opening_impulse() {
    for frames in [32, 100, 440, 442, 882] {
        let mut input = vec![0.0; frames];
        input[0] = 1.0;
        let output = resample_interleaved(input, 1, 44_100, 48_000).unwrap();
        let (position, peak) = output
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            .unwrap();
        assert_eq!(position, 0, "opening impulse shifted for {frames} frames");
        assert!(*peak > 0.5, "opening impulse faded for {frames} frames");
    }
}

#[test]
fn resample_interleaved_reuses_allocation_when_rates_match() {
    // A device already running at the file's rate must hand the buffer straight
    // back. Copying instead costs a full duplicate of the track — >2 GB on a
    // two-hour set, which is what pushed long sets into swap.
    let samples = vec![0.0, 0.25, -0.25, 0.5];
    let original = samples.as_ptr();

    let resampled = resample_interleaved(samples, 2, 44_100, 44_100).unwrap();

    assert_eq!(resampled.as_ptr(), original);
}

#[test]
fn remix_channels_reuses_allocation_when_layout_matches() {
    let stereo = vec![1.0, -1.0, 0.5, 0.25];
    let original = stereo.as_ptr();

    let remixed = remix_channels(stereo, 2, 2);

    assert_eq!(remixed.as_ptr(), original);
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
fn play_restarts_at_eof_and_preserves_mid_track_position() {
    let mut state = PlaybackState::default();
    state.load(Arc::new(PlaybackBuffer::new(vec![0.0; 100], 100, 1)));
    state.seek_seconds(1.0);
    state.play();
    assert_eq!(state.transport(), TransportState::Playing);
    assert_eq!(state.cursor_frame(), 0);

    state.seek_seconds(0.4);
    state.pause();
    state.play();
    assert_eq!(state.cursor_frame(), 40);
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
