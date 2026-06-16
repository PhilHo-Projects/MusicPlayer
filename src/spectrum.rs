//! Live spectrum analyzer for the visualizer strip.
//!
//! This is purely cosmetic — it runs an FFT over the *output* mix captured by the
//! audio tap and produces per-bar display values. It never touches the stored
//! waveform analysis (which stays amplitude-accurate). One small FFT per UI frame
//! is cheap (~tens of microseconds), so it's safe to run at the repaint rate.

use std::sync::Arc;

use realfft::{RealFftPlanner, RealToComplex, num_complex::Complex};

/// FFT window length. A power of two keeps the transform fast; 2048 at ~44.1 kHz
/// gives ~21 Hz resolution over ~46 ms of context — lively without lagging. The
/// audio tap retains a ring at least this long so a full window is always ready.
pub const FFT_SIZE: usize = 2048;

/// Analyzer resolution: log-spaced frequency bars. The strip downsamples these to
/// however many columns fit at a fixed on-screen pitch, so a wider/fullscreen
/// window shows *more* bars rather than fatter ones. Sized so we don't run out of
/// bars until well past a fullscreen window on a typical monitor.
pub const BAR_COUNT: usize = 192;

// Magnitudes are converted to dB and mapped from this window onto 0..=1. Tuned by
// eye so typical music fills the strip without the noise floor lighting it up.
pub const DB_FLOOR: f32 = -84.0;
pub const DB_CEIL: f32 = -18.0;

// Per-frame envelope (assumes ~30 fps repaint): instant attack, eased release.
// 0.88 ≈ 55% remaining after 1 s — slow enough that bars feel "weighted."
const RELEASE: f32 = 0.88;

/// Visual style for the spectrum strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpectrumDisplayMode {
    /// Vertical bars with small gaps — classic equalizer look.
    Bars,
    /// Smooth filled mountain with a gradient mesh and a stroke outline.
    Line,
}

/// Live, tweakable parameters for *drawing* the spectrum. Changing these is
/// instant — the FFT history is unaffected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpectrumParams {
    pub mode: SpectrumDisplayMode,
    /// Lower dB limit (noise floor). A bar is at height 0 at this level.
    pub db_floor: f32,
    /// Upper dB limit (full height). A bar reaches height 1.0 at this level.
    pub db_ceil: f32,
    /// Show floating peak-hold caps above the bars / line.
    pub peak_caps: bool,
    /// Overall height multiplier applied after the dB map (0.25 – 3.0).
    pub sensitivity: f32,
    /// Mirror the display so the lowest frequency is in the center and the
    /// highest frequencies are at the edges (like MusicBee's analyzer).
    pub symmetric: bool,
}

impl Default for SpectrumParams {
    fn default() -> Self {
        Self {
            mode: SpectrumDisplayMode::Bars,
            db_floor: DB_FLOOR,
            db_ceil: DB_CEIL,
            peak_caps: true,
            sensitivity: 1.0,
            symmetric: true,
        }
    }
}

// Falling peak-hold caps.
// The cap snaps up instantly when a bar rises, holds for PEAK_HOLD_SECS, then
// falls under CAP_GRAVITY acceleration — the "freeze then drop" look.
const PEAK_HOLD_SECS: f32 = 0.65;
const CAP_GRAVITY: f32 = 1.8;

pub struct SpectrumAnalyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// `[start, end)` FFT-bin range feeding each bar.
    edges: Vec<(usize, usize)>,
    /// Smoothed display height per bar, 0..=1.
    bars: Vec<f32>,
    /// Peak-hold cap height per bar, 0..=1.
    peaks: Vec<f32>,
    cap_vel: Vec<f32>,
    /// Remaining hold time before a cap starts falling.
    cap_hold: Vec<f32>,
}

impl SpectrumAnalyzer {
    pub fn new(sample_rate: u32) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let spectrum = fft.make_output_vec();
        let scratch = fft.make_scratch_vec();
        Self {
            fft,
            window: hann_window(FFT_SIZE),
            input: vec![0.0; FFT_SIZE],
            spectrum,
            scratch,
            edges: compute_edges(sample_rate.max(1) as f32),
            bars: vec![0.0; BAR_COUNT],
            peaks: vec![0.0; BAR_COUNT],
            cap_vel: vec![0.0; BAR_COUNT],
            cap_hold: vec![0.0; BAR_COUNT],
        }
    }

    pub fn bars(&self) -> &[f32] {
        &self.bars
    }

    pub fn peaks(&self) -> &[f32] {
        &self.peaks
    }

    /// Analyze the latest output samples (oldest-first; the most recent
    /// [`FFT_SIZE`] are used). `params` supplies the dB window; `dt` drives the
    /// peak-cap fall.
    pub fn update(&mut self, samples: &[f32], params: &SpectrumParams, dt: f32) {
        let take = samples.len().min(FFT_SIZE);
        let pad = FFT_SIZE - take;
        let src = &samples[samples.len() - take..];
        self.input[..pad].fill(0.0);
        for (slot, (&sample, &w)) in self.input[pad..]
            .iter_mut()
            .zip(src.iter().zip(&self.window[pad..]))
        {
            *slot = sample * w;
        }

        if self
            .fft
            .process_with_scratch(&mut self.input, &mut self.spectrum, &mut self.scratch)
            .is_err()
        {
            return;
        }

        let len = self.spectrum.len();
        for bar in 0..BAR_COUNT {
            let (start, end) = self.edges[bar];
            let end = end.min(len);
            let mut mag = 0.0_f32;
            for bin in &self.spectrum[start..end.max(start)] {
                mag = mag.max(bin.norm());
            }
            // Normalize so a full-scale tone lands near 0 dB, then map dB→height.
            let norm = mag / (FFT_SIZE as f32 * 0.5);
            let db = 20.0 * (norm + 1e-9).log10();
            let range = (params.db_ceil - params.db_floor).max(1.0);
            let target = (((db - params.db_floor) / range) * params.sensitivity.clamp(0.25, 3.0))
                .clamp(0.0, 1.0);
            self.bars[bar] = if target >= self.bars[bar] {
                target
            } else {
                self.bars[bar] * RELEASE
            };
            self.update_cap(bar, dt);
        }
    }

    /// Ease everything toward zero when nothing is playing (no FFT needed).
    pub fn decay(&mut self, dt: f32) {
        for bar in 0..BAR_COUNT {
            self.bars[bar] *= RELEASE;
            self.update_cap(bar, dt);
        }
    }

    fn update_cap(&mut self, bar: usize, dt: f32) {
        if self.bars[bar] >= self.peaks[bar] {
            // Bar rose: snap cap up, reset hold timer and fall velocity.
            self.peaks[bar] = self.bars[bar];
            self.cap_vel[bar] = 0.0;
            self.cap_hold[bar] = PEAK_HOLD_SECS;
        } else if self.cap_hold[bar] > 0.0 {
            // Holding: count down, cap stays put.
            self.cap_hold[bar] -= dt;
        } else {
            // Falling under gravity — accelerates so the drop looks snappy.
            self.cap_vel[bar] += CAP_GRAVITY * dt;
            self.peaks[bar] = (self.peaks[bar] - self.cap_vel[bar] * dt).max(self.bars[bar]);
        }
    }
}

fn hann_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            (std::f32::consts::PI * i as f32 / (n as f32 - 1.0))
                .sin()
                .powi(2)
        })
        .collect()
}

/// Log-spaced FFT-bin ranges from ~30 Hz to ~18 kHz (clamped to Nyquist). Each
/// bar always gets at least one bin so low-frequency bars never collapse.
fn compute_edges(sample_rate: f32) -> Vec<(usize, usize)> {
    let bin_hz = sample_rate / FFT_SIZE as f32;
    let max_bin = FFT_SIZE / 2; // valid bins are 0..=max_bin
    let f_min = 30.0_f32;
    let f_max = (sample_rate * 0.5).min(18_000.0).max(f_min * 2.0);

    let mut edges = Vec::with_capacity(BAR_COUNT);
    let mut prev = ((f_min / bin_hz).floor() as usize).clamp(1, max_bin);
    for bar in 0..BAR_COUNT {
        let frac = (bar + 1) as f32 / BAR_COUNT as f32;
        let freq = f_min * (f_max / f_min).powf(frac);
        let end = ((freq / bin_hz).round() as usize).clamp(prev + 1, max_bin + 1);
        edges.push((prev, end));
        prev = end.min(max_bin);
    }
    edges
}
