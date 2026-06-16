use std::{
    borrow::Cow,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use lofty::{
    file::{AudioFile, TaggedFileExt},
    prelude::Accessor,
    probe::Probe,
    tag::ItemKey,
};

#[derive(Clone, Debug, PartialEq)]
pub struct CoverArt {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackInfo {
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<String>,
    pub genre: Option<String>,
    pub duration: Option<Duration>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub bitrate: Option<u32>,
    /// Beats per minute, as a display string (e.g. `"126"`). Present on most
    /// store/DJ-tagged files via the ID3 `TBPM` frame.
    pub bpm: Option<String>,
    /// Musical key in whatever notation the tagger wrote (Traktor uses Open Key,
    /// e.g. `"4m"`), from the ID3 `TKEY` frame.
    pub key: Option<String>,
    /// True when the file carries Traktor's `PRIV:TRAKTOR4` analysis frame.
    pub traktor_analyzed: bool,
    pub cover_art: Option<CoverArt>,
}

impl TrackInfo {
    pub fn fallback(path: PathBuf) -> Self {
        let title = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.trim().is_empty())
            .unwrap_or("Unknown Track")
            .to_owned();

        Self {
            path,
            title,
            artist: None,
            album: None,
            year: None,
            genre: None,
            duration: None,
            sample_rate: None,
            channels: None,
            bitrate: None,
            bpm: None,
            key: None,
            traktor_analyzed: false,
            cover_art: None,
        }
    }
}

pub fn metadata_fallback_from_path(path: PathBuf) -> TrackInfo {
    TrackInfo::fallback(path)
}

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("Could not read metadata from {path}: {message}")]
    Read { path: PathBuf, message: String },
}

pub fn read_track_info(path: &Path) -> Result<TrackInfo, MetadataError> {
    let tagged_file = Probe::open(path)
        .map_err(|error| MetadataError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?
        .guess_file_type()
        .map_err(|error| MetadataError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?
        .read()
        .map_err(|error| MetadataError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

    let mut info = TrackInfo::fallback(path.to_path_buf());
    let properties = tagged_file.properties();
    info.duration = Some(properties.duration());
    info.sample_rate = properties.sample_rate();
    info.channels = properties.channels().map(u16::from);
    info.bitrate = properties
        .audio_bitrate()
        .or_else(|| properties.overall_bitrate());

    if let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    {
        if let Some(title) = owned_non_empty(tag.title()) {
            info.title = title;
        }
        info.artist = owned_non_empty(tag.artist());
        info.album = owned_non_empty(tag.album());
        info.genre = owned_non_empty(tag.genre());
        info.year = tag.date().map(|date| date.to_string());
        info.cover_art = tag
            .pictures()
            .first()
            .and_then(|picture| image::load_from_memory(picture.data()).ok())
            .map(|image| {
                let rgba = image.to_rgba8();
                let (width, height) = rgba.dimensions();
                CoverArt {
                    rgba: rgba.into_raw(),
                    width,
                    height,
                }
            });

        let bpm = tag
            .get_string(ItemKey::IntegerBpm)
            .or_else(|| tag.get_string(ItemKey::Bpm));
        info.bpm = owned_non_empty(bpm.map(Cow::Borrowed));
        info.key = owned_non_empty(tag.get_string(ItemKey::InitialKey).map(Cow::Borrowed));
    }

    info.traktor_analyzed = detect_traktor(path);

    let missing_audio_properties = (info.duration.is_none()
        || info.duration == Some(Duration::ZERO))
        || info.sample_rate.is_none()
        || info.channels.is_none();
    if missing_audio_properties && let Ok(decoded) = crate::decoder::decode_track(path) {
        info.duration = Some(decoded.duration);
        info.sample_rate = Some(decoded.sample_rate);
        info.channels = Some(decoded.channels as u16);
    }

    Ok(info)
}

fn owned_non_empty(value: Option<Cow<'_, str>>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Traktor writes its analysis (beatgrid, cue points, gain) into a `PRIV` ID3
/// frame whose owner identifier is `TRAKTOR4`. Presence of that signature is our
/// reliable "this was analyzed by Traktor" gate.
fn detect_traktor(path: &Path) -> bool {
    read_id3v2_tag(path).is_some_and(|bytes| contains(&bytes, b"TRAKTOR"))
}

/// Read just the leading ID3v2 tag (header + declared frame bytes), capped so a
/// pathological declared size can't try to allocate the whole file. Returns
/// `None` when there is no ID3v2 tag. Lets us sniff vendor signatures without a
/// full tag parse.
fn read_id3v2_tag(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).ok()?;
    if &header[0..3] != b"ID3" {
        return None;
    }
    // The tag size is a 28-bit synchsafe integer in the last 4 header bytes.
    let size = (((header[6] & 0x7f) as usize) << 21)
        | (((header[7] & 0x7f) as usize) << 14)
        | (((header[8] & 0x7f) as usize) << 7)
        | ((header[9] & 0x7f) as usize);
    let body_len = size.min(8 * 1024 * 1024);
    let mut body = Vec::new();
    file.take(body_len as u64).read_to_end(&mut body).ok()?;
    let mut bytes = Vec::with_capacity(10 + body.len());
    bytes.extend_from_slice(&header);
    bytes.append(&mut body);
    Some(bytes)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
