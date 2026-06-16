# CLAUDE.md

Guidance for working in this repository.

## What this is

A native Rust desktop music player (`eframe`/`egui`) aimed at DJs whose library
is analyzed in Traktor. See `README.md` for the feature list.

## Repository & CI

- GitHub: <https://github.com/PhilHo-Projects/MusicPlayer.git> (org `PhilHo-Projects`).
- `.github/workflows/ci.yml` — fmt check, clippy, test, release build on push/PR
  (Windows runner). Clippy is not yet `-D warnings`; see the note in the workflow.
- `.github/workflows/release.yml` — on a `vX.Y.Z` tag, runs `scripts\package.ps1`
  and attaches the zip to a GitHub Release. To cut a release: bump `version` in
  `Cargo.toml`, commit, then `git tag vX.Y.Z && git push --tags`.

## Build & run on this machine

`cargo` is **not on the Bash tool's PATH**. Use PowerShell and prepend it:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo check    # or test / run
```

For long/GUI commands, capture output to a file and read it back
(`cargo ... *> "$env:TEMP\out.txt"`), since live output can be truncated. The app
holds a lock on `target\debug\music_player.exe` while running — stop the process
(`Get-Process music_player | Stop-Process -Force`) before rebuilding.

Open the app on a known-good fixture: `Example\Red Axes - Salty Dog [OHR001].mp3`
(Traktor-analyzed, so it exercises BPM/key, bar jumps, and spectral coloring).

**Packaging a standalone build:** `pwsh -File scripts\package.ps1`. It builds
`--release` with `+crt-static` (so the single exe needs no Visual C++ redist) and
stages `dist\v<version>\MusicPlayer.exe` + a zip. The Rust binary is otherwise
fully self-contained — no DLLs to bundle. `dist/` is gitignored; bump the version
in `Cargo.toml` to stamp a new subfolder.

## Toolkit note — this is egui/eframe 0.34.3, not the stock API

Two things differ from older/standard egui and will trip you up:

- `eframe::App` is implemented via **`fn ui(&mut self, ui: &mut egui::Ui, frame)`**,
  not `fn update(&mut self, ctx, frame)`.
- Panels use a **unified `egui::Panel`** (`Panel::right(id)`, `Panel::top(id)`, …)
  with `.show_inside(ui, …)`. Use `.default_size`/`.size_range` (not the deprecated
  `default_width`/`width_range`).

Always confirm an API against the crate source in
`~/.cargo/registry/src/*/egui-0.34.3/` before assuming it matches what you remember.

## Architecture map

- `app.rs` — UI + app state (`MusicPlayerApp`). Panels: top bar, **spectrum strip**
  (between album art and waveform), bottom transport (waveform + controls + clip
  meter), right **inspector** (collapsible Track info / Waveform / Equalizer
  sections), central album art.
- `audio.rs` — `AudioEngine` (cpal stream + `Arc<Mutex<EngineShared>>`),
  `PlaybackState`, `EqProcessor` (biquad peaking, `EQ_BANDS_HZ`), channel remix
  and `rubato` resampling. Volume gain is linear (`PlaybackState::gain`); an
  earlier log taper was reverted because it made the bottom half nearly silent.
  `VizTap` captures the final mono mix + pre-clamp peak/clip for the visualizers;
  the UI drains it via `AudioEngine::drain_viz`.
- `spectrum.rs` — `SpectrumAnalyzer`: one `realfft` pass per frame over the tapped
  output → log-spaced bars + peak-hold caps. Purely cosmetic (like `WaveformParams`).
- `decoder.rs` — `decode_track` → `DecodedTrack { samples, …, waveform }`.
- `metadata.rs` — `read_track_info` via `lofty`; BPM/key/Traktor fields.
- `waveform.rs` — analysis + render params (see below).
- `single_instance.rs` — loopback-socket guard: a second launch (double-clicking
  a file) forwards its path to the running window and exits, rather than opening a
  new window. `main.rs` calls `acquire`/`serve`; the UI polls the receiver.

## Key facts (verified against the fixture)

- **Traktor signature**: an ID3 `PRIV` frame whose owner is `TRAKTOR4`. Detected by
  scanning the leading ID3v2 tag for the bytes `TRAKTOR` (`metadata::detect_traktor`).
  This is the gate for spectral coloring and bar jumps.
- **Key**: ID3 `TKEY`, in Traktor's Open-Key notation (e.g. `4m`). Shown verbatim.
- **BPM**: ID3 `TBPM` (`ItemKey::IntegerBpm`). Bar jumps are *relative* (need only
  BPM): `bars * 4 beats * 60 / bpm` seconds. The `PRIV:TRAKTOR4` payload also holds
  the full beatgrid/cue chunks (`DMRT`/`SKHC`/`DOMF`) — not yet parsed; parse it if
  you ever need absolute bar snapping or beat markers.

## Waveform pipeline

Two layers, kept separate on purpose:

1. **Analysis** (`WaveformAnalysis`, baked once in `analyze_waveform` at decode):
   `peak`, `rms`, and `low`/`mid`/`high` band energies (biquad LP 250 / BP 900 /
   HP 4000 over a mono mixdown). Amplitude-accurate; never mutated by the UI.
2. **Render params** (`WaveformParams`, live in `app.rs`): bars, reduction
   (Peak/RMS), gamma, height, smoothing, mirror, color mode. Applied per frame in
   `paint_waveform`.

The user values the waveform staying **amplitude-accurate** — keep new "prettiness"
as draw-time transforms over stored analysis, not edits to the analysis itself.

## Conventions

- Comments explain **why**, not what (match the existing density).
- Tests live in `tests/`; keep `build_waveform`'s contract (peak bins) stable —
  `core_tests` and `decode_tests` depend on it.
