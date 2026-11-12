use std::{
    borrow::Cow,
    io::Cursor,
    path::{Path, PathBuf},
    time::Duration,
};

use image::ImageReader;
use lofty::{
    file::{AudioFile, TaggedFileExt},
    id3::v2::{Frame, Id3v2Tag},
    prelude::Accessor,
    probe::Probe,
    tag::{ItemKey, TagType},
};

const MAX_COVER_INPUT_SIDE: u32 = 8192;
const MAX_COVER_TEXTURE_SIDE: u32 = 1024;

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
    let mut info = read_track_tags(path)?;
    let missing_audio_properties = info.duration.is_none_or(|duration| duration.is_zero())
        || info.sample_rate.is_none()
        || info.channels.is_none();
    if missing_audio_properties && let Ok(decoded) = crate::decoder::decode_track_unanalyzed(path) {
        info.duration = Some(decoded.duration);
        info.sample_rate = Some(decoded.sample_rate);
        info.channels = Some(decoded.channels as u16);
    }
    Ok(info)
}

/// The player already has the properties from decoding, so it can skip the
/// decode fallback used by callers of `read_track_info`.
pub(crate) fn read_track_tags(path: &Path) -> Result<TrackInfo, MetadataError> {
    let mut tagged_file = Probe::open(path)
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
            .and_then(|picture| decode_cover(picture.data()));

        let bpm = tag
            .get_string(ItemKey::IntegerBpm)
            .or_else(|| tag.get_string(ItemKey::Bpm));
        info.bpm = owned_non_empty(bpm.map(Cow::Borrowed));
        info.key = owned_non_empty(tag.get_string(ItemKey::InitialKey).map(Cow::Borrowed));
    }

    // Lofty retains private frames in the generic ID3v2 tag's companion data.
    info.traktor_analyzed = tagged_file.remove(TagType::Id3v2).is_some_and(|tag| {
        Id3v2Tag::from(tag)
            .into_iter()
            .any(|frame| matches!(frame, Frame::Private(frame) if frame.owner == "TRAKTOR4"))
    });

    Ok(info)
}

fn owned_non_empty(value: Option<Cow<'_, str>>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn decode_cover(data: &[u8]) -> Option<CoverArt> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_COVER_INPUT_SIDE);
    limits.max_image_height = Some(MAX_COVER_INPUT_SIDE);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let image = if image.width().max(image.height()) > MAX_COVER_TEXTURE_SIDE {
        image.thumbnail(MAX_COVER_TEXTURE_SIDE, MAX_COVER_TEXTURE_SIDE)
    } else {
        image
    };
    let rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    Some(CoverArt {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}
