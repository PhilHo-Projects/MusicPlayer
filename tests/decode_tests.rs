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

#[test]
fn mp3_with_oversized_id3_tag_decodes_past_false_frame_syncs() {
    // Regression guard for symphonia's `id3v2` feature (see Cargo.toml). A
    // Traktor-tagged MP3 with embedded art carries a large leading ID3v2 tag; the
    // JPEG inside is full of 0xFF bytes that look like MPEG frame syncs. Without
    // `id3v2`, symphonia's probe never recognizes/skips the tag and locks the MP3
    // demuxer onto a false sync *inside* it — often a Layer I/II frame whose
    // decoder we don't register — so decode dies with "unsupported audio codec".
    //
    // This fixture reproduces that exactly: an ID3v2.3 tag whose body opens with a
    // false Layer I sync pair, followed by real (silent) Layer III audio. Decoding
    // must reach the Layer III audio, proving the tag was skipped.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("traktor tagged.mp3");
    write_mp3_with_false_sync_in_id3(&path);

    let decoded = decode_track(&path).expect("tagged MP3 should decode past the ID3 tag");

    assert_eq!(decoded.sample_rate, 44_100);
    assert_eq!(decoded.channels, 2);
    // 30 Layer III frames * 1152 samples/frame * 2 channels.
    assert_eq!(decoded.samples.len(), 30 * 1152 * 2);
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

/// Builds an MP3 that mimics a Traktor file with embedded art: a large ID3v2.3
/// tag whose body opens with a *false* MPEG frame-sync pair, followed by real
/// decodable Layer III audio. Used to prove the decoder skips the tag rather than
/// syncing to junk inside it.
fn write_mp3_with_false_sync_in_id3(path: &Path) {
    let mut bytes = id3v2_tag_with_false_sync(1024);
    for _ in 0..30 {
        bytes.extend_from_slice(&silent_l3_frame());
    }
    fs::write(path, bytes).unwrap();
}

/// A valid, decodable MPEG-1 Layer III frame of digital silence: 44.1 kHz,
/// 128 kbps, stereo. All-zero side info and main data decode to 1152 silent
/// samples per channel. `FF FB 90 00` is the frame header; size is
/// 144 * 128000 / 44100 = 417 bytes.
fn silent_l3_frame() -> Vec<u8> {
    let mut frame = vec![0u8; 417];
    frame[0] = 0xFF;
    frame[1] = 0xFB;
    frame[2] = 0x90;
    frame[3] = 0x00;
    frame
}

/// An ID3v2.3 tag of `body_len` bytes whose body opens with two consecutive
/// `FF FF 40 00` headers — a valid MPEG-1 Layer I sync pair (frame size 136), the
/// kind of false sync found inside embedded JPEG art. The demuxer's two-frame
/// check will latch onto this if the tag isn't skipped first.
fn id3v2_tag_with_false_sync(body_len: usize) -> Vec<u8> {
    let mut body = vec![0u8; body_len];
    for &start in &[0usize, 136] {
        body[start] = 0xFF;
        body[start + 1] = 0xFF;
        body[start + 2] = 0x40;
        body[start + 3] = 0x00;
    }

    let mut tag = Vec::new();
    tag.extend_from_slice(b"ID3");
    tag.extend_from_slice(&[0x03, 0x00]); // version 2.3.0
    tag.push(0x00); // flags
    // Tag size is synchsafe: 7 usable bits per byte.
    let size = body_len as u32;
    tag.push(((size >> 21) & 0x7F) as u8);
    tag.push(((size >> 14) & 0x7F) as u8);
    tag.push(((size >> 7) & 0x7F) as u8);
    tag.push((size & 0x7F) as u8);
    tag.extend_from_slice(&body);
    tag
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
