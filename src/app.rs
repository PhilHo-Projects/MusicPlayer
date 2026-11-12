use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Button, Color32, ColorImage, ComboBox, Context, Layout, Mesh, Pos2, Rect, RichText,
    Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use symphonia::core::audio::Channels;

use crate::{
    audio::{
        AudioEngine, EQ_BANDS_HZ, EqSettings, PlaybackBuffer, PlaybackSnapshot, PlaybackState,
        TransportState, remix_channels, remix_track_channels, resample_interleaved,
    },
    decoder::{DecodeError, DecodedTrack, WAVEFORM_BINS, decode_track_cancelable},
    metadata::{CoverArt, TrackInfo, read_track_tags},
    spectrum::{SpectrumAnalyzer, SpectrumDisplayMode, SpectrumParams},
    waveform::{
        ColorMode, ReductionMode, WaveformAnalysis, WaveformParams, analyze_waveform_cancelable,
    },
};

const OUTPUT_RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub struct MusicPlayerApp {
    audio: Result<AudioEngine, String>,
    suspended_playback: PlaybackState,
    retry_audio_at: Instant,
    pending_recovery: Option<Pending<Result<RecoveredOutput, String>>>,
    track: Option<LoadedTrack>,
    cover_texture: Option<TextureHandle>,
    status: String,
    eq_settings: EqSettings,
    waveform_params: WaveformParams,
    waveform_columns: Option<WaveformColumns>,
    track_worker: Option<Sender<TrackJob>>,
    pending_load: Option<Pending<LoadOutcome>>,
    /// In-flight background waveform analysis for the current track.
    pending_analysis: Option<Pending<(u64, WaveformAnalysis)>>,
    analysis_gate: AnalysisGate,
    /// File-open requests forwarded from secondary launches (double-click in
    /// Explorer). `None` when single-instance IPC isn't wired up.
    file_rx: Option<Receiver<PathBuf>>,
    /// Live spectrum strip (`None` when no audio device is available).
    spectrum: Option<SpectrumAnalyzer>,
    spectrum_params: SpectrumParams,
    clip_meter: ClipMeter,
    /// Reused scratch for draining the audio tap each frame.
    viz_samples: Vec<f32>,
}

/// Stamps each load with a generation so a background waveform result from a
/// superseded load can be discarded instead of painting over the current track.
/// Generation 0 is never issued, so a default gate accepts nothing.
#[derive(Default)]
pub struct AnalysisGate {
    current: u64,
}

impl AnalysisGate {
    pub fn begin(&mut self) -> u64 {
        self.current += 1;
        self.current
    }

    pub fn accepts(&self, generation: u64) -> bool {
        generation != 0 && generation == self.current
    }
}

struct LoadedTrack {
    info: TrackInfo,
    decoded: DecodedTrack,
}

struct WaveformColumns {
    params: WaveformParams,
    bars: Vec<WaveformColumn>,
}

struct WaveformColumn {
    height: f32,
    played: Color32,
    unplayed: Color32,
}

/// Output peak/clip meter state. `level`/`cap` are 0..=1 bar heights; `clip_latch`
/// keeps the red clip block lit for a moment after a true full-scale overshoot so
/// brief clips aren't missed.
#[derive(Default)]
struct ClipMeter {
    level: f32,
    cap: f32,
    cap_vel: f32,
    clip_latch: f32,
}

impl ClipMeter {
    fn update(&mut self, peak: f32, clipped: bool, dt: f32) {
        // Map the pre-clamp peak to a height over a -48..0 dBFS window. Instant
        // attack so transients show; eased release so it reads as a meter.
        let db = 20.0 * peak.max(1e-6).log10();
        let target = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
        self.level = if target >= self.level {
            target
        } else {
            (self.level - 1.4 * dt).max(target)
        };

        if self.level >= self.cap {
            self.cap = self.level;
            self.cap_vel = 0.0;
        } else {
            self.cap_vel += 0.9 * dt;
            self.cap = (self.cap - self.cap_vel * dt).max(self.level);
        }

        if clipped || peak >= 1.0 {
            self.clip_latch = 0.9;
        } else {
            self.clip_latch = (self.clip_latch - dt).max(0.0);
        }
    }

    fn decay(&mut self, dt: f32) {
        self.update(0.0, false, dt);
    }

    fn is_animating(&self) -> bool {
        self.level > 0.0 || self.cap > 0.0 || self.clip_latch > 0.0
    }
}

/// Result of decoding/resampling a file on a background thread.
enum LoadOutcome {
    // Boxed: the payload dwarfs `Failed(String)`, and every `LoadOutcome` (sent
    // over the load-thread mpsc channel) would otherwise be sized to it.
    Loaded(Box<LoadedData>),
    Failed(String),
}

struct LoadedData {
    info: TrackInfo,
    decoded: DecodedTrack,
    buffer: PlaybackBuffer,
}

struct Pending<T> {
    result: Receiver<T>,
    canceled: Arc<AtomicBool>,
}

impl<T> Pending<T> {
    fn channel() -> (Sender<T>, Self) {
        let (sender, result) = mpsc::channel();
        (
            sender,
            Self {
                result,
                canceled: Arc::default(),
            },
        )
    }
}

impl<T> Drop for Pending<T> {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::Relaxed);
    }
}

struct RecoveredOutput {
    engine: AudioEngine,
    buffer: Option<PlaybackBuffer>,
}

type TrackJob = Box<dyn FnOnce() + Send>;

fn start_track_worker(ctx: Context) -> std::io::Result<Sender<TrackJob>> {
    let (sender, receiver) = mpsc::channel::<TrackJob>();
    std::thread::Builder::new()
        .name("track-loader".into())
        .spawn(move || {
            while let Ok(job) = receiver.recv() {
                let job = receiver.try_iter().last().unwrap_or(job);
                job();
                ctx.request_repaint();
            }
        })?;
    Ok(sender)
}

fn open_audio(ctx: &Context) -> Result<AudioEngine, String> {
    let wake = ctx.clone();
    AudioEngine::with_error_wake(move || wake.request_repaint()).map_err(|error| error.to_string())
}

/// Cosmetic render settings persisted between sessions via eframe's storage
/// (a small RON file in the OS app-data dir). Deliberately scoped to the
/// spectrum/waveform draw params — not audio, EQ, or window geometry.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PersistedSettings {
    waveform: WaveformParams,
    spectrum: SpectrumParams,
}

impl PersistedSettings {
    /// Load from storage, or defaults if nothing's saved yet / the record is
    /// unreadable. `#[serde(default)]` on the param structs keeps a partial or
    /// outdated record usable instead of discarding the whole thing.
    fn load(storage: &dyn eframe::Storage) -> Self {
        eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
    }
}

impl MusicPlayerApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial_file: Option<PathBuf>,
        file_rx: Option<Receiver<PathBuf>>,
    ) -> Self {
        configure_style(&cc.egui_ctx);
        let audio = open_audio(&cc.egui_ctx);
        let eq_settings = audio
            .as_ref()
            .map(|engine| engine.eq_settings())
            .unwrap_or_default();
        let spectrum = audio
            .as_ref()
            .ok()
            .map(|engine| SpectrumAnalyzer::new(engine.output_sample_rate()));

        // Restore the spectrum/waveform render settings from the last session.
        // Missing or unreadable storage (fresh install) falls back to defaults.
        let settings = cc.storage.map(PersistedSettings::load).unwrap_or_default();
        let status = audio
            .as_ref()
            .err()
            .cloned()
            .unwrap_or_else(|| "Open or drop one audio file.".to_owned());

        let mut app = Self {
            audio,
            suspended_playback: PlaybackState::default(),
            retry_audio_at: Instant::now() + OUTPUT_RETRY_INTERVAL,
            pending_recovery: None,
            track: None,
            cover_texture: None,
            status,
            eq_settings,
            waveform_params: settings.waveform,
            waveform_columns: None,
            track_worker: None,
            pending_load: None,
            pending_analysis: None,
            analysis_gate: AnalysisGate::default(),
            file_rx,
            spectrum,
            spectrum_params: settings.spectrum,
            clip_meter: ClipMeter::default(),
            viz_samples: Vec::with_capacity(crate::spectrum::FFT_SIZE),
        };

        if let Some(path) = initial_file {
            app.load_path(path, &cc.egui_ctx);
        }

        app
    }

    /// Kick off decoding/resampling on a background thread so the UI never
    /// freezes while a file loads. The result is picked up in [`Self::poll_load`].
    fn load_path(&mut self, path: PathBuf, ctx: &Context) {
        let display = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        self.status = format!("Loading {display}\u{2026}");

        let output = self
            .audio
            .as_ref()
            .ok()
            .map(|engine| (engine.output_sample_rate(), engine.output_channels()));

        self.pending_load = None;
        self.pending_recovery = None;
        match self.queue_track_work(ctx, move |canceled| {
            load_track(path, output, || canceled.load(Ordering::Relaxed))
        }) {
            Ok(pending) => self.pending_load = Some(pending),
            Err(error) => self.status = format!("Could not start file load ({error})"),
        }
    }

    fn queue_track_work<T: Send + 'static>(
        &mut self,
        ctx: &Context,
        work: impl FnOnce(&AtomicBool) -> T + Send + 'static,
    ) -> std::io::Result<Pending<T>> {
        let (result, pending) = Pending::channel();
        let canceled = Arc::clone(&pending.canceled);
        let mut job: TrackJob = Box::new(move || {
            if !canceled.load(Ordering::Relaxed) {
                let _ = result.send(work(&canceled));
            }
        });
        if let Some(worker) = &self.track_worker {
            match worker.send(job) {
                Ok(()) => return Ok(pending),
                Err(error) => job = error.0,
            }
        }
        let worker = start_track_worker(ctx.clone())?;
        worker
            .send(job)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.track_worker = Some(worker);
        Ok(pending)
    }

    /// Apply a finished background load, if one is ready.
    fn poll_load(&mut self, ctx: &Context) {
        let outcome = match &self.pending_load {
            Some(pending) => match pending.result.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.pending_load = None;
                    self.status = "Could not finish opening the file.".to_owned();
                    return;
                }
            },
            None => return,
        };
        self.pending_load = None;

        match outcome {
            LoadOutcome::Failed(error) => {
                self.status = error;
            }
            LoadOutcome::Loaded(data) => {
                let LoadedData {
                    info,
                    decoded,
                    buffer,
                } = *data;
                let buffer = Arc::new(buffer);
                if let Ok(engine) = &self.audio {
                    engine.load(Arc::clone(&buffer));
                    engine.play();
                } else {
                    self.suspended_playback.load(Arc::clone(&buffer));
                    self.suspended_playback.play();
                }
                self.start_waveform_analysis(ctx, buffer);
                self.cover_texture = info
                    .cover_art
                    .as_ref()
                    .and_then(|cover| texture_from_cover(ctx, cover));
                self.status = self
                    .audio
                    .as_ref()
                    .err()
                    .cloned()
                    .unwrap_or_else(|| format!("Loaded {}", info.title));
                self.track = Some(LoadedTrack { info, decoded });
                self.waveform_columns = None;
            }
        }
    }

    fn start_waveform_analysis(&mut self, ctx: &Context, buffer: Arc<PlaybackBuffer>) {
        let generation = self.analysis_gate.begin();
        let (sender, pending) = Pending::channel();
        let canceled = Arc::clone(&pending.canceled);
        let wake = ctx.clone();
        self.pending_analysis = None;
        let worker = std::thread::Builder::new()
            .name("waveform-analysis".into())
            .spawn(move || {
                if let Some(analysis) = analyze_waveform_cancelable(
                    &buffer.samples,
                    buffer.channels,
                    buffer.sample_rate,
                    WAVEFORM_BINS,
                    || canceled.load(Ordering::Relaxed),
                ) {
                    let _ = sender.send((generation, analysis));
                    wake.request_repaint();
                }
            });
        if worker.is_ok() {
            self.pending_analysis = Some(pending);
        }
    }

    /// Apply a finished background waveform analysis, if one is ready. A result
    /// from a superseded load is dropped rather than painted onto the track that
    /// replaced it.
    fn poll_analysis(&mut self) {
        let Some(pending) = &self.pending_analysis else {
            return;
        };
        match pending.result.try_recv() {
            Ok((generation, analysis)) => {
                self.pending_analysis = None;
                if self.analysis_gate.accepts(generation)
                    && let Some(track) = &mut self.track
                {
                    track.decoded.waveform = analysis;
                    self.waveform_columns = None;
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.pending_analysis = None,
        }
    }

    fn poll_audio_error(&mut self) {
        if let Ok(engine) = &self.audio
            && let Some(error) = engine.take_output_error()
        {
            self.suspended_playback = engine.suspend_playback();
            let status = format!("Audio output disconnected ({error})");
            if self.pending_load.is_none() {
                self.status.clone_from(&status);
            }
            self.audio = Err(status);
            self.spectrum = None;
            self.retry_audio_at = Instant::now();
        }
    }

    fn poll_audio_recovery(&mut self, ctx: &Context) {
        if self.audio.is_ok() || self.pending_load.is_some() {
            return;
        }

        if let Some(pending) = &self.pending_recovery {
            let result = match pending.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    Err("Could not restart audio output.".to_owned())
                }
            };
            self.pending_recovery = None;
            match result {
                Ok(output) if output.engine.take_output_error().is_none() => {
                    self.restore_audio(ctx, output);
                    return;
                }
                Ok(_) => self.update_audio_error("Audio output disconnected.".to_owned()),
                Err(error) => self.update_audio_error(error),
            }
            self.retry_audio_at = Instant::now() + OUTPUT_RETRY_INTERVAL;
        }

        let delay = self
            .retry_audio_at
            .saturating_duration_since(Instant::now());
        if !delay.is_zero() {
            ctx.request_repaint_after(delay);
            return;
        }

        let wake = ctx.clone();
        let buffer = self.suspended_playback.track();
        let source = self.track.as_ref().map(|track| track.info.path.clone());
        let layout = self
            .track
            .as_ref()
            .zip(buffer.as_ref())
            .filter(|(track, buffer)| track.decoded.channels == buffer.channels)
            .and_then(|(track, _)| track.decoded.channel_layout.clone());
        match self.queue_track_work(ctx, move |canceled| {
            let engine = open_audio(&wake)?;
            let buffer = match buffer {
                Some(buffer) => recovery_buffer(
                    &buffer,
                    source.as_deref(),
                    layout.as_ref(),
                    (engine.output_sample_rate(), engine.output_channels()),
                    || canceled.load(Ordering::Relaxed),
                )?,
                None => None,
            };
            Ok(RecoveredOutput { engine, buffer })
        }) {
            Ok(pending) => self.pending_recovery = Some(pending),
            Err(error) => {
                self.update_audio_error(format!("Could not restart audio output ({error})"));
                self.retry_audio_at = Instant::now() + OUTPUT_RETRY_INTERVAL;
                ctx.request_repaint_after(OUTPUT_RETRY_INTERVAL);
            }
        }
    }

    fn update_audio_error(&mut self, error: String) {
        if self.audio.as_ref().err() == Some(&self.status) {
            self.status.clone_from(&error);
        }
        self.audio = Err(error);
    }

    fn restore_audio(&mut self, ctx: &Context, output: RecoveredOutput) {
        let RecoveredOutput { engine, buffer } = output;
        if let Some(buffer) = buffer {
            let buffer = Arc::new(buffer);
            self.suspended_playback.replace_buffer(Arc::clone(&buffer));
            self.start_waveform_analysis(ctx, buffer);
        }
        engine.set_eq_settings(self.eq_settings);
        engine.restore_playback(std::mem::take(&mut self.suspended_playback));
        self.spectrum = Some(SpectrumAnalyzer::new(engine.output_sample_rate()));
        if self.audio.as_ref().err() == Some(&self.status) {
            self.status = match &self.track {
                Some(track) => format!("Loaded {}", track.info.title),
                None => "Open or drop one audio file.".to_owned(),
            };
        }
        self.audio = Ok(engine);
    }

    /// Load a file forwarded from a secondary launch (double-click in Explorer)
    /// and pop the window to the front, like a typical media player. Only the most
    /// recent request matters if several arrive in one frame.
    fn poll_file_requests(&mut self, ctx: &Context) {
        let latest = match &self.file_rx {
            Some(rx) => {
                let mut latest = None;
                while let Ok(path) = rx.try_recv() {
                    latest = Some(path);
                }
                latest
            }
            None => None,
        };
        if let Some(path) = latest {
            self.load_path(path, ctx);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn open_file_dialog(&mut self, ctx: &Context) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Open audio file")
            .add_filter("Audio", &["mp3", "wav", "flac", "m4a", "aac"])
            .pick_file()
        {
            self.load_path(path, ctx);
        }
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        self.audio.as_ref().map_or_else(
            |_| self.suspended_playback.snapshot(),
            AudioEngine::snapshot,
        )
    }

    fn handle_dropped_files(&mut self, ctx: &Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        if let Some(file) = dropped.into_iter().find_map(|file| file.path) {
            self.load_path(file, ctx);
        }
    }
}

impl eframe::App for MusicPlayerApp {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_audio_error();
        // eframe runs logic even for a minimized window when IPC requests a repaint.
        self.poll_file_requests(ctx);
        self.handle_dropped_files(ctx);
        self.poll_load(ctx);
        self.poll_analysis();
        self.poll_audio_recovery(ctx);
        if self.pending_load.is_some()
            || self.pending_analysis.is_some()
            || self.pending_recovery.is_some()
        {
            // Check for a disconnected worker even if it could not request a repaint.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dt = ctx.input(|input| input.stable_dt).clamp(0.0, 0.1);
        self.update_visualizers(dt);

        egui::Panel::top("top_bar")
            .exact_size(34.0)
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(24, 24, 24)))
            .show_inside(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new("MusicPlayer").strong().color(Color32::WHITE));
                    if ui.button("Open").clicked() {
                        self.open_file_dialog(&ctx);
                    }
                    ui.add_space(8.0);
                    ui.label(RichText::new(&self.status).color(Color32::from_gray(170)));
                    if let Some(track) = &self.track {
                        ui.add_space(8.0);
                        let (text, color) = if track.info.traktor_analyzed {
                            ("Traktor analyzed", Color32::from_rgb(95, 200, 120))
                        } else {
                            ("Not Traktor analyzed", Color32::from_rgb(205, 130, 95))
                        };
                        ui.label(RichText::new(text).strong().color(color));
                    }
                });
            });

        // Transport spans the full width along the bottom.
        egui::Panel::bottom("transport")
            .exact_size(120.0)
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(36, 36, 36)))
            .show_inside(ui, |ui| self.show_transport(ui));

        // Inspector is declared next so it claims the right column first; the
        // spectrum strip below then only spans the remaining (central) width and
        // resizes when the inspector is dragged.
        egui::Panel::right("inspector")
            .resizable(true)
            .default_size(300.0)
            .size_range(240.0..=560.0)
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(18, 18, 18)))
            .show_inside(ui, |ui| self.show_inspector(ui));

        // The panel's own fill is the strip background, so the dark reaches every
        // edge (no lighter frame peeking out the sides). Bars sit in a slightly
        // inset area within it.
        egui::Panel::bottom("spectrum")
            .exact_size(68.0)
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(14, 14, 14)))
            .show_inside(ui, |ui| {
                let strip = ui.max_rect().shrink2(Vec2::new(6.0, 5.0));
                let params = self.spectrum_params;
                self.paint_spectrum(ui.painter(), strip, params);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(28, 28, 28)))
            .show_inside(ui, |ui| self.show_album_area(ui));

        if (self.audio.is_ok() && self.snapshot().transport == TransportState::Playing)
            || self.clip_meter.is_animating()
            || self
                .spectrum
                .as_ref()
                .is_some_and(SpectrumAnalyzer::is_animating)
        {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    /// Persist the cosmetic render settings. eframe calls this on shutdown and
    /// every `auto_save_interval`, so a Reset that's left in place is saved too —
    /// the next launch then opens with those defaults, exactly as left.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = PersistedSettings {
            waveform: self.waveform_params,
            spectrum: self.spectrum_params,
        };
        eframe::set_value(storage, eframe::APP_KEY, &settings);
    }

    /// We save our own settings explicitly above; don't also persist egui's
    /// memory (panel widths, which sections are open). Keeps cross-session state
    /// limited to what the Spectrum/Waveform panels actually tweak.
    fn persist_egui_memory(&self) -> bool {
        false
    }
}

impl MusicPlayerApp {
    fn show_album_area(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let side = available.x.min(available.y).clamp(180.0, 420.0);
        ui.allocate_ui_with_layout(
            available,
            Layout::centered_and_justified(egui::Direction::TopDown),
            |ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
                ui.painter()
                    .rect_filled(rect, 0.0, Color32::from_rgb(48, 48, 48));
                ui.painter().rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(1.0_f32, Color32::from_rgb(82, 82, 82)),
                    egui::StrokeKind::Inside,
                );

                if let Some(texture) = &self.cover_texture {
                    let image_size = fit_size(texture.size_vec2(), rect.size());
                    let image_rect = Rect::from_center_size(rect.center(), image_size);
                    ui.painter().image(
                        texture.id(),
                        image_rect,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "No Cover Art",
                        egui::TextStyle::Button.resolve(ui.style()),
                        Color32::from_gray(175),
                    );
                }
            },
        );
    }

    fn show_track_info(&mut self, ui: &mut egui::Ui) {
        let Some(track) = &self.track else {
            ui.add_space(8.0);
            ui.label(RichText::new("No track loaded").color(Color32::from_gray(170)));
            return;
        };

        metadata_row(ui, "Title", &track.info.title);
        metadata_row(ui, "Artist", optional_text(&track.info.artist));
        if let Some(genre) = &track.info.genre {
            metadata_row(ui, "Genre", genre);
        }
        if let Some(bpm) = &track.info.bpm {
            metadata_row(ui, "BPM", bpm);
        }
        if let Some(key) = &track.info.key {
            metadata_row(ui, "Key", key);
        }
        metadata_row(ui, "Duration", &format_duration(track.decoded.duration));
        if let Some(bitrate) = track.info.bitrate {
            metadata_row(ui, "Bitrate", &format!("{bitrate} kbps"));
        }
        if let Ok(engine) = &self.audio {
            metadata_row(
                ui,
                "Output",
                &output_path_summary(
                    track.decoded.sample_rate,
                    track.decoded.channels,
                    engine.output_sample_rate(),
                    engine.output_channels(),
                ),
            );
        }
        if track.info.traktor_analyzed {
            metadata_row(ui, "Analyzed", "Traktor");
        }
        metadata_row(ui, "File", &display_path(&track.info.path));
    }

    fn show_transport(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.snapshot();
        let waveform_height = 72.0;
        let width = ui.available_width();
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(width, waveform_height), Sense::click_and_drag());
        self.paint_waveform(ui, rect, snapshot.position_seconds, snapshot.duration);

        if (response.clicked() || response.dragged())
            && snapshot.duration > Duration::ZERO
            && let Some(position) = response.interact_pointer_pos()
        {
            let fraction = ((position.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            if let Ok(engine) = &self.audio {
                engine.seek_seconds(snapshot.duration.as_secs_f32() * fraction);
            }
        }

        ui.add_space(8.0);
        // Bar jumps need a tempo and a Traktor analysis; otherwise fall back to
        // plain ±10s skips.
        let bars_mode = self
            .track
            .as_ref()
            .filter(|track| track.info.traktor_analyzed)
            .and_then(|track| track.info.bpm.as_deref())
            .and_then(parse_bpm);
        let controls_enabled = snapshot.has_track && self.audio.is_ok();

        // Horizontally center the control cluster. egui can't shrink-wrap a
        // `horizontal` row, so we measure its width one frame and use it to pad the
        // next — stable since the row only changes on resize / track swap.
        let row_id = ui.id().with("transport_controls_w");
        let content_w = ui.data(|d| d.get_temp::<f32>(row_id)).unwrap_or(0.0);
        let offset = ((ui.available_width() - content_w) * 0.5).max(0.0);
        ui.horizontal(|ui| {
            ui.add_space(offset);
            let measured = ui
                .horizontal(|ui| {
                    ui.add_enabled_ui(controls_enabled, |ui| {
                        self.jump_buttons(ui, bars_mode, true);

                        let playing = snapshot.transport == TransportState::Playing;
                        if play_pause_button(ui, playing).clicked()
                            && let Ok(engine) = &self.audio
                        {
                            engine.toggle_playback();
                        }

                        self.jump_buttons(ui, bars_mode, false);
                    });

                    ui.add_space(14.0);
                    ui.label(RichText::new("Vol").color(Color32::from_gray(190)));
                    let mut volume = snapshot.volume;
                    if ui
                        .add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false))
                        .changed()
                    {
                        if let Ok(engine) = &self.audio {
                            engine.set_volume(volume);
                        } else {
                            self.suspended_playback.set_volume(volume);
                        }
                    }

                    ui.add_space(14.0);
                    ui.label(
                        RichText::new(format!(
                            "{} / {}",
                            format_duration_secs(snapshot.position_seconds),
                            format_duration(snapshot.duration)
                        ))
                        .color(Color32::WHITE),
                    );

                    ui.add_space(10.0);
                    let (meter_rect, meter_response) =
                        ui.allocate_exact_size(Vec2::new(10.0, 32.0), Sense::hover());
                    self.paint_clip_meter(ui.painter(), meter_rect);
                    meter_response.on_hover_text("Output level — top turns red when clipping");
                })
                .response
                .rect
                .width();
            ui.data_mut(|d| d.insert_temp(row_id, measured));
        });
    }

    /// Render either the rewind set (`leading`) or the fast-forward set. With a
    /// tempo it's `±4/8/16/32` bars; without, a single `±10s` button.
    fn jump_buttons(&self, ui: &mut egui::Ui, bars_mode: Option<f32>, leading: bool) {
        match bars_mode {
            Some(bpm) => {
                let bars_set: [i32; 4] = if leading {
                    [-32, -16, -8, -4]
                } else {
                    [4, 8, 16, 32]
                };
                for bars in bars_set {
                    let label = if bars > 0 {
                        format!("+{bars}")
                    } else {
                        bars.to_string()
                    };
                    let seconds = bars_to_seconds(bars, bpm);
                    if ui
                        .add_sized([38.0, 28.0], Button::new(label))
                        .on_hover_text(format!("{} bars (~{:.1}s)", bars.abs(), seconds.abs()))
                        .clicked()
                        && let Ok(engine) = &self.audio
                    {
                        engine.skip_seconds(seconds);
                    }
                }
            }
            None => {
                let (label, delta) = if leading {
                    ("-10", -10.0)
                } else {
                    ("+10", 10.0)
                };
                if ui.add_sized([42.0, 28.0], Button::new(label)).clicked()
                    && let Ok(engine) = &self.audio
                {
                    engine.skip_seconds(delta);
                }
            }
        }
    }

    fn paint_waveform(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        position_seconds: f32,
        duration: Duration,
    ) {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, Color32::from_rgb(19, 19, 19));

        let Some(track) = &self.track else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop or open an audio file",
                egui::TextStyle::Button.resolve(ui.style()),
                Color32::from_gray(140),
            );
            return;
        };

        let analysis = &track.decoded.waveform;
        if analysis.is_empty() {
            let status = if self.pending_analysis.is_some() {
                "Analyzing waveform\u{2026}"
            } else {
                "Waveform unavailable"
            };
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                status,
                egui::TextStyle::Button.resolve(ui.style()),
                Color32::from_gray(120),
            );
            return;
        }

        let params = self.waveform_params;
        let bin_count = analysis.len();

        let progress = if duration > Duration::ZERO {
            (position_seconds / duration.as_secs_f32()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let played_x = rect.left() + rect.width() * progress;
        let center_y = rect.center().y;
        let half_height = rect.height() * 0.46;
        let baseline_y = center_y + half_height;
        let width = rect.width().max(1.0);

        // Collapse the high-res analysis into `params.bins` drawn columns, capped
        // at one column per pixel so we never overdraw.
        let columns = params
            .bins
            .min(bin_count)
            .min(width.round() as usize)
            .max(1);
        let spectral = params.color_mode == ColorMode::Spectral && track.info.traktor_analyzed;

        if self
            .waveform_columns
            .as_ref()
            .is_some_and(|cache| cache.params != params || cache.bars.len() != columns)
        {
            self.waveform_columns = None;
        }
        let cache = self
            .waveform_columns
            .get_or_insert_with(|| WaveformColumns::new(analysis, params, columns, spectral));

        let slot = width / columns as f32;
        let bar_width = (slot - 1.0).clamp(1.0, slot.max(1.0));
        for (column, bar) in cache.bars.iter().enumerate() {
            let height = (bar.height * half_height).max(0.5);
            let x = rect.left() + (column as f32 + 0.5) * slot;
            let color = if x <= played_x {
                bar.played
            } else {
                bar.unplayed
            };

            let (top, bottom) = if params.mirror {
                (center_y - height, center_y + height)
            } else {
                (baseline_y - 2.0 * height, baseline_y)
            };
            painter.line_segment(
                [Pos2::new(x, top), Pos2::new(x, bottom)],
                Stroke::new(bar_width, color),
            );
        }

        painter.line_segment(
            [
                Pos2::new(played_x, rect.top()),
                Pos2::new(played_x, rect.bottom()),
            ],
            Stroke::new(1.0_f32, Color32::from_rgb(170, 210, 225)),
        );
    }

    /// Feed the audio tap into the spectrum + clip meter, or ease them down when
    /// nothing is playing.
    fn update_visualizers(&mut self, dt: f32) {
        let playing = matches!(self.snapshot().transport, TransportState::Playing);
        if playing && let Ok(engine) = &self.audio {
            let levels = engine.drain_viz(&mut self.viz_samples);
            let params = self.spectrum_params;
            if let Some(spectrum) = &mut self.spectrum {
                spectrum.update(&self.viz_samples, &params, dt);
            }
            self.clip_meter.update(levels.peak, levels.clipped, dt);
        } else {
            if let Some(spectrum) = &mut self.spectrum {
                spectrum.decay(dt);
            }
            self.clip_meter.decay(dt);
        }
    }

    fn paint_spectrum(&self, painter: &egui::Painter, rect: Rect, params: SpectrumParams) {
        // Background comes from the panel fill (full width); `rect` is the inset
        // area the bars draw into.
        let Some(spectrum) = &self.spectrum else {
            return;
        };
        let bars = spectrum.bars();
        let peaks = spectrum.peaks();
        let n = bars.len();
        if n == 0 || rect.width() <= 1.0 {
            return;
        }

        let baseline = rect.bottom() - 1.0;
        let usable = (rect.height() - 2.0).max(1.0);

        // Downsample the analyzer's n bars to dn display columns by taking the
        // loudest bar in each bucket — keeps transient energy visible. dn <= n
        // always (clamped below), so a bucket is never empty.
        let sb = |d: usize, dn: usize| bucket_max(bars, d, dn);
        let sp = |d: usize, dn: usize| bucket_max(peaks, d, dn);

        // Target on-screen pitch per column, in points. The column count is
        // width / pitch (capped at BAR_COUNT), so bars keep a *constant* width as
        // the window grows and only multiply — the fat bars on fullscreen were
        // just 64 bars stretched to fill. ~5 pt reproduces the default-window
        // density; columns only have to widen again past ~a fullscreen window.
        let min_px: f32 = 5.0;

        if params.symmetric {
            // Center = lowest freq, edges = highest. Both halves mirror each other.
            let half_w = rect.width() * 0.5;
            let half_n = ((half_w / min_px) as usize).clamp(1, n);
            let slot = half_w / half_n as f32;
            let cx = rect.left() + half_w;

            match params.mode {
                SpectrumDisplayMode::Bars => {
                    let gap = (slot * 0.22).clamp(0.5, 3.0);
                    let bw = (slot - gap).max(1.0);
                    let cap_color = Color32::from_rgba_unmultiplied(235, 235, 235, 170);

                    for d in 0..half_n {
                        // Left half: d=0 is the left edge (highest), d=half_n-1 is center (lowest).
                        let li = half_n - 1 - d;
                        let lv = sb(li, half_n);
                        let lh = lv.clamp(0.0, 1.0) * usable;
                        let lx = rect.left() + d as f32 * slot + gap * 0.5;
                        if lh > 0.5 {
                            paint_vgradient(
                                painter,
                                Rect::from_min_max(
                                    Pos2::new(lx, baseline - lh),
                                    Pos2::new(lx + bw, baseline),
                                ),
                                level_color(0.0),
                                level_color(lv),
                            );
                        }
                        // Peak caps are a Bars-only flourish (the Line style omits them).
                        {
                            let cy = baseline - sp(li, half_n).clamp(0.0, 1.0) * usable;
                            painter.line_segment(
                                [Pos2::new(lx, cy), Pos2::new(lx + bw, cy)],
                                Stroke::new(1.5_f32, cap_color),
                            );
                        }

                        // Right half: d=0 is center (lowest), d=half_n-1 is right edge (highest).
                        let rv = sb(d, half_n);
                        let rh = rv.clamp(0.0, 1.0) * usable;
                        let rx = cx + d as f32 * slot + gap * 0.5;
                        if rh > 0.5 {
                            paint_vgradient(
                                painter,
                                Rect::from_min_max(
                                    Pos2::new(rx, baseline - rh),
                                    Pos2::new(rx + bw, baseline),
                                ),
                                level_color(0.0),
                                level_color(rv),
                            );
                        }
                        {
                            let cy = baseline - sp(d, half_n).clamp(0.0, 1.0) * usable;
                            painter.line_segment(
                                [Pos2::new(rx, cy), Pos2::new(rx + bw, cy)],
                                Stroke::new(1.5_f32, cap_color),
                            );
                        }
                    }
                }

                SpectrumDisplayMode::Line => {
                    let floor = level_color(0.0);
                    let mut mesh = Mesh::default();

                    // Left half: left edge=high, center=low.
                    for d in 0..half_n {
                        let li = half_n - 1 - d;
                        let v = sb(li, half_n);
                        let h = v.clamp(0.0, 1.0) * usable;
                        let x = rect.left() + (d as f32 + 0.5) * slot;
                        let idx = mesh.vertices.len() as u32;
                        mesh.colored_vertex(Pos2::new(x, baseline - h), level_color(v));
                        mesh.colored_vertex(Pos2::new(x, baseline), floor);
                        if d > 0 {
                            mesh.add_triangle(idx - 2, idx - 1, idx);
                            mesh.add_triangle(idx - 1, idx + 1, idx);
                        }
                    }

                    // Right half: center=low, right edge=high. Connected to left half.
                    for d in 0..half_n {
                        let v = sb(d, half_n);
                        let h = v.clamp(0.0, 1.0) * usable;
                        let x = cx + (d as f32 + 0.5) * slot;
                        let idx = mesh.vertices.len() as u32;
                        mesh.colored_vertex(Pos2::new(x, baseline - h), level_color(v));
                        mesh.colored_vertex(Pos2::new(x, baseline), floor);
                        mesh.add_triangle(idx - 2, idx - 1, idx);
                        mesh.add_triangle(idx - 1, idx + 1, idx);
                    }
                    painter.add(mesh);

                    let ridge: Vec<Pos2> = (0..half_n)
                        .map(|d| {
                            let h = sb(half_n - 1 - d, half_n).clamp(0.0, 1.0) * usable;
                            Pos2::new(rect.left() + (d as f32 + 0.5) * slot, baseline - h)
                        })
                        .chain((0..half_n).map(|d| {
                            let h = sb(d, half_n).clamp(0.0, 1.0) * usable;
                            Pos2::new(cx + (d as f32 + 0.5) * slot, baseline - h)
                        }))
                        .collect();
                    painter.add(egui::Shape::line(
                        ridge,
                        Stroke::new(1.5_f32, Color32::from_rgba_unmultiplied(230, 230, 230, 150)),
                    ));
                }
            }
        } else {
            // Non-symmetric: low freq at left, high freq at right.
            let display_n = ((rect.width() / min_px) as usize).clamp(1, n);
            let slot = rect.width() / display_n as f32;

            match params.mode {
                SpectrumDisplayMode::Bars => {
                    let gap = (slot * 0.22).clamp(0.5, 3.0);
                    let bw = (slot - gap).max(1.0);
                    let cap_color = Color32::from_rgba_unmultiplied(235, 235, 235, 170);
                    for i in 0..display_n {
                        let v = sb(i, display_n);
                        let h = v.clamp(0.0, 1.0) * usable;
                        let x = rect.left() + i as f32 * slot + gap * 0.5;
                        if h > 0.5 {
                            paint_vgradient(
                                painter,
                                Rect::from_min_max(
                                    Pos2::new(x, baseline - h),
                                    Pos2::new(x + bw, baseline),
                                ),
                                level_color(0.0),
                                level_color(v),
                            );
                        }
                        // Peak caps are a Bars-only flourish (the Line style omits them).
                        {
                            let cy = baseline - sp(i, display_n).clamp(0.0, 1.0) * usable;
                            painter.line_segment(
                                [Pos2::new(x, cy), Pos2::new(x + bw, cy)],
                                Stroke::new(1.5_f32, cap_color),
                            );
                        }
                    }
                }

                SpectrumDisplayMode::Line => {
                    let floor = level_color(0.0);
                    let mut mesh = Mesh::default();
                    for i in 0..display_n {
                        let v = sb(i, display_n);
                        let h = v.clamp(0.0, 1.0) * usable;
                        let x = rect.left() + (i as f32 + 0.5) * slot;
                        let idx = mesh.vertices.len() as u32;
                        mesh.colored_vertex(Pos2::new(x, baseline - h), level_color(v));
                        mesh.colored_vertex(Pos2::new(x, baseline), floor);
                        if i > 0 {
                            mesh.add_triangle(idx - 2, idx - 1, idx);
                            mesh.add_triangle(idx - 1, idx + 1, idx);
                        }
                    }
                    painter.add(mesh);

                    let pts: Vec<Pos2> = (0..display_n)
                        .map(|i| {
                            let h = sb(i, display_n).clamp(0.0, 1.0) * usable;
                            Pos2::new(rect.left() + (i as f32 + 0.5) * slot, baseline - h)
                        })
                        .collect();
                    painter.add(egui::Shape::line(
                        pts,
                        Stroke::new(1.5_f32, Color32::from_rgba_unmultiplied(230, 230, 230, 150)),
                    ));
                }
            }
        }
    }

    fn paint_clip_meter(&self, painter: &egui::Painter, rect: Rect) {
        painter.rect_filled(rect, 3.0, Color32::from_rgb(14, 14, 14));
        let meter = &self.clip_meter;
        let inset = rect.shrink(2.0);
        let baseline = inset.bottom();
        let usable = inset.height().max(1.0);

        let height = meter.level.clamp(0.0, 1.0) * usable;
        if height > 0.5 {
            let bar = Rect::from_min_max(
                Pos2::new(inset.left(), baseline - height),
                Pos2::new(inset.right(), baseline),
            );
            paint_vgradient(painter, bar, level_color(0.0), level_color(meter.level));
        }

        let cap_y = baseline - meter.cap.clamp(0.0, 1.0) * usable;
        painter.line_segment(
            [
                Pos2::new(inset.left(), cap_y),
                Pos2::new(inset.right(), cap_y),
            ],
            Stroke::new(1.5_f32, Color32::from_rgba_unmultiplied(235, 235, 235, 200)),
        );

        // Clip latch: a solid red block at the very top after a true full-scale
        // overshoot, held briefly so quick clips aren't missed.
        if meter.clip_latch > 0.0 {
            let clip_height = (usable * 0.14).max(3.0);
            let clip_rect = Rect::from_min_max(
                Pos2::new(inset.left(), inset.top()),
                Pos2::new(inset.right(), inset.top() + clip_height),
            );
            painter.rect_filled(clip_rect, 0.0, Color32::from_rgb(255, 64, 52));
        }

        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0_f32, Color32::from_rgb(64, 64, 64)),
            egui::StrokeKind::Inside,
        );
    }

    /// The right-hand inspector: a scrollable stack of collapsible sections
    /// (Unreal/Unity-style), replacing the old floating EQ window.
    fn show_inspector(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                egui::CollapsingHeader::new(RichText::new("Track info").strong())
                    .default_open(true)
                    .show(ui, |ui| self.show_track_info(ui));
                egui::CollapsingHeader::new(RichText::new("Waveform").strong())
                    .default_open(true)
                    .show(ui, |ui| self.show_waveform_section(ui));

                egui::CollapsingHeader::new(RichText::new("Spectrum").strong())
                    .default_open(false)
                    .show(ui, |ui| self.show_spectrum_section(ui));

                egui::CollapsingHeader::new(RichText::new("Equalizer").strong())
                    .default_open(false)
                    .show(ui, |ui| self.show_equalizer_section(ui));
            });
    }

    /// Live waveform-rendering controls. These only reshape the *drawing* — the
    /// stored analysis stays amplitude-accurate.
    fn show_waveform_section(&mut self, ui: &mut egui::Ui) {
        let traktor = self
            .track
            .as_ref()
            .is_some_and(|track| track.info.traktor_analyzed);
        let params = &mut self.waveform_params;

        ui.add_space(2.0);
        ComboBox::from_label("Reduction")
            .selected_text(match params.mode {
                ReductionMode::Peak => "Peak",
                ReductionMode::Rms => "RMS",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut params.mode, ReductionMode::Peak, "Peak (accurate)");
                ui.selectable_value(&mut params.mode, ReductionMode::Rms, "RMS (smooth)");
            });
        ui.add(egui::Slider::new(&mut params.bins, 64..=2000).text("Bars"));
        ui.add(egui::Slider::new(&mut params.gamma, 0.3..=2.0).text("Gamma"));
        ui.add(egui::Slider::new(&mut params.height_scale, 0.2..=2.5).text("Height"));
        ui.add(egui::Slider::new(&mut params.smoothing, 0..=8).text("Smoothing"));
        ui.checkbox(&mut params.mirror, "Mirror");

        ui.horizontal(|ui| {
            ui.label("Color");
            ui.selectable_value(&mut params.color_mode, ColorMode::Solid, "Solid");
            ui.add_enabled_ui(traktor, |ui| {
                ui.selectable_value(&mut params.color_mode, ColorMode::Spectral, "Spectral")
                    .on_disabled_hover_text("Traktor-analyzed tracks only");
            });
        });

        ui.add_space(4.0);
        if ui.button("Reset waveform").clicked() {
            *params = WaveformParams::default();
        }
    }

    fn show_spectrum_section(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.spectrum_params;
        ui.add_space(2.0);

        ui.horizontal(|ui| {
            ui.label("Style");
            ui.selectable_value(&mut p.mode, SpectrumDisplayMode::Bars, "Bars");
            ui.selectable_value(&mut p.mode, SpectrumDisplayMode::Line, "Line");
        });

        ui.add(
            egui::Slider::new(&mut p.db_floor, -120.0..=-30.0)
                .text("Floor dB")
                .suffix(" dB"),
        );
        ui.add(
            egui::Slider::new(&mut p.db_ceil, -30.0..=0.0)
                .text("Ceil dB")
                .suffix(" dB"),
        );
        // Keep floor below ceil with a 6 dB minimum gap.
        if p.db_floor >= p.db_ceil - 6.0 {
            p.db_floor = p.db_ceil - 6.0;
        }

        ui.add(egui::Slider::new(&mut p.sensitivity, 0.25..=3.0).text("Sensitivity"));
        ui.checkbox(&mut p.symmetric, "Symmetric (center = bass)");

        ui.add_space(4.0);
        if ui.button("Reset spectrum").clicked() {
            *p = SpectrumParams::default();
        }
    }

    fn show_equalizer_section(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut enabled = self.eq_settings.enabled();
        if ui.checkbox(&mut enabled, "Enabled").changed() {
            self.eq_settings.set_enabled(enabled);
            changed = true;
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for (index, frequency) in EQ_BANDS_HZ.iter().enumerate() {
                ui.vertical(|ui| {
                    let mut gain = self.eq_settings.gain_db(index).unwrap_or(0.0);
                    if ui
                        .add(
                            egui::Slider::new(&mut gain, -12.0..=12.0)
                                .vertical()
                                .show_value(false),
                        )
                        .changed()
                    {
                        self.eq_settings.set_gain(index, gain);
                        changed = true;
                    }
                    ui.label(RichText::new(format_band(*frequency)).size(11.0));
                });
            }
        });

        ui.add_space(6.0);
        if ui.button("Reset").clicked() {
            self.eq_settings.reset();
            changed = true;
        }

        if changed && let Ok(engine) = &self.audio {
            engine.set_eq_settings(self.eq_settings);
        }
    }
}

impl WaveformColumns {
    fn new(
        analysis: &WaveformAnalysis,
        params: WaveformParams,
        columns: usize,
        spectral: bool,
    ) -> Self {
        let amplitude = analysis.amplitude(params.mode);
        let mut amplitudes = Vec::with_capacity(columns);
        let mut bars = Vec::with_capacity(columns);
        for column in 0..columns {
            let start = column * amplitude.len() / columns;
            let end = (column + 1) * amplitude.len() / columns;
            let values = &amplitude[start..end];
            let level = match params.mode {
                ReductionMode::Peak => values.iter().copied().fold(0.0, f32::max),
                ReductionMode::Rms => {
                    let power: f32 = values.iter().map(|value| value * value).sum();
                    (power / values.len() as f32).sqrt()
                }
            };
            let (played, unplayed) = if spectral {
                let low = analysis.low[start..end].iter().copied().fold(0.0, f32::max);
                let mid = analysis.mid[start..end].iter().copied().fold(0.0, f32::max);
                let high = analysis.high[start..end]
                    .iter()
                    .copied()
                    .fold(0.0, f32::max);
                (
                    spectral_color(low, mid, high, true),
                    spectral_color(low, mid, high, false),
                )
            } else {
                (
                    Color32::from_rgb(96, 158, 184),
                    Color32::from_rgb(72, 78, 84),
                )
            };
            amplitudes.push(level);
            bars.push(WaveformColumn {
                height: 0.0,
                played,
                unplayed,
            });
        }
        if params.smoothing > 0 {
            amplitudes = smooth(&amplitudes, params.smoothing);
        }
        for (bar, amplitude) in bars.iter_mut().zip(amplitudes) {
            bar.height = (amplitude.clamp(0.0, 1.0).powf(params.gamma) * params.height_scale)
                .clamp(0.0, 1.0);
        }
        Self { params, bars }
    }
}

/// Runs on a worker thread: decode, read tags, and resample into a buffer
/// matching the audio device. `output` is the device's `(sample_rate, channels)`,
/// or `None` when no audio engine is available (metadata/waveform only).
fn load_track(
    path: PathBuf,
    output: Option<(u32, usize)>,
    canceled: impl Fn() -> bool,
) -> LoadOutcome {
    let mut decoded = match decode_track_cancelable(&path, &canceled) {
        Ok(decoded) => decoded,
        Err(error) => return LoadOutcome::Failed(error.to_string()),
    };

    if canceled() {
        return LoadOutcome::Failed(DecodeError::Canceled.to_string());
    }
    let mut info = read_track_tags(&path).unwrap_or_else(|_| TrackInfo::fallback(path.clone()));
    info.duration = Some(decoded.duration);
    info.sample_rate = Some(decoded.sample_rate);
    info.channels = Some(decoded.channels as u16);

    if canceled() {
        return LoadOutcome::Failed(DecodeError::Canceled.to_string());
    }
    // Playback and waveform analysis share a single owned sample buffer.
    let samples = std::mem::take(&mut decoded.samples);

    let buffer = match output {
        Some(output) => {
            match prepare_playback_buffer(
                samples,
                (decoded.sample_rate, decoded.channels),
                decoded.channel_layout.as_ref(),
                output,
            ) {
                Ok(buffer) => buffer,
                Err(error) => return LoadOutcome::Failed(error.to_string()),
            }
        }
        None => PlaybackBuffer::new(samples, decoded.sample_rate, decoded.channels),
    };

    LoadOutcome::Loaded(Box::new(LoadedData {
        info,
        decoded,
        buffer,
    }))
}

fn recovery_buffer(
    buffer: &PlaybackBuffer,
    source: Option<&Path>,
    layout: Option<&Channels>,
    output: (u32, usize),
    canceled: impl Fn() -> bool,
) -> Result<Option<PlaybackBuffer>, String> {
    let input = (buffer.sample_rate, buffer.channels);
    if input == output {
        return Ok(None);
    }

    // Reuse the source file on format changes so switching devices repeatedly
    // doesn't accumulate resampling or downmix losses. The loaded PCM is the
    // fallback for tracks opened from a drive that has since been disconnected.
    let decoded = source.and_then(|path| decode_track_cancelable(path, &canceled).ok());
    if canceled() {
        return Err(DecodeError::Canceled.to_string());
    }
    let converted = if let Some(decoded) = decoded {
        prepare_playback_buffer(
            decoded.samples,
            (decoded.sample_rate, decoded.channels),
            decoded.channel_layout.as_ref(),
            output,
        )
    } else {
        prepare_playback_buffer(buffer.samples.clone(), input, layout, output)
    };
    converted.map(Some).map_err(|error| error.to_string())
}

/// Consumes the decoded samples: when the device already matches the file's rate
/// and layout both steps hand the same allocation through, so a matching device
/// costs no extra copy of the track at all.
fn prepare_playback_buffer(
    samples: Vec<f32>,
    (input_sample_rate, input_channels): (u32, usize),
    layout: Option<&Channels>,
    (output_sample_rate, output_channels): (u32, usize),
) -> Result<PlaybackBuffer, crate::audio::AudioProcessError> {
    // Perform the rate conversion with the smaller channel count. Expanding
    // early would resample channels that only contain copies or silence.
    let (samples, channels) = if output_channels < input_channels {
        let downmixed = remix_track_channels(samples, input_channels, layout, output_channels);
        (downmixed, output_channels)
    } else {
        (samples, input_channels)
    };
    let resampled = resample_interleaved(samples, channels, input_sample_rate, output_sample_rate)?;
    let remixed = remix_channels(resampled, channels, output_channels);
    Ok(PlaybackBuffer::new(
        remixed,
        output_sample_rate,
        output_channels,
    ))
}

fn configure_style(ctx: &Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = Color32::from_rgb(35, 35, 35);
    visuals.panel_fill = Color32::from_rgb(28, 28, 28);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(55, 55, 55);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(74, 74, 74);
    visuals.widgets.active.bg_fill = Color32::from_rgb(90, 90, 90);
    visuals.selection.bg_fill = Color32::from_rgb(75, 120, 145);
    ctx.set_visuals(visuals);
}

fn texture_from_cover(ctx: &Context, cover: &CoverArt) -> Option<TextureHandle> {
    let max_side = ctx.input(|input| input.max_texture_side);
    if cover.rgba.is_empty()
        || cover.width == 0
        || cover.height == 0
        || cover.width.max(cover.height) as usize > max_side
    {
        return None;
    }

    Some(ctx.load_texture(
        "cover_art",
        ColorImage::from_rgba_unmultiplied(
            [cover.width as usize, cover.height as usize],
            &cover.rgba,
        ),
        TextureOptions::LINEAR,
    ))
}

fn metadata_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(label)
            .size(11.0)
            .color(Color32::from_gray(130)),
    );
    ui.label(RichText::new(value).color(Color32::from_gray(215)));
}

/// One line describing what happens between the file and the audio device.
///
/// The player always follows the device's mix format and deliberately offers no
/// rate override: cpal drives WASAPI in *shared* mode, where Windows converts to
/// the mix format regardless, so asking for anything else would resample twice
/// (ours, then Windows') for strictly worse quality. This read-out makes the
/// resulting path visible — "direct" means the track reaches the device untouched.
pub fn output_path_summary(
    file_rate: u32,
    file_channels: usize,
    device_rate: u32,
    device_channels: usize,
) -> String {
    let rate = if file_rate == device_rate {
        format!("{file_rate} Hz")
    } else {
        format!("{file_rate} -> {device_rate} Hz")
    };

    if file_channels != device_channels {
        format!("{rate} - {file_channels} -> {device_channels} ch")
    } else if file_rate == device_rate {
        format!("{rate} - direct")
    } else {
        format!("{rate} - resampled")
    }
}

fn optional_text(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("-")
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn format_duration(duration: Duration) -> String {
    format_duration_secs(duration.as_secs_f32())
}

fn format_duration_secs(seconds: f32) -> String {
    let total = seconds.max(0.0).round() as u64;
    let minutes = total / 60;
    let seconds = total % 60;
    format!("{minutes}:{seconds:02}")
}

fn format_band(frequency: f32) -> String {
    if frequency >= 1000.0 {
        format!("{}k", (frequency / 1000.0) as u32)
    } else {
        (frequency as u32).to_string()
    }
}

fn fit_size(source: Vec2, bounds: Vec2) -> Vec2 {
    if source.x <= 0.0 || source.y <= 0.0 {
        return bounds;
    }
    let scale = (bounds.x / source.x).min(bounds.y / source.y);
    source * scale
}

fn parse_bpm(value: &str) -> Option<f32> {
    let bpm: f32 = value.trim().parse().ok()?;
    (bpm.is_finite() && bpm > 1.0).then_some(bpm)
}

/// Duration of `bars` bars of 4/4 at `bpm` (4 beats per bar). Negative bars give
/// a negative (rewind) duration.
fn bars_to_seconds(bars: i32, bpm: f32) -> f32 {
    bars as f32 * 4.0 * 60.0 / bpm
}

/// Moving-average over columns; `radius` is in columns (0 leaves it untouched).
fn smooth(values: &[f32], radius: usize) -> Vec<f32> {
    if radius == 0 {
        return values.to_vec();
    }
    let len = values.len();
    (0..len)
        .map(|index| {
            let start = index.saturating_sub(radius);
            let end = index.saturating_add(radius).saturating_add(1).min(len);
            let sum: f32 = values[start..end].iter().sum();
            sum / (end - start) as f32
        })
        .collect()
}

/// Roland-style level color: green (quiet) → yellow → red (clipping), keyed to a
/// fraction of full height so the gradient lines up across bars and the meter.
/// Compact transport control drawn as a glyph — a right-pointing triangle when
/// stopped/paused, two bars when playing — so it reads as a play/pause icon
/// rather than a text button. Styled to match the surrounding buttons.
fn play_pause_button(ui: &mut egui::Ui, playing: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(46.0, 28.0), Sense::click());
    let visuals = *ui.style().interact(&response);
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, visuals.weak_bg_fill);
    painter.rect_stroke(rect, 4.0, visuals.bg_stroke, egui::StrokeKind::Inside);

    let color = visuals.fg_stroke.color;
    let c = rect.center();
    if playing {
        // Pause: two vertical bars.
        let bar = Vec2::new(3.5, 12.0);
        painter.rect_filled(
            Rect::from_center_size(Pos2::new(c.x - 4.0, c.y), bar),
            1.0,
            color,
        );
        painter.rect_filled(
            Rect::from_center_size(Pos2::new(c.x + 4.0, c.y), bar),
            1.0,
            color,
        );
    } else {
        // Play: right-pointing triangle.
        let pts = vec![
            Pos2::new(c.x - 5.0, c.y - 7.0),
            Pos2::new(c.x - 5.0, c.y + 7.0),
            Pos2::new(c.x + 7.0, c.y),
        ];
        painter.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
    }
    response
}

/// Loudest of the analyzer bars feeding display column `d` of `dn` total — a
/// max-pooled downsample. `dn <= arr.len()`, so the bucket always spans >=1 bar.
fn bucket_max(arr: &[f32], d: usize, dn: usize) -> f32 {
    let n = arr.len();
    let lo = d * n / dn;
    let hi = ((d + 1) * n / dn).clamp(lo + 1, n);
    arr[lo..hi].iter().copied().fold(0.0, f32::max)
}

fn level_color(t: f32) -> Color32 {
    let green = Color32::from_rgb(54, 200, 88);
    let yellow = Color32::from_rgb(232, 197, 58);
    let red = Color32::from_rgb(229, 72, 58);
    let t = t.clamp(0.0, 1.0);
    if t < 0.6 {
        lerp_color(green, yellow, t / 0.6)
    } else {
        lerp_color(yellow, red, (t - 0.6) / 0.4)
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Fill `rect` with a vertical gradient (bottom → top) as a two-triangle mesh —
/// the cheapest way to get a smooth gradient in egui.
fn paint_vgradient(painter: &egui::Painter, rect: Rect, bottom: Color32, top: Color32) {
    let mut mesh = Mesh::default();
    let base = mesh.vertices.len() as u32;
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_top(), top);
    mesh.add_triangle(base, base + 1, base + 2);
    mesh.add_triangle(base, base + 2, base + 3);
    painter.add(mesh);
}

/// Traktor-style additive color: low→red, mid→green, high→blue. The band balance
/// sets the hue, the loudest band sets brightness, and unplayed columns dim.
fn spectral_color(low: f32, mid: f32, high: f32, played: bool) -> Color32 {
    let dominant = low.max(mid).max(high).max(1e-4);
    let red = (low / dominant).powf(0.7);
    let green = (mid / dominant).powf(0.7);
    let blue = (high / dominant).powf(0.7);
    let energy = dominant.clamp(0.0, 1.0).sqrt();
    let brightness = (0.35 + 0.65 * energy) * if played { 1.0 } else { 0.45 };
    let scale = 255.0 * brightness;
    Color32::from_rgb(
        (red * scale) as u8,
        (green * scale) as u8,
        (blue * scale) as u8,
    )
}

pub fn initial_file_from_args() -> Option<PathBuf> {
    std::env::args_os().nth(1).and_then(path_from_os)
}

fn path_from_os(value: OsString) -> Option<PathBuf> {
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_to_matching_output_reuses_the_playback_buffer() {
        let buffer = PlaybackBuffer::new(vec![0.5, 0.25], 44_100, 2);

        assert!(
            recovery_buffer(&buffer, None, None, (44_100, 2), || false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reconnect_can_convert_retained_audio_when_the_source_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("removed.wav");
        let buffer = PlaybackBuffer::new(vec![0.5; 441], 44_100, 1);

        let converted = recovery_buffer(&buffer, Some(&path), None, (48_000, 2), || false)
            .unwrap()
            .unwrap();

        assert_eq!(converted.sample_rate, 48_000);
        assert_eq!(converted.channels, 2);
        assert!(converted.frame_count().abs_diff(480) <= 1);
        assert!(
            converted
                .samples
                .as_chunks::<2>()
                .0
                .iter()
                .all(|frame| frame[0] == frame[1])
        );
    }

    #[test]
    fn load_without_an_output_device_keeps_samples_for_the_waveform() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("device unavailable.wav");
        std::fs::write(
            &path,
            b"RIFF\x28\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x44\xac\0\0\x88\x58\x01\0\x02\0\x10\0data\x04\0\0\0\0\x20\0\x40",
        )
        .unwrap();
        let LoadOutcome::Loaded(data) = load_track(path, None, || false) else {
            panic!("valid file should load even without an audio device");
        };
        assert_eq!(data.buffer.samples, [0.25, 0.5]);
    }

    #[test]
    fn opening_a_surround_wav_preserves_center_audio_on_stereo_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("surround.wav");
        let mut wave = b"RIFF".to_vec();
        wave.extend(72_u32.to_le_bytes());
        wave.extend(b"WAVEfmt ");
        wave.extend(40_u32.to_le_bytes());
        wave.extend(0xfffe_u16.to_le_bytes()); // WAVE_FORMAT_EXTENSIBLE
        wave.extend(6_u16.to_le_bytes());
        wave.extend(44_100_u32.to_le_bytes());
        wave.extend(529_200_u32.to_le_bytes());
        wave.extend(12_u16.to_le_bytes());
        wave.extend(16_u16.to_le_bytes());
        wave.extend(22_u16.to_le_bytes());
        wave.extend(16_u16.to_le_bytes());
        wave.extend(0x3f_u32.to_le_bytes()); // FL, FR, FC, LFE, rear left and right
        wave.extend([
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
        wave.extend(b"data");
        wave.extend(12_u32.to_le_bytes());
        wave.extend(
            [0_i16, 0, 16_384, 0, 0, 0]
                .into_iter()
                .flat_map(i16::to_le_bytes),
        );
        std::fs::write(&path, wave).unwrap();

        let LoadOutcome::Loaded(data) = load_track(path, Some((44_100, 2)), || false) else {
            panic!("surround WAV should load");
        };
        let [left, right] = data.buffer.samples[..] else {
            panic!("expected one stereo frame");
        };
        assert!(left > 0.1, "center channel was lost");
        assert_eq!(left, right);
    }
}
