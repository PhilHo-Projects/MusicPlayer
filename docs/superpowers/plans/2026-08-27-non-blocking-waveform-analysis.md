# Non-Blocking Waveform Analysis Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Start playback as soon as the track is decoded, and compute the waveform on a background thread instead of blocking on it — cutting time to first sound on a 2.1 h WAV from 7.36 s to ~1.9 s.

**Architecture:** `decode_track` splits into an unanalyzed decode plus a separate analysis step. The load thread returns a playable buffer immediately; the app hands it to the engine as an `Arc<PlaybackBuffer>`, starts playback, then spawns a thread that analyzes *that same Arc* — so nothing is copied and the zero-copy memory work is preserved. A generation counter stops a superseded load's analysis from overwriting the current track's waveform.

**Tech Stack:** Rust 2024, eframe/egui 0.34.3, symphonia 0.6, cpal 0.18, `std::sync::mpsc` + `std::thread` (no new dependencies).

**Spec:** `docs/superpowers/specs/2026-08-27-progressive-track-loading-design.md` (this plan implements **Phase 1** of the Delivery order section)

## Global Constraints

- `cargo` is not on the Bash tool's PATH. Use PowerShell: `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"` first.
- CI gates that must stay green: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- The app locks `target\release\music_player.exe` while running. Stop it before rebuilding: `Get-Process music_player | Stop-Process -Force`.
- Comments explain **why**, not what. Match existing density.
- `build_waveform`'s contract (peak bins) stays stable — `core_tests` and `decode_tests` depend on it.
- The waveform must stay amplitude-accurate. Do not alter `analyze_waveform`'s maths in this phase.
- Do not add a sample-rate override. cpal is WASAPI shared-mode only.
- No new crate dependencies.
- Real test file for timing: `E:\Traktor recordings\2023-07-29_21h27m49.wav` (1.24 GB, PCM s16, 44.1 kHz stereo, 2.10 h).

---

### Task 1: Split analysis out of the decode

**Files:**
- Modify: `src/decoder.rs` (the `decode_track` function, currently ends at the `analyze_waveform` call)
- Test: `tests/decode_tests.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub const music_player::decoder::WAVEFORM_BINS: usize` (value `2000`)
  - `pub fn music_player::decoder::decode_track_unanalyzed(path: &Path) -> Result<DecodedTrack, DecodeError>` — identical to `decode_track` except `DecodedTrack::waveform` is `WaveformAnalysis::default()` (empty).
  - `decode_track` keeps its exact current signature and behaviour.

- [ ] **Step 1: Write the failing test**

Add to `tests/decode_tests.rs`. Note it uses the existing `write_ramp_wav` helper already in that file:

```rust
#[test]
fn unanalyzed_decode_returns_audio_without_the_waveform() {
    // Phase 1 starts playback before the waveform exists, so the decode step has
    // to be able to skip analysis. Everything except the waveform must be
    // identical to a normal decode.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("unanalyzed.wav");
    write_ramp_wav(&path, 50_000);

    let full = decode_track(&path).expect("WAV should decode");
    let raw = decode_track_unanalyzed(&path).expect("WAV should decode");

    assert!(raw.waveform.is_empty());
    assert_eq!(raw.samples, full.samples);
    assert_eq!(raw.sample_rate, full.sample_rate);
    assert_eq!(raw.channels, full.channels);
    assert_eq!(raw.duration, full.duration);
}
```

Update the import at the top of `tests/decode_tests.rs`:

```rust
use music_player::{
    decoder::{DecodeError, decode_track, decode_track_unanalyzed},
    metadata::read_track_info,
};
```

- [ ] **Step 2: Run test to verify it fails**

Run: `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"; cargo test --test decode_tests unanalyzed`
Expected: FAIL to compile — `unresolved import ... decode_track_unanalyzed`.

- [ ] **Step 3: Write minimal implementation**

In `src/decoder.rs`, add the constant near the top (after the `use` block):

```rust
/// Analysis resolution. Higher than the on-screen pixel width so the renderer can
/// down-sample to the widget size cleanly instead of stretching a coarse buffer.
pub const WAVEFORM_BINS: usize = 2000;
```

Rename the existing `decode_track` to `decode_track_unanalyzed`, and replace its
final block. The current tail reads:

```rust
    let frame_count = samples.len() / channels;
    let duration = Duration::from_secs_f64(frame_count as f64 / sample_rate as f64);
    // Higher than the on-screen pixel width so the renderer can down-sample to
    // the widget size cleanly instead of stretching a coarse buffer. Bakes peak,
    // RMS, and low/mid/high band energies in one pass.
    let waveform = analyze_waveform(&samples, channels, sample_rate, 2000);

    Ok(DecodedTrack {
        path: path.to_path_buf(),
        samples,
        sample_rate,
        channels,
        duration,
        waveform,
    })
}
```

Replace it with:

```rust
    let frame_count = samples.len() / channels;
    let duration = Duration::from_secs_f64(frame_count as f64 / sample_rate as f64);

    Ok(DecodedTrack {
        path: path.to_path_buf(),
        samples,
        sample_rate,
        channels,
        duration,
        waveform: WaveformAnalysis::default(),
    })
}

/// Decode and bake the waveform in one call. Kept for the batch path and for
/// callers that want a fully-formed track; the app's load path decodes without
/// analysis so playback can start first, then analyses on a background thread.
pub fn decode_track(path: &Path) -> Result<DecodedTrack, DecodeError> {
    let mut decoded = decode_track_unanalyzed(path)?;
    decoded.waveform = analyze_waveform(
        &decoded.samples,
        decoded.channels,
        decoded.sample_rate,
        WAVEFORM_BINS,
    );
    Ok(decoded)
}
```

Also change the signature line of the renamed function to:

```rust
pub fn decode_track_unanalyzed(path: &Path) -> Result<DecodedTrack, DecodeError> {
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test decode_tests`
Expected: PASS, 7 tests. The pre-existing `wav_decode_returns_samples_properties_and_waveform` asserts `decoded.waveform.len() == 2000` and must still pass — that proves `decode_track` kept its behaviour.

- [ ] **Step 5: Commit**

```bash
git add src/decoder.rs tests/decode_tests.rs
git commit -m "refactor: separate waveform analysis from decoding"
```

---

### Task 2: Generation gate for background analysis results

**Files:**
- Modify: `src/app.rs` (add near the other small helper types, e.g. above `struct LoadedTrack` at line 43)
- Test: `tests/core_tests.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub struct music_player::app::AnalysisGate` with `Default`, plus:
  - `pub fn begin(&mut self) -> u64` — starts a new load, returns its generation.
  - `pub fn accepts(&self, generation: u64) -> bool` — true only for the newest generation.

**Why this exists:** opening a second file while the first is still analyzing is a normal action (drag-drop, or a double-click forwarded by `single_instance`). Without a gate, the first file's analysis lands on the second file's track and paints the wrong waveform.

- [ ] **Step 1: Write the failing test**

Add to the end of `tests/core_tests.rs`:

```rust
#[test]
fn analysis_gate_rejects_results_from_a_superseded_load() {
    // Opening a second file mid-analysis must not let the first file's waveform
    // land on the second file's track.
    let mut gate = AnalysisGate::default();
    let first = gate.begin();
    let second = gate.begin();

    assert!(!gate.accepts(first));
    assert!(gate.accepts(second));
}

#[test]
fn analysis_gate_accepts_nothing_before_any_load_starts() {
    // Guards the zero value: a default gate must not accept generation 0.
    let gate = AnalysisGate::default();

    assert!(!gate.accepts(0));
}
```

Update the import at the top of `tests/core_tests.rs`:

```rust
use music_player::{
    app::{AnalysisGate, output_path_summary},
    audio::{EQ_BANDS_HZ, EqSettings, clamp_seek_seconds},
    decoder::is_supported_extension,
    metadata::{TrackInfo, metadata_fallback_from_path},
    waveform::build_waveform,
};
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test core_tests analysis_gate`
Expected: FAIL to compile — `unresolved import ... AnalysisGate`.

- [ ] **Step 3: Write minimal implementation**

In `src/app.rs`, above `struct LoadedTrack`:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test core_tests`
Expected: PASS, 12 tests.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs tests/core_tests.rs
git commit -m "feat: add generation gate for background analysis results"
```

---

### Task 3: Lock the waveform against the input change

**Files:**
- Test: `tests/decode_tests.rs`

**Interfaces:**
- Consumes: `WAVEFORM_BINS` and `decode_track` from Task 1; `PlaybackBuffer::new` (existing, `src/audio.rs:155`).
- Produces: no production code — this is a guard test.

**Note on TDD:** this is a **characterization test**, not red-green. It passes before the change and must still pass after. Its whole job is to prove that Task 4's behaviour change — analyzing the playback buffer instead of the decoded samples — is a no-op on a rate-matched device. Do not skip it: it is the only thing standing between this refactor and a silently different waveform.

- [ ] **Step 1: Write the guard test**

Add to `tests/decode_tests.rs`:

```rust
#[test]
fn analysis_from_the_playback_buffer_matches_the_decode_time_waveform() {
    // Phase 1 moves analysis off the decode thread, where it reads the buffer the
    // engine already holds — that is post-resample data. On a device whose rate
    // and layout match the file (the common case, and this machine's) it is the
    // same samples, so the drawn waveform must not change at all.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("equivalence.wav");
    write_ramp_wav(&path, 50_000);

    let decoded = decode_track(&path).expect("WAV should decode");
    let buffer = PlaybackBuffer::new(
        decoded.samples.clone(),
        decoded.sample_rate,
        decoded.channels,
    );

    let from_buffer = analyze_waveform(
        &buffer.samples,
        buffer.channels,
        buffer.sample_rate,
        WAVEFORM_BINS,
    );

    assert_eq!(from_buffer.peak, decoded.waveform.peak);
    assert_eq!(from_buffer.rms, decoded.waveform.rms);
    assert_eq!(from_buffer.low, decoded.waveform.low);
    assert_eq!(from_buffer.mid, decoded.waveform.mid);
    assert_eq!(from_buffer.high, decoded.waveform.high);
}
```

Update the import at the top of `tests/decode_tests.rs` to:

```rust
use music_player::{
    audio::PlaybackBuffer,
    decoder::{DecodeError, WAVEFORM_BINS, decode_track, decode_track_unanalyzed},
    metadata::read_track_info,
    waveform::analyze_waveform,
};
```

- [ ] **Step 2: Run the test and confirm it passes**

Run: `cargo test --test decode_tests analysis_from_the_playback_buffer`
Expected: PASS. If it fails, stop — the assumption behind Task 4 is wrong and the plan needs revisiting.

- [ ] **Step 3: Commit**

```bash
git add tests/decode_tests.rs
git commit -m "test: pin waveform equivalence between decode and playback buffer"
```

---

### Task 4: Play first, analyze after

**Files:**
- Modify: `src/audio.rs:525-527` (`AudioEngine::load`)
- Modify: `src/app.rs` — imports; `MusicPlayerApp` fields (around line 26-40); `MusicPlayerApp::new` initializers (around line 145-155); `load_track` (around line 1096); `poll_load` (around line 192); `paint_waveform` (line 591-594); the `ui` method's poll calls (line 286-287)
- Test: manual timing on the real file (see Step 7)

**Interfaces:**
- Consumes: `decode_track_unanalyzed` + `WAVEFORM_BINS` (Task 1), `AnalysisGate` (Task 2).
- Produces: no new public API.

- [ ] **Step 1: Make the engine hand back a shareable buffer**

In `src/audio.rs`, change `AudioEngine::load` (currently lines 525-527) from:

```rust
    pub fn load(&self, track: PlaybackBuffer) {
        self.with_shared(|shared| shared.playback.load(Arc::new(track)));
    }
```

to:

```rust
    /// Takes an `Arc` so the caller can keep a handle and analyze the same buffer
    /// on a background thread — the waveform is computed from the audio the engine
    /// is already playing rather than from a second copy of the track.
    pub fn load(&self, track: Arc<PlaybackBuffer>) {
        self.with_shared(|shared| shared.playback.load(track));
    }
```

`with_shared` takes `FnOnce` (`src/audio.rs:574`), so moving the `Arc` into the closure compiles.

- [ ] **Step 2: Add the app state**

In `src/app.rs`, `Arc` is **not** currently imported. Replace the first `use` block (lines 1-6) with:

```rust
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    sync::mpsc::{Receiver, TryRecvError},
    time::Duration,
};
```

Then in the `use crate::{...}` block (lines 13-22), replace the `decoder` and `waveform` lines:

```rust
    decoder::{DecodedTrack, WAVEFORM_BINS, decode_track_unanalyzed},
```

```rust
    waveform::{ColorMode, ReductionMode, WaveformAnalysis, WaveformParams, analyze_waveform},
```

Add two fields to `struct MusicPlayerApp`:

```rust
    pending_analysis: Option<Receiver<(u64, WaveformAnalysis)>>,
    analysis_gate: AnalysisGate,
```

And initialize them in `MusicPlayerApp::new`, alongside `pending_load: None`:

```rust
            pending_analysis: None,
            analysis_gate: AnalysisGate::default(),
```

- [ ] **Step 3: Decode without analyzing**

In `load_track` (around line 1097), change:

```rust
    let mut decoded = match decode_track(&path) {
```

to:

```rust
    let mut decoded = match decode_track_unanalyzed(&path) {
```

- [ ] **Step 4: Start playback, then spawn analysis**

In `poll_load`, replace the `LoadOutcome::Loaded` arm's engine block. It currently reads:

```rust
                if let (Ok(engine), Some(buffer)) = (&self.audio, buffer) {
                    engine.load(buffer);
                    engine.play();
                }
```

Replace with:

```rust
                if let (Ok(engine), Some(buffer)) = (&self.audio, buffer) {
                    let buffer = Arc::new(buffer);
                    engine.load(Arc::clone(&buffer));
                    engine.play();

                    // Analysis no longer gates playback. It reads the very buffer
                    // the engine is playing, so the track isn't duplicated, and
                    // the result is stamped with a generation so a load started
                    // while this one is still running can't be overwritten by it.
                    // Note this is *post*-resample audio: identical to the decoded
                    // samples on a rate-matched device (the normal case), and the
                    // resampled stream otherwise — envelope-equivalent either way.
                    let generation = self.analysis_gate.begin();
                    let (sender, receiver) = std::sync::mpsc::channel();
                    self.pending_analysis = Some(receiver);
                    std::thread::spawn(move || {
                        let analysis = analyze_waveform(
                            &buffer.samples,
                            buffer.channels,
                            buffer.sample_rate,
                            WAVEFORM_BINS,
                        );
                        let _ = sender.send((generation, analysis));
                    });
                }
```

- [ ] **Step 5: Apply the finished analysis**

Add this method to `impl MusicPlayerApp`, next to `poll_load`:

```rust
    /// Apply a finished background waveform analysis, if one is ready. A result
    /// from a superseded load is dropped rather than painted onto the track that
    /// replaced it.
    fn poll_analysis(&mut self) {
        let Some(receiver) = &self.pending_analysis else {
            return;
        };
        match receiver.try_recv() {
            Ok((generation, analysis)) => {
                self.pending_analysis = None;
                if self.analysis_gate.accepts(generation)
                    && let Some(track) = &mut self.track
                {
                    track.decoded.waveform = analysis;
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.pending_analysis = None,
        }
    }
```

And call it in the `ui` method, right after `self.poll_load(&ctx);` (line 287):

```rust
        self.poll_analysis();
```

- [ ] **Step 6: Show that analysis is in flight**

In `paint_waveform` (lines 591-594), replace:

```rust
        let analysis = &track.decoded.waveform;
        if analysis.is_empty() {
            return;
        }
```

with:

```rust
        let analysis = &track.decoded.waveform;
        if analysis.is_empty() {
            // The track is already playing at this point — the waveform is still
            // being analyzed on a background thread.
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Analyzing waveform\u{2026}",
                egui::TextStyle::Button.resolve(ui.style()),
                Color32::from_gray(120),
            );
            return;
        }
```

- [ ] **Step 7: Verify the full suite and the CI gates**

Run each and confirm:

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Expected: fmt exit 0, clippy exit 0, all tests pass (34 total).

- [ ] **Step 8: Measure it on the real file**

Stop any running instance, build release, and launch on the 2.1 h WAV:

```bash
powershell -c "Get-Process music_player -EA SilentlyContinue | Stop-Process -Force"
```

```bash
cargo build --release
```

```bash
./target/release/music_player.exe "E:\Traktor recordings\2023-07-29_21h27m49.wav"
```

Confirm by observation:
- Audio starts in roughly **2 seconds**, not 7.4 (baseline for this file: 7.36 s).
- The waveform area shows "Analyzing waveform…" and then the full waveform appears a few seconds later.
- The seek bar and Duration in Track info are correct from the start.
- Track info still shows `44100 Hz - direct` in the Output row.
- Peak memory stays ~2.5 GB (Task Manager) — analysis must not duplicate the track.

Then open a second file while the first is still analyzing, and confirm the first file's waveform never appears on the second track.

- [ ] **Step 9: Commit**

```bash
git add src/app.rs src/audio.rs
git commit -m "feat: start playback before the waveform is analyzed"
```

---

### Task 5: Record the new behaviour

**Files:**
- Modify: `CLAUDE.md` (the `app.rs` and `decoder.rs` bullets in the Architecture map)

**Interfaces:**
- Consumes: everything above.
- Produces: documentation only.

- [ ] **Step 1: Update the architecture map**

In `CLAUDE.md`, change the `decoder.rs` bullet from:

```
- `decoder.rs` — `decode_track` → `DecodedTrack { samples, …, waveform }`.
```

to:

```
- `decoder.rs` — `decode_track_unanalyzed` → `DecodedTrack` with an empty
  waveform; `decode_track` adds the analysis on top. The app's load path uses the
  unanalyzed form so playback can start before the waveform exists.
```

And add to the `app.rs` bullet:

```
  **Loading is two-stage**: the load thread decodes and hands the engine an
  `Arc<PlaybackBuffer>`, playback starts immediately, then a second thread
  analyzes *that same Arc* (so the track is never duplicated) and posts the
  waveform back via `poll_analysis`. Results carry an `AnalysisGate` generation
  so a superseded load can't paint its waveform onto the track that replaced it.
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: describe the two-stage load path"
```

---

## Done when

- `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo test` all pass.
- Time to first sound on `2023-07-29_21h27m49.wav` is ~2 s, down from 7.36 s.
- Peak memory on that file is unchanged at ~2.5 GB.
- The waveform, once it appears, is identical to what the current build draws.

## Explicitly not in this phase

Chunked progressive playback (spec sections 1-7) and the waveform cache (section
8). Those are Phases 2 and 3, each with its own plan. In particular this phase
does **not** improve MP3 loads much — decode still dominates there (13.8 s to
~8.1 s). That is expected; Phase 2 is what takes both formats under a second.
