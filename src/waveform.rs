use biquad::{Biquad, Coefficients, DirectForm1, Q_BUTTERWORTH_F32, ToHertz, Type};

/// Peak (max absolute amplitude) per bin, normalized to 0..=1. This is the
/// amplitude-accurate reduction — it traces the true peak envelope, which is why
/// loud masters render as a near-solid block.
pub fn build_waveform(samples: &[f32], channels: usize, bin_count: usize) -> Vec<f32> {
    if samples.is_empty() || channels == 0 || bin_count == 0 {
        return Vec::new();
    }

    let frame_count = samples.len() / channels;
    if frame_count == 0 {
        return vec![0.0; bin_count];
    }

    (0..bin_count)
        .map(|bin| {
            let start_frame = bin * frame_count / bin_count;
            let end_frame = ((bin + 1) * frame_count / bin_count).max(start_frame + 1);
            let end_frame = end_frame.min(frame_count);

            let mut peak = 0.0_f32;
            for frame in start_frame..end_frame {
                let sample_offset = frame * channels;
                let mut frame_peak = 0.0_f32;
                for channel in 0..channels {
                    frame_peak = frame_peak.max(samples[sample_offset + channel].abs());
                }
                peak = peak.max(frame_peak);
            }

            peak.clamp(0.0, 1.0)
        })
        .collect()
}

/// How a column of samples is collapsed into one drawn value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReductionMode {
    /// Loudest sample in the bin — accurate, but loud music fills the height.
    Peak,
    /// Average energy in the bin — shorter and reveals the track's dynamics.
    Rms,
}

/// How waveform columns are colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    /// Single played/unplayed color.
    Solid,
    /// Traktor-style additive RGB from the low/mid/high band energies.
    Spectral,
}

/// Live, tweakable parameters for *drawing* the waveform. These never touch the
/// stored analysis (which stays amplitude-accurate) — they only reshape the draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformParams {
    /// Number of drawn bars across the width.
    pub bins: usize,
    pub mode: ReductionMode,
    /// Vertical curve applied to each bar (`< 1.0` lifts quiet parts).
    pub gamma: f32,
    /// Overall amplitude multiplier.
    pub height_scale: f32,
    /// Moving-average radius over columns (0 = off).
    pub smoothing: usize,
    /// Mirror around the centerline (vs. baseline-up bars).
    pub mirror: bool,
    pub color_mode: ColorMode,
}

impl Default for WaveformParams {
    fn default() -> Self {
        Self {
            bins: 1100,
            mode: ReductionMode::Rms,
            gamma: 0.65,
            height_scale: 1.4,
            smoothing: 1,
            mirror: true,
            color_mode: ColorMode::Spectral,
        }
    }
}

/// Per-bin analysis baked once at decode time. All vectors share the same length.
/// `peak`/`rms` are the two reductions; `low`/`mid`/`high` are band energies used
/// for Traktor-style spectral coloring.
#[derive(Clone, Debug, Default)]
pub struct WaveformAnalysis {
    pub peak: Vec<f32>,
    pub rms: Vec<f32>,
    pub low: Vec<f32>,
    pub mid: Vec<f32>,
    pub high: Vec<f32>,
}

impl WaveformAnalysis {
    pub fn len(&self) -> usize {
        self.peak.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peak.is_empty()
    }

    /// The per-bin amplitude array for the chosen reduction.
    pub fn amplitude(&self, mode: ReductionMode) -> &[f32] {
        match mode {
            ReductionMode::Peak => &self.peak,
            ReductionMode::Rms => &self.rms,
        }
    }
}

/// One-pass biquad filter at `f0`, or `None` if the coefficients are invalid for
/// this sample rate.
fn band_filter(filter_type: Type<f32>, sample_rate: f32, f0: f32) -> Option<DirectForm1<f32>> {
    let f0 = f0.clamp(20.0, sample_rate * 0.45);
    Coefficients::<f32>::from_params(filter_type, sample_rate.hz(), f0.hz(), Q_BUTTERWORTH_F32)
        .ok()
        .map(DirectForm1::<f32>::new)
}

/// Build the full per-bin analysis: peak, RMS, and low/mid/high band energies.
/// The band split runs three biquads continuously over a mono mixdown and
/// accumulates RMS per bin — done once on the load thread.
pub fn analyze_waveform(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    bin_count: usize,
) -> WaveformAnalysis {
    let peak = build_waveform(samples, channels, bin_count);
    if peak.is_empty() || channels == 0 {
        return WaveformAnalysis::default();
    }

    let bins = peak.len();
    let frame_count = samples.len() / channels;
    if frame_count == 0 {
        let zeros = vec![0.0; bins];
        return WaveformAnalysis {
            peak,
            rms: zeros.clone(),
            low: zeros.clone(),
            mid: zeros.clone(),
            high: zeros,
        };
    }

    let fs = sample_rate.max(1) as f32;
    let mut low = band_filter(Type::LowPass, fs, 250.0);
    let mut mid = band_filter(Type::BandPass, fs, 900.0);
    let mut high = band_filter(Type::HighPass, fs, 4000.0);

    let mut acc_rms = vec![0.0_f64; bins];
    let mut acc_low = vec![0.0_f64; bins];
    let mut acc_mid = vec![0.0_f64; bins];
    let mut acc_high = vec![0.0_f64; bins];
    let mut counts = vec![0_u32; bins];

    for frame in 0..frame_count {
        let offset = frame * channels;
        let mut mono = 0.0_f32;
        for channel in 0..channels {
            mono += samples[offset + channel];
        }
        mono /= channels as f32;

        let bin = (frame * bins / frame_count).min(bins - 1);
        acc_rms[bin] += (mono * mono) as f64;
        if let Some(filter) = low.as_mut() {
            let value = filter.run(mono);
            acc_low[bin] += (value * value) as f64;
        }
        if let Some(filter) = mid.as_mut() {
            let value = filter.run(mono);
            acc_mid[bin] += (value * value) as f64;
        }
        if let Some(filter) = high.as_mut() {
            let value = filter.run(mono);
            acc_high[bin] += (value * value) as f64;
        }
        counts[bin] += 1;
    }

    let finish = |acc: &[f64]| -> Vec<f32> {
        acc.iter()
            .zip(&counts)
            .map(|(sum, &count)| {
                if count == 0 {
                    0.0
                } else {
                    ((sum / count as f64).sqrt() as f32).clamp(0.0, 1.0)
                }
            })
            .collect()
    };

    WaveformAnalysis {
        peak,
        rms: finish(&acc_rms),
        low: finish(&acc_low),
        mid: finish(&acc_mid),
        high: finish(&acc_high),
    }
}
