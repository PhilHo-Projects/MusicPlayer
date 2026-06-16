# MusicPlayer

A native desktop audio player written in Rust with [`egui`](https://github.com/emilk/egui),
built for DJs who keep their library in [Traktor](https://www.native-instruments.com/en/products/traktor/).
It decodes a track, shows its metadata and cover art, draws a tweakable waveform,
and — when a file has been analyzed by Traktor — lights the waveform up
spectrally and turns the transport into beat-accurate bar jumps.

## Features

- **Playback** of MP3, WAV, FLAC, M4A, AAC (decode via `symphonia`, output via `cpal`).
- **10-band equalizer** (biquad peaking filters), docked in a collapsible inspector.
- **Linear volume** fader — direct gain across the full range of travel.
- **Tweakable waveform** — switch peak ↔ RMS reduction, and adjust bar count,
  gamma, height, smoothing and mirroring live.
- **Live spectrum strip** — a log-spaced FFT analyzer (`realfft`) between the
  album art and the waveform.
- **Traktor awareness** — reads `TBPM`/`TKEY` and detects Traktor's
  `PRIV:TRAKTOR4` analysis frame. Analyzed tracks get:
  - **Spectral waveform coloring** (low → red, mid → green, high → blue).
  - **Bar-jump transport** (`±4 / ±8 / ±16 / ±32` bars) instead of `±10s`.

## Build & run

Requires a recent Rust toolchain (edition 2024).

```sh
cargo run --release            # launch empty
cargo run --release -- "path/to/track.mp3"   # open a file on start
```

You can also drag-and-drop a file onto the window, or use **Open**.

```sh
cargo test                     # unit + integration tests
```

## Architecture

| Module | Responsibility |
|---|---|
| `src/main.rs` | eframe bootstrap, window options |
| `src/app.rs` | All UI: top bar, transport, inspector, waveform painting |
| `src/audio.rs` | `cpal` output stream, playback state, EQ processor, resampling |
| `src/decoder.rs` | `symphonia` decode → interleaved `f32` + baked waveform analysis |
| `src/metadata.rs` | `lofty` tag reading, BPM/key, Traktor signature detection |
| `src/waveform.rs` | Per-bin analysis (peak/RMS/3-band) + render parameters |
| `src/spectrum.rs` | Real-time FFT spectrum strip (`realfft`) — cosmetic bars + peak-hold |
| `src/single_instance.rs` | Loopback-socket guard so a second launch reuses the open window |

The waveform pipeline is split in two: a **heavy analysis** baked once at decode
(`WaveformAnalysis` — amplitude-accurate peak, RMS, and low/mid/high band
energies), and **cheap render parameters** (`WaveformParams`) applied every frame.
The stored analysis is never altered by the on-screen controls.

## Stack

`eframe`/`egui` · `cpal` · `symphonia` · `rubato` · `biquad` · `lofty` · `image` · `rfd`
