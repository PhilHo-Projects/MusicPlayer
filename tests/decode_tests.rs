use std::{fs, path::Path};

use music_player::{
    decoder::{DecodeError, decode_track},
    metadata::read_track_info,
};

#[test]
fn wav_decode_returns_samples_properties_and_waveform() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("untagged test.wav");
    write_test_wav(&path);

    let decoded = decode_track(&path).expect("test WAV should decode");

    assert_eq!(decoded.sample_rate, 44_100);
    assert_eq!(decoded.channels, 2);
    assert_eq!(decoded.samples.len(), 8);
    assert!(decoded.duration.as_secs_f32() > 0.0);
    assert_eq!(decoded.waveform.len(), 2000);
}

#[test]
fn wav_decode_accumulates_all_packets() {
    // A file large enough to span many decoder packets. Regression guard for a
    // bug where the decode loop kept only the final packet (so playback was a
    // tiny fragment). `copy_to_vec_interleaved` overwrites its destination, so
    // every packet must be appended to the full track.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("long.wav");
    let frames = 50_000;
    write_ramp_wav(&path, frames);

    let decoded = decode_track(&path).expect("multi-packet WAV should decode");

    assert_eq!(decoded.samples.len(), frames * 2);
    assert!((decoded.duration.as_secs_f32() - frames as f32 / 44_100.0).abs() < 0.01);
}

#[test]
fn unsupported_file_extension_returns_visible_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("engine.wem");
    fs::write(&path, b"not audio").unwrap();

    let error = decode_track(&path).expect_err("wem should be outside v1 scope");

    assert!(matches!(error, DecodeError::UnsupportedExtension { .. }));
    assert!(error.to_string().contains("Unsupported audio format"));
}

#[test]
fn untagged_metadata_uses_filename_and_audio_properties() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fresh download.wav");
    write_test_wav(&path);

    let info = read_track_info(&path).expect("metadata fallback should succeed");

    assert_eq!(info.title, "fresh download");
    assert_eq!(info.sample_rate, Some(44_100));
    assert_eq!(info.channels, Some(2));
    assert!(info.duration.unwrap().as_secs_f32() > 0.0);
    assert!(info.cover_art.is_none());
}

fn write_test_wav(path: &Path) {
    let mut bytes = Vec::new();
    let samples: [i16; 8] = [0, 0, 12_000, -12_000, 24_000, -24_000, -6000, 6000];
    let data_len = (samples.len() * 2) as u32;
    let file_len = 36 + data_len;

    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&file_len.to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&44_100_u32.to_le_bytes());
    bytes.extend_from_slice(&(44_100_u32 * 2 * 16 / 8).to_le_bytes());
    bytes.extend_from_slice(&(2_u16 * 16 / 8).to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }

    fs::write(path, bytes).unwrap();
}

fn write_ramp_wav(path: &Path, frames: usize) {
    let channels = 2_u16;
    let bytes_per_sample = 2_u32;
    let block_align = channels as u32 * bytes_per_sample;
    let data_len = frames as u32 * block_align;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);

    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&44_100_u32.to_le_bytes());
    bytes.extend_from_slice(&(44_100_u32 * block_align).to_le_bytes());
    bytes.extend_from_slice(&(block_align as u16).to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let sample = (frame % 2000) as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
        bytes.extend_from_slice(&sample.to_le_bytes());
    }

    fs::write(path, bytes).unwrap();
}
