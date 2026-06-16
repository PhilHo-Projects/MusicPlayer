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
