use music_player::spectrum::{BAR_COUNT, FFT_SIZE, SpectrumAnalyzer, SpectrumParams};

const SAMPLE_RATE: u32 = 44_100;

fn tone(freq: f32, amplitude: f32) -> Vec<f32> {
    (0..FFT_SIZE)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            amplitude * (std::f32::consts::TAU * freq * t).sin()
        })
        .collect()
}

fn max_bar(analyzer: &SpectrumAnalyzer) -> f32 {
    analyzer.bars().iter().copied().fold(0.0, f32::max)
}

#[test]
fn silence_leaves_bars_at_zero() {
    let mut analyzer = SpectrumAnalyzer::new(SAMPLE_RATE);
    analyzer.update(&vec![0.0; FFT_SIZE], &SpectrumParams::default(), 0.033);
    assert!(
        max_bar(&analyzer) < 1e-3,
        "silence should not light any bar"
    );
}

#[test]
fn loud_tone_lights_the_strip() {
    let mut analyzer = SpectrumAnalyzer::new(SAMPLE_RATE);
    analyzer.update(&tone(1_000.0, 1.0), &SpectrumParams::default(), 0.033);
    assert!(
        max_bar(&analyzer) > 0.5,
        "a full-scale tone should drive a bar near the top"
    );
}

#[test]
fn tone_energy_is_localized_not_smeared() {
    let mut analyzer = SpectrumAnalyzer::new(SAMPLE_RATE);
    analyzer.update(&tone(1_000.0, 1.0), &SpectrumParams::default(), 0.033);
    let bars = analyzer.bars();
    assert_eq!(bars.len(), BAR_COUNT);
    // A 1 kHz tone should leave the sub-bass bar quiet while a mid bar peaks.
    assert!(
        bars[0] < 0.2,
        "sub-bass bar should stay low for a 1 kHz tone (was {})",
        bars[0]
    );
}

#[test]
fn full_scale_midrange_tone_peaks_near_zero_db_on_the_log_axis() {
    let mut analyzer = SpectrumAnalyzer::new(SAMPLE_RATE);
    let freq = 46.0 * SAMPLE_RATE as f32 / FFT_SIZE as f32;
    let params = SpectrumParams {
        db_ceil: 0.0,
        ..Default::default()
    };
    analyzer.update(&tone(freq, 1.0), &params, 1.0 / 30.0);

    let (index, peak) = analyzer
        .bars()
        .iter()
        .copied()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .unwrap();
    assert!(
        (96..=112).contains(&index),
        "1 kHz should be near the middle of a 30 Hz to 18 kHz log axis"
    );
    assert!(peak > 0.99, "the Hann window's gain should be compensated");
}

#[test]
fn bar_release_depends_on_elapsed_time() {
    let mut a = SpectrumAnalyzer::new(SAMPLE_RATE);
    let mut b = SpectrumAnalyzer::new(SAMPLE_RATE);
    let signal = tone(1_000.0, 1.0);
    a.update(&signal, &SpectrumParams::default(), 1.0 / 30.0);
    b.update(&signal, &SpectrumParams::default(), 1.0 / 30.0);

    a.decay(0.1);
    for _ in 0..3 {
        b.decay(0.1 / 3.0);
    }

    assert!((max_bar(&a) - max_bar(&b)).abs() < 1e-6);
}
