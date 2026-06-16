use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use symphonia::core::{
    codecs::audio::{AudioDecoderOptions, CODEC_ID_NULL_AUDIO},
    errors::Error as SymphoniaError,
    formats::probe::Hint,
    formats::{FormatOptions, TrackType},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

use crate::waveform::{WaveformAnalysis, analyze_waveform};

#[derive(Clone, Debug)]
pub struct DecodedTrack {
    pub path: PathBuf,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: usize,
    pub duration: Duration,
    pub waveform: WaveformAnalysis,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("Unsupported audio format: {path}")]
    UnsupportedExtension { path: PathBuf },
    #[error("No decodable audio track found in {path}")]
    NoAudioTrack { path: PathBuf },
    #[error("Audio file has no decoded samples: {path}")]
    EmptyAudio { path: PathBuf },
    #[error("Could not open audio file {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Could not decode audio file {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: SymphoniaError,
    },
}

pub fn is_supported_extension(extension: &str) -> bool {
    matches!(
        extension
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str(),
        "mp3" | "wav" | "flac" | "m4a" | "aac"
    )
}

pub fn decode_track(path: &Path) -> Result<DecodedTrack, DecodeError> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    if !is_supported_extension(extension) {
        return Err(DecodeError::UnsupportedExtension {
            path: path.to_path_buf(),
        });
    }

    let file = File::open(path).map_err(|source| DecodeError::Open {
        path: path.to_path_buf(),
        source,
    })?;

    let mut hint = Hint::new();
    hint.with_extension(extension);

    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|source| DecodeError::Decode {
            path: path.to_path_buf(),
            source,
        })?;

    let track =
        format
            .default_track(TrackType::Audio)
            .ok_or_else(|| DecodeError::NoAudioTrack {
                path: path.to_path_buf(),
            })?;

    let track_id = track.id;
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .filter(|params| params.codec != CODEC_ID_NULL_AUDIO)
        .cloned()
        .ok_or_else(|| DecodeError::NoAudioTrack {
            path: path.to_path_buf(),
        })?;
    let sample_rate = codec_params.sample_rate.unwrap_or(44_100);
    let channels = codec_params
        .channels
        .as_ref()
        .map(|channels| channels.count())
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .map_err(|source| DecodeError::Decode {
            path: path.to_path_buf(),
            source,
        })?;

    let mut samples = Vec::new();
    // `copy_to_vec_interleaved` resizes the destination to the *current* packet's
    // length and overwrites it, so it must target a scratch buffer that we then
    // append to the full track — otherwise only the final packet survives.
    let mut packet_samples = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(source) => {
                return Err(DecodeError::Decode {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                decoded.copy_to_vec_interleaved::<f32>(&mut packet_samples);
                samples.extend_from_slice(&packet_samples);
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(source) => {
                return Err(DecodeError::Decode {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }

    if samples.is_empty() {
        return Err(DecodeError::EmptyAudio {
            path: path.to_path_buf(),
        });
    }

    let frame_count = samples.len() / channels;
    let duration = Duration::from_secs_f64(frame_count as f64 / sample_rate as f64);
    // Higher than the on-screen pixel width so the renderer can down-sample to
    // the widget size cleanly instead of stretching a coarse buffer. Bakes peak,
    // RMS, and low/mid/high band energies in one pass.
    let waveform = analyze_waveform(&samples, channels, sample_rate, 2000);

    Ok(DecodedTrack {
        path: path.to_path_buf(),
        samples,
        sample_rate,
        channels,
        duration,
        waveform,
    })
}
