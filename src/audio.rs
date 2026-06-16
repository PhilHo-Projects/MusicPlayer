use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use biquad::{Biquad, Coefficients, DirectForm1, Q_BUTTERWORTH_F32, ToHertz, Type};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::{Fft, FixedSync, Resampler, audioadapter_buffers::direct::InterleavedSlice};

pub const EQ_BANDS_HZ: [f32; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqSettings {
    enabled: bool,
    gains_db: [f32; 10],
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            gains_db: [0.0; 10],
        }
    }
}

impl EqSettings {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn gains_db(&self) -> &[f32; 10] {
        &self.gains_db
    }

    pub fn gain_db(&self, index: usize) -> Option<f32> {
        self.gains_db.get(index).copied()
    }

    pub fn set_gain(&mut self, index: usize, gain_db: f32) {
        if let Some(gain) = self.gains_db.get_mut(index) {
            *gain = gain_db.clamp(-12.0, 12.0);
        }
    }

    pub fn reset(&mut self) {
        self.gains_db = [0.0; 10];
    }
}

pub fn clamp_seek_seconds(seconds: f32, duration: Duration) -> f32 {
    seconds.clamp(0.0, duration.as_secs_f32())
}

pub struct EqProcessor {
    sample_rate: u32,
    channels: usize,
    settings: EqSettings,
    filters: Vec<Vec<DirectForm1<f32>>>,
}

impl EqProcessor {
    pub fn new(sample_rate: u32, channels: usize, settings: EqSettings) -> Self {
        let mut processor = Self {
            sample_rate,
            channels,
            settings,
            filters: Vec::new(),
        };
        processor.rebuild_filters();
        processor
    }

    pub fn update_settings(&mut self, settings: EqSettings) {
        if self.settings != settings {
            self.settings = settings;
            self.rebuild_filters();
        }
    }

    pub fn process_frame(&mut self, frame: &mut [f32]) {
        if !self.settings.enabled() {
            return;
        }

        for (channel, sample) in frame.iter_mut().take(self.channels).enumerate() {
            if let Some(filters) = self.filters.get_mut(channel) {
                let mut processed = *sample;
                for filter in filters {
                    processed = filter.run(processed);
                }
                *sample = processed;
            }
        }
    }

    fn rebuild_filters(&mut self) {
        let safe_sample_rate = self.sample_rate.max(1) as f32;
        self.filters = (0..self.channels)
            .map(|_| {
                EQ_BANDS_HZ
                    .iter()
                    .zip(self.settings.gains_db().iter())
                    .filter_map(|(frequency, gain)| {
                        let frequency = frequency.clamp(1.0, safe_sample_rate * 0.45);
                        Coefficients::<f32>::from_params(
                            Type::PeakingEQ(*gain),
                            safe_sample_rate.hz(),
                            frequency.hz(),
                            Q_BUTTERWORTH_F32,
                        )
                        .ok()
                        .map(DirectForm1::<f32>::new)
                    })
                    .collect()
            })
            .collect();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AudioProcessError {
    #[error("Channel count must be greater than zero")]
    InvalidChannels,
    #[error("Audio buffer length is not divisible by channel count")]
    InvalidInterleavedLength,
    #[error("Could not resample audio: {0}")]
    Resample(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AudioEngineError {
    #[error("No default Windows output device is available")]
    NoOutputDevice,
    #[error("Audio output error: {0}")]
    Cpal(#[from] cpal::Error),
    #[error("Unsupported output sample format: {0:?}")]
    UnsupportedSampleFormat(cpal::SampleFormat),
}

#[derive(Clone, Debug)]
pub struct PlaybackBuffer {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: usize,
}

impl PlaybackBuffer {
    pub fn new(samples: Vec<f32>, sample_rate: u32, channels: usize) -> Self {
        Self {
            samples,
            sample_rate,
            channels,
        }
    }

    pub fn frame_count(&self) -> usize {
        self.samples.len().checked_div(self.channels).unwrap_or(0)
    }

    pub fn duration(&self) -> Duration {
        if self.sample_rate == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(self.frame_count() as f64 / self.sample_rate as f64)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Debug)]
pub struct PlaybackState {
    track: Option<Arc<PlaybackBuffer>>,
    cursor_frame: usize,
    transport: TransportState,
    volume: f32,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            track: None,
            cursor_frame: 0,
            transport: TransportState::Stopped,
            volume: 1.0,
        }
    }
}

impl PlaybackState {
    pub fn load(&mut self, track: Arc<PlaybackBuffer>) {
        self.track = Some(track);
        self.cursor_frame = 0;
        self.transport = TransportState::Stopped;
    }

    pub fn clear(&mut self) {
        self.track = None;
        self.cursor_frame = 0;
        self.transport = TransportState::Stopped;
    }

    pub fn play(&mut self) {
        if self.track.is_some() {
            self.transport = TransportState::Playing;
        }
    }

    pub fn pause(&mut self) {
        if self.transport == TransportState::Playing {
            self.transport = TransportState::Paused;
        }
    }

    pub fn toggle_playback(&mut self) {
        match self.transport {
            TransportState::Playing => self.pause(),
            TransportState::Paused | TransportState::Stopped => self.play(),
        }
    }

    pub fn seek_seconds(&mut self, seconds: f32) {
        if let Some(track) = &self.track {
            let clamped = clamp_seek_seconds(seconds, track.duration());
            self.cursor_frame = (clamped * track.sample_rate as f32).round() as usize;
            self.cursor_frame = self.cursor_frame.min(track.frame_count());
        }
    }

    pub fn skip_seconds(&mut self, delta: f32) {
        self.seek_seconds(self.position_seconds() + delta);
    }

    pub fn position_seconds(&self) -> f32 {
        self.track.as_ref().map_or(0.0, |track| {
            if track.sample_rate == 0 {
                0.0
            } else {
                self.cursor_frame as f32 / track.sample_rate as f32
            }
        })
    }

    pub fn duration(&self) -> Duration {
        self.track
            .as_ref()
            .map(|track| track.duration())
            .unwrap_or(Duration::ZERO)
    }

    pub fn transport(&self) -> TransportState {
        self.transport
    }

    /// The fader position (0..=1), as shown on the volume slider.
    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    /// Linear amplitude gain for the current fader position.
    ///
    /// Loudness perception is roughly logarithmic, so a linear fader feels like
    /// nothing happens until the very top. Map the 0..=1 fader through a ~60 dB
    /// exponential taper instead, so equal slider movement is roughly equal
    /// perceived loudness change. Fader 1.0 == unity (0 dB); 0.0 == silence.
    pub fn gain(&self) -> f32 {
        if self.volume <= 0.0 {
            0.0
        } else {
            10f32.powf((self.volume - 1.0) * 3.0)
        }
    }

    pub fn track(&self) -> Option<Arc<PlaybackBuffer>> {
        self.track.clone()
    }

    pub fn cursor_frame(&self) -> usize {
        self.cursor_frame
    }

    pub fn set_cursor_frame(&mut self, cursor_frame: usize) {
        let max_frame = self
            .track
            .as_ref()
            .map(|track| track.frame_count())
            .unwrap_or(0);
        self.cursor_frame = cursor_frame.min(max_frame);
    }
}

#[derive(Clone, Debug)]
pub struct PlaybackSnapshot {
    pub transport: TransportState,
    pub position_seconds: f32,
    pub duration: Duration,
    pub volume: f32,
    pub has_track: bool,
}

/// Peak/clip readings drained from the audio tap once per UI frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct VizLevels {
    /// Peak `|sample|` of the final mix since the last drain, measured *before*
    /// the output clamp — so a value `> 1.0` means the signal is clipping.
    pub peak: f32,
    /// True if any sample exceeded full scale since the last drain.
    pub clipped: bool,
}

/// Mono output samples retained for the live spectrum FFT. Matches the analyzer's
/// window so the UI can always read a full frame's worth. Power of two so the ring
/// wrap is a cheap mask.
const VIZ_RING: usize = 2048;

/// Real-time visualization tap. The audio callback writes the final (post-EQ,
/// post-gain) mono mix into a ring and tracks peak/clip; the UI drains it each
/// frame. Deliberately trivial so the realtime thread does almost no extra work.
struct VizTap {
    ring: Box<[f32; VIZ_RING]>,
    write: usize,
    peak: f32,
    clipped: bool,
}

impl VizTap {
    fn new() -> Self {
        Self {
            ring: Box::new([0.0; VIZ_RING]),
            write: 0,
            peak: 0.0,
            clipped: false,
        }
    }

    #[inline]
    fn push(&mut self, mono: f32, frame_peak: f32) {
        self.ring[self.write] = mono;
        self.write = (self.write + 1) % VIZ_RING;
        self.peak = self.peak.max(frame_peak);
        self.clipped |= frame_peak > 1.0;
    }

    /// Copy the ring into `out` (chronological, oldest-first) and reset peak/clip.
    fn drain(&mut self, out: &mut Vec<f32>) -> VizLevels {
        out.clear();
        out.extend_from_slice(&self.ring[self.write..]);
        out.extend_from_slice(&self.ring[..self.write]);
        let levels = VizLevels {
            peak: self.peak,
            clipped: self.clipped,
        };
        self.peak = 0.0;
        self.clipped = false;
        levels
    }
}

struct EngineShared {
    playback: PlaybackState,
    eq_settings: EqSettings,
    eq_processor: EqProcessor,
    output_channels: usize,
    viz: VizTap,
}

impl EngineShared {
    fn new(sample_rate: u32, output_channels: usize) -> Self {
        let eq_settings = EqSettings::default();
        Self {
            playback: PlaybackState::default(),
            eq_settings,
            eq_processor: EqProcessor::new(sample_rate, output_channels, eq_settings),
            output_channels,
            viz: VizTap::new(),
        }
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        PlaybackSnapshot {
            transport: self.playback.transport(),
            position_seconds: self.playback.position_seconds(),
            duration: self.playback.duration(),
            volume: self.playback.volume(),
            has_track: self.playback.track.is_some(),
        }
    }

    fn write_output(&mut self, output: &mut [f32]) {
        let Some(track) = self.playback.track.clone() else {
            output.fill(0.0);
            return;
        };

        if self.playback.transport != TransportState::Playing {
            output.fill(0.0);
            return;
        }

        let channels = self.output_channels;
        let gain = self.playback.gain();
        let mut frame = vec![0.0_f32; channels];

        for output_frame in output.chunks_mut(channels) {
            if self.playback.cursor_frame >= track.frame_count() {
                self.playback.transport = TransportState::Stopped;
                self.playback.cursor_frame = track.frame_count();
                output_frame.fill(0.0);
                continue;
            }

            let offset = self.playback.cursor_frame * channels;
            for (channel, sample) in frame.iter_mut().enumerate().take(channels) {
                *sample = track.samples[offset + channel] * gain;
            }

            self.eq_processor.update_settings(self.eq_settings);
            self.eq_processor.process_frame(&mut frame);

            // Visualization tap: mono mix plus the frame's peak, captured here so
            // it reflects the EQ/gain but *not* the clamp below — that's what lets
            // the clip meter see an over-boosted band push past full scale.
            let mut mono = 0.0_f32;
            let mut frame_peak = 0.0_f32;
            for &sample in frame.iter().take(channels) {
                mono += sample;
                frame_peak = frame_peak.max(sample.abs());
            }
            self.viz.push(mono / channels as f32, frame_peak);

            for (sample, processed) in output_frame.iter_mut().zip(frame.iter()) {
                *sample = processed.clamp(-1.0, 1.0);
            }

            self.playback.cursor_frame += 1;
        }
    }
}

pub struct AudioEngine {
    shared: Arc<Mutex<EngineShared>>,
    _stream: cpal::Stream,
    output_sample_rate: u32,
    output_channels: usize,
}

impl AudioEngine {
    pub fn new() -> Result<Self, AudioEngineError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioEngineError::NoOutputDevice)?;
        let supported_config = device.default_output_config()?;
        let sample_format = supported_config.sample_format();
        let stream_config: cpal::StreamConfig = supported_config.into();
        let output_sample_rate = stream_config.sample_rate;
        let output_channels = stream_config.channels as usize;
        let shared = Arc::new(Mutex::new(EngineShared::new(
            output_sample_rate,
            output_channels,
        )));

        let error_callback = |error| eprintln!("audio stream error: {error}");
        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let shared = Arc::clone(&shared);
                device.build_output_stream(
                    stream_config,
                    move |output: &mut [f32], _| fill_output_f32(output, &shared),
                    error_callback,
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                let shared = Arc::clone(&shared);
                device.build_output_stream(
                    stream_config,
                    move |output: &mut [i16], _| fill_output_i16(output, &shared),
                    error_callback,
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                let shared = Arc::clone(&shared);
                device.build_output_stream(
                    stream_config,
                    move |output: &mut [u16], _| fill_output_u16(output, &shared),
                    error_callback,
                    None,
                )?
            }
            other => return Err(AudioEngineError::UnsupportedSampleFormat(other)),
        };

        stream.play()?;

        Ok(Self {
            shared,
            _stream: stream,
            output_sample_rate,
            output_channels,
        })
    }

    pub fn output_sample_rate(&self) -> u32 {
        self.output_sample_rate
    }

    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    pub fn load(&self, track: PlaybackBuffer) {
        self.with_shared(|shared| shared.playback.load(Arc::new(track)));
    }

    pub fn toggle_playback(&self) {
        self.with_shared(|shared| shared.playback.toggle_playback());
    }

    pub fn play(&self) {
        self.with_shared(|shared| shared.playback.play());
    }

    pub fn pause(&self) {
        self.with_shared(|shared| shared.playback.pause());
    }

    pub fn seek_seconds(&self, seconds: f32) {
        self.with_shared(|shared| shared.playback.seek_seconds(seconds));
    }

    pub fn skip_seconds(&self, seconds: f32) {
        self.with_shared(|shared| shared.playback.skip_seconds(seconds));
    }

    pub fn set_volume(&self, volume: f32) {
        self.with_shared(|shared| shared.playback.set_volume(volume));
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.with_shared(|shared| shared.snapshot())
    }

    /// Drain the visualization tap into `out` (reused to avoid per-frame
    /// allocations); returns the recent mono window in `out` plus peak/clip.
    pub fn drain_viz(&self, out: &mut Vec<f32>) -> VizLevels {
        self.with_shared(|shared| shared.viz.drain(out))
    }

    pub fn eq_settings(&self) -> EqSettings {
        self.with_shared(|shared| shared.eq_settings)
    }

    pub fn set_eq_settings(&self, settings: EqSettings) {
        self.with_shared(|shared| {
            shared.eq_settings = settings;
            shared.eq_processor.update_settings(settings);
        });
    }

    fn with_shared<T>(&self, f: impl FnOnce(&mut EngineShared) -> T) -> T {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut shared)
    }
}

fn fill_output_f32(output: &mut [f32], shared: &Arc<Mutex<EngineShared>>) {
    let mut buffer = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    buffer.write_output(output);
}

fn fill_output_i16(output: &mut [i16], shared: &Arc<Mutex<EngineShared>>) {
    let mut temp = vec![0.0_f32; output.len()];
    fill_output_f32(&mut temp, shared);
    for (out, sample) in output.iter_mut().zip(temp.iter()) {
        *out = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
    }
}

fn fill_output_u16(output: &mut [u16], shared: &Arc<Mutex<EngineShared>>) {
    let mut temp = vec![0.0_f32; output.len()];
    fill_output_f32(&mut temp, shared);
    for (out, sample) in output.iter_mut().zip(temp.iter()) {
        let normalized = sample.clamp(-1.0, 1.0) * 0.5 + 0.5;
        *out = (normalized * u16::MAX as f32) as u16;
    }
}

pub fn remix_channels(samples: &[f32], input_channels: usize, output_channels: usize) -> Vec<f32> {
    if samples.is_empty() || input_channels == 0 || output_channels == 0 {
        return Vec::new();
    }

    if input_channels == output_channels {
        return samples.to_vec();
    }

    let frame_count = samples.len() / input_channels;
    let mut remixed = Vec::with_capacity(frame_count * output_channels);

    for frame in 0..frame_count {
        let input_offset = frame * input_channels;
        if output_channels == 1 {
            let sum: f32 = samples[input_offset..input_offset + input_channels]
                .iter()
                .copied()
                .sum();
            remixed.push(sum / input_channels as f32);
        } else if input_channels == 1 {
            remixed.extend(std::iter::repeat_n(samples[input_offset], output_channels));
        } else {
            for channel in 0..output_channels {
                let source_channel = channel.min(input_channels - 1);
                remixed.push(samples[input_offset + source_channel]);
            }
        }
    }

    remixed
}

pub fn resample_interleaved(
    samples: &[f32],
    channels: usize,
    input_rate: u32,
    output_rate: u32,
) -> Result<Vec<f32>, AudioProcessError> {
    if channels == 0 {
        return Err(AudioProcessError::InvalidChannels);
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(AudioProcessError::InvalidInterleavedLength);
    }
    if input_rate == output_rate {
        return Ok(samples.to_vec());
    }
    if samples.is_empty() {
        return Ok(Vec::new());
    }

    let input_frames = samples.len() / channels;
    // Resample directly in f32. Promoting the whole clip to f64 (and back) just
    // to feed the resampler doubled both the memory footprint and the work for
    // no audible benefit.
    let input_adapter = InterleavedSlice::new(samples, channels, input_frames)
        .map_err(|error| AudioProcessError::Resample(error.to_string()))?;

    let chunk_size = input_frames.clamp(16, 4096);
    let mut resampler = Fft::<f32>::new(
        input_rate as usize,
        output_rate as usize,
        chunk_size,
        2,
        channels,
        FixedSync::Both,
    )
    .map_err(|error| AudioProcessError::Resample(error.to_string()))?;

    let output_frames_needed = resampler.process_all_needed_output_len(input_frames);
    let mut output = vec![0.0_f32; output_frames_needed * channels];
    let mut output_adapter = InterleavedSlice::new_mut(&mut output, channels, output_frames_needed)
        .map_err(|error| AudioProcessError::Resample(error.to_string()))?;
    let (_, output_frames) = resampler
        .process_all_into_buffer(&input_adapter, &mut output_adapter, input_frames, None)
        .map_err(|error| AudioProcessError::Resample(error.to_string()))?;

    output.truncate(output_frames * channels);
    Ok(output)
}
