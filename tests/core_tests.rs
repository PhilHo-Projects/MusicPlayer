use std::{path::PathBuf, time::Duration};

use music_player::{
    audio::{EQ_BANDS_HZ, EqSettings, clamp_seek_seconds},
    decoder::is_supported_extension,
    metadata::{TrackInfo, metadata_fallback_from_path},
    waveform::build_waveform,
};

#[test]
fn waveform_generation_returns_requested_normalized_bins() {
    let samples = vec![0.0, 0.0, 0.5, -0.5, 1.0, -1.0, 0.25, -0.25];

    let bins = build_waveform(&samples, 2, 4);

    assert_eq!(bins.len(), 4);
    assert_eq!(bins[0], 0.0);
    assert!((bins[1] - 0.5).abs() < f32::EPSILON);
    assert_eq!(bins[2], 1.0);
    assert!((bins[3] - 0.25).abs() < f32::EPSILON);
}

#[test]
fn seek_math_clamps_to_track_duration() {
    let duration = Duration::from_secs(180);

    assert_eq!(clamp_seek_seconds(-10.0, duration), 0.0);
    assert_eq!(clamp_seek_seconds(42.5, duration), 42.5);
    assert_eq!(clamp_seek_seconds(999.0, duration), 180.0);
}

#[test]
fn equalizer_settings_clamp_gain_and_reset_to_flat() {
    let mut settings = EqSettings::default();

    settings.set_gain(0, -99.0);
    settings.set_gain(4, 6.5);
    settings.set_gain(9, 99.0);
    settings.set_gain(100, 4.0);

    assert_eq!(
        EQ_BANDS_HZ,
        [
            31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0
        ]
    );
    assert_eq!(settings.gains_db()[0], -12.0);
    assert_eq!(settings.gains_db()[4], 6.5);
    assert_eq!(settings.gains_db()[9], 12.0);

    settings.reset();

    assert!(settings.gains_db().iter().all(|gain| *gain == 0.0));
}

#[test]
fn metadata_fallback_uses_file_stem_when_tags_are_missing() {
    let info = TrackInfo::fallback(PathBuf::from(
        r"C:\Users\Phil\Downloads\Reverse Skydiving.mp3",
    ));

    assert_eq!(info.title, "Reverse Skydiving");
    assert_eq!(info.artist.as_deref(), None);
    assert_eq!(info.album.as_deref(), None);
    assert!(info.cover_art.is_none());
}

#[test]
fn metadata_path_helper_matches_trackinfo_fallback() {
    let info = metadata_fallback_from_path(PathBuf::from("loose-download.wav"));

    assert_eq!(info.title, "loose-download");
    assert_eq!(info.path, PathBuf::from("loose-download.wav"));
}

#[test]
fn supported_extension_scope_is_limited_to_v1_audio_formats() {
    assert!(is_supported_extension("mp3"));
    assert!(is_supported_extension("WAV"));
    assert!(is_supported_extension("flac"));
    assert!(is_supported_extension("m4a"));
    assert!(is_supported_extension("aac"));
    assert!(!is_supported_extension("ogg"));
    assert!(!is_supported_extension("wem"));
}
