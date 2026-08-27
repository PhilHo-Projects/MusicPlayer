# Progressive track loading

**Date:** 2026-08-27
**Status:** approved for planning

## Problem

Opening a long DJ set blocks until the entire file is decoded and analyzed.
Measured on two real files (release build, after the zero-copy work landed):

| stage | 2.10 h WAV (real recording) | 1.97 h MP3 |
| --- | --- | --- |
| decode | 1.87 s | ~8.1 s |
| waveform analysis | **5.48 s (75%)** | ~5.5 s (40%) |
| **time to first sound** | **7.36 s** | **13.8 s** |
| peak memory | 2.49 GB | 2.39 GB |

The WAV is `E:\Traktor recordings\2023-07-29_21h27m49.wav` — 1.24 GB, PCM s16,
44.1 kHz stereo — i.e. what Traktor actually records and what prompted this work.

**The two formats have different bottlenecks, and this drives the delivery
order.** For WAV, decoding is nearly free and the waveform analysis is three
quarters of the wait; simply not blocking on it would cut time-to-first-sound
from 7.36 s to ~1.9 s without touching the audio thread at all. For MP3, decode
dominates and only chunked playback gets it under a second.

Nothing plays during that wait. MusicBee feels instant on the same file
because it streams through BASS: it decodes a fraction of a second ahead of the
playhead, so time-to-first-sound is independent of track length. It buys that by
never holding the track — and therefore never having a whole-track waveform to
draw for free.

We want MusicBee's *responsiveness* while keeping the whole-track waveform this
player exists for.

## Goal

Time to first sound under one second regardless of track length, with the
waveform painting in left-to-right behind it.

## Non-goals

- **Reducing memory.** A two-hour set stays ~2.4 GB resident. This work changes
  *when* audio becomes playable, not how much of it we hold. Real streaming is
  the only fix for memory and it costs the whole-track waveform.
- **Faster total decode.** Total wall-clock stays ~14 s; it just happens behind
  playback instead of in front of it.
- **A rate override.** Settled previously: cpal is WASAPI shared-mode only, so
  anything but the device mix format resamples twice. See `CLAUDE.md`.

## Architecture

### 1. Chunked playback buffer

`PlaybackBuffer` stops being a flat `Vec<f32>` and becomes an append-only list of
fixed-size chunks:

```rust
pub const CHUNK_FRAMES: usize = 1 << 15; // 32768 frames, ~0.74 s at 44.1 kHz

pub struct PlaybackBuffer {
    chunks: Vec<Arc<[f32]>>,  // CHUNK_FRAMES * channels each, except the last
    frames_ready: usize,      // decoded and appended so far
    expected_frames: usize,   // from the demuxer, known before decoding starts
    complete: bool,
    sample_rate: u32,
    channels: usize,
}
```

The buffer lives inside `EngineShared`, which the audio callback **already**
locks on every call (`fill_output_f32`, audio.rs:583). The decode thread appends
under that same lock — roughly two acquisitions per second, each held only long
enough to push an `Arc`. This deliberately adds no new locking discipline to the
real-time path.

`PlaybackState::track` changes from `Option<Arc<PlaybackBuffer>>` to
`Option<PlaybackBuffer>`; the per-callback `Arc` clone that avoided a borrow
conflict is replaced by one cheap `Arc<[f32]>` clone per chunk crossed, hoisted
out of the per-frame loop.

`PlaybackBuffer::new(samples, rate, channels)` is retained, building a complete
buffer by splitting a flat `Vec` — this keeps the batch path and the existing
tests working unchanged.

### 2. Three distinct frame counts

The current code has one `frame_count()` doing two jobs. They now differ, and
conflating them is the most likely source of bugs:

| quantity | source | used for |
| --- | --- | --- |
| `expected_frames` | demuxer `num_frames` | duration, seek-bar width, end-of-track |
| `frames_ready` | chunks appended | how far playback may read |
| `cursor_frame` | playback | current position |

Behaviour at the boundaries:

- `cursor >= expected_frames` -> transport stops. True end of track.
- `frames_ready <= cursor < expected_frames` -> **underrun**: emit silence and
  stay `Playing`. Decode runs ~150x realtime so this should never occur, but it
  must degrade to a gap rather than a false end-of-track.
- Seeks clamp to `frames_ready`. The undecoded tail renders greyed in the
  waveform so the limit is visible rather than mysterious.

The seek bar is full-width from the first frame, because `expected_frames` is
known before decoding starts.

### 3. Streaming decode

`decoder` gains a streaming entry point; `decode_track` is reimplemented on top
of it so there is exactly one decode path:

```rust
pub struct DecodeStream { /* format reader, decoder, track id */ }

impl DecodeStream {
    pub fn open(path: &Path) -> Result<(TrackHeader, Self), DecodeError>;
}

pub struct TrackHeader {
    pub sample_rate: u32,
    pub channels: usize,
    pub num_frames: Option<u64>,
}

impl Iterator for DecodeStream {
    type Item = Result<Vec<f32>, DecodeError>; // interleaved, packet-sized
}
```

The header is available before any audio is decoded, which is what lets the
buffer preallocate and the seek bar size itself immediately.

**Fallback:** when `num_frames` is `None` we cannot size the buffer or bin the
waveform, so the whole load falls back to today's batch path. Behaviour is then
exactly what ships now. This is one branch, and it is rare (WAV always reports a
count; MP3 does via Xing/VBRI).

### 4. Load cancellation

Today an in-flight load is discarded implicitly: `load_path` replaces the mpsc
receiver, so a stale result simply fails to send. That no longer works once the
decode thread writes *into the engine* — a superseded thread would keep appending
its chunks into the newly loaded track's buffer.

Each load therefore carries a generation id, held in `EngineShared` and bumped on
every `load_path`. The decode thread re-checks it while holding the lock, before
each append, and exits when superseded. Opening a second file mid-decode is a
normal action (drag-drop, double-click via the single-instance forwarder), not an
edge case, so this is core behaviour rather than a hardening detail.

### 5. When playback starts

`poll_load` currently calls `engine.play()` once the whole track is ready. That
moves to: the engine begins playing as soon as the **first chunk** is appended,
which is the entire point of the change. The transport shows `Playing` from that
moment; `expected_frames` already gives it a full-width seek bar, so the UI does
not visibly reconfigure itself as decoding proceeds.

### 6. Streaming resample

Chunked playback requires chunked resampling when the device rate differs from
the file. rubato's `Fft` with `FixedSync::Both` is designed for this: fixed input
size in, fixed output size out, state carried across calls.

Pipeline per decoded packet:

```
decoded packet -> staging buffer
  -> (rate differs?) resampler.process_into_buffer, else pass through
  -> remix_channels (stateless, chunkable as-is)
  -> split into CHUNK_FRAMES pieces -> PlaybackBuffer::push_chunk
```

At EOF the resampler is flushed and the partial tail chunk is pushed before
`finish()`.

On the identity path (rate and layout both match — the current rig) staging
passes straight through with no resampling and no remix work, exactly as it does
today.

### 7. Progressive waveform analysis

A stateful analyzer consumes the same decoded chunks that feed the resampler, so
it sees **pre-resample** samples at the file's own rate — identical input to
today's `analyze_waveform`, therefore identical output.

```rust
pub struct WaveformAnalyzer { /* accumulators, filters, frame cursor */ }

impl WaveformAnalyzer {
    pub fn new(sample_rate: u32, channels: usize, expected_frames: usize, bins: usize) -> Self;
    pub fn push(&mut self, samples: &[f32]);
    pub fn snapshot(&self) -> WaveformAnalysis; // partial, for the live UI
    pub fn finish(self) -> WaveformAnalysis;
}
```

Bin boundaries derive from `expected_frames`, known up front. The three band
filters carry state across chunks, which is what makes progressive output match
batch output exactly.

**Exactness.** When `num_frames` is accurate — always, for WAV; nearly always for
MP3 — output is bit-identical to today's batch analysis, pinned by test. When the
header is off by a small delta, the final bins absorb the drift: sub-bin
magnitude, visually imperceptible, but *not* bit-identical. Documented rather
than papered over.

**Rejected alternative.** Accumulating into fine-grained fixed-size buckets and
recombining at completion would remove the dependency on `num_frames`. It was
rejected because bucket boundaries do not align with bin boundaries, so
recombining is not exact for either peak (max) or RMS — it trades a rare,
bounded inaccuracy for a permanent one.

**Edge case.** `expected_frames < bins` (a file shorter than ~0.05 s) makes
`build_waveform`'s ranges overlap, which a forward-only cursor cannot reproduce.
Such files take the batch fallback.

### 8. Waveform cache

Keyed on `path + file size + mtime`, storing the five bin arrays — ~40 KB per
track — under `%APPDATA%\MusicPlayer\waveforms\`.

Scope note: the cache eliminates the 5.5 s analysis, **not** the 8 s decode.
Since progressive playback already makes first sound instant, its real effect is
that reopening a known set shows a fully drawn waveform immediately instead of
watching it paint. Corrupt or unreadable entries are ignored and recomputed;
eviction is LRU above a fixed entry count.

## Delivery order

Revised after measuring the real WAV, which showed analysis — not decode — is 75%
of that file's load. Ordered by value per unit of risk:

**Phase 1 — stop blocking on the waveform.** No chunked buffer, no change to the
audio callback. Decode as today, hand the buffer to the engine, start playing,
and run `analyze_waveform` on a background thread that reads the already-shared
`Arc<PlaybackBuffer>`. Post a waveform update when it finishes.

- Time to first sound: 7.36 s -> ~1.9 s (WAV), 13.8 s -> ~8.1 s (MP3).
- Memory-neutral: analysis reads the buffer the engine already holds, so the
  samples are not duplicated and the zero-copy work is preserved.
- One documented behaviour change: analysis then sees *post*-resample audio. On a
  rate-matched device (the current rig, and the common case) that is byte-for-byte
  the same input as today, so output is identical. When rates differ it analyses
  the resampled stream instead — envelope-equivalent, not bit-identical. Tests
  assert the identity case; the resampled case is documented.
- Risk: low. No real-time-thread changes.

**Phase 2 — chunked progressive playback.** Sections 1-7. Takes first sound under
a second on *both* formats and makes the waveform paint in progressively rather
than appearing at once. This is where the audio-thread work and its tests live.

**Phase 3 — cache.** Section 8. Re-prioritised upward by the measurement: it
removes the 5.48 s analysis, which on the real WAV is 75% of the load. Reopening a
known set becomes ~1.9 s *with* a fully drawn waveform.

Phase 1 is the plan to write now. Phases 2 and 3 get their own plans once Phase 1
is proven on real sets — at which point the measured benefit of Phase 2 can be
re-checked against how it actually feels.

## Testing

The audio callback is a real-time path and the analyzer has an exactness
guarantee, so both get pinned hard:

1. Chunk indexing resolves correctly across chunk boundaries, including a final
   partial chunk.
2. Underrun (`cursor >= frames_ready`) emits silence and keeps transport
   `Playing`.
3. Reaching `expected_frames` stops transport exactly once.
4. Seeks clamp to `frames_ready`, not `expected_frames`.
5. `PlaybackBuffer::new(flat_vec)` yields a buffer that reads back identically to
   the flat original — keeps existing playback tests meaningful.
6. **Progressive analysis is bit-identical to batch `analyze_waveform`** across
   several frame counts, chunk sizes, and channel counts, including counts that
   are not multiples of the chunk size.
7. Streaming resample output matches batch `resample_interleaved` within a stated
   tolerance (FFT chunk boundaries make exact equality the wrong assertion).
8. Cache round-trips; invalidates on size or mtime change; a corrupt entry is
   ignored rather than fatal.
9. Existing zero-copy guards stay green — the batch fallback must keep its
   single-allocation property.

## Risks

| risk | mitigation |
| --- | --- |
| Glitches/underruns on the real-time thread | Append under the lock the callback already takes; no new locks. Underrun degrades to silence, never to a false stop. |
| Waveform silently changes appearance | Bit-identical test vs. batch analysis; drift case documented and bounded. |
| Streaming resample drifts from batch output | Tolerance test; identity path bypasses it entirely. |
| `num_frames` missing or wrong | Missing -> batch fallback. Wrong -> bounded sub-bin drift. |
| Scope creep into a streaming rewrite | Explicit non-goal. Memory profile is unchanged by design. |

## Resolved: the file that prompted this

`E:\Traktor recordings\2023-07-29_21h27m49.wav` — 1.24 GB, PCM s16, 44.1 kHz
stereo, 2.10 h — decodes to 2.49 GB and peaks at 2.49 GB, confirmed against the
OS. Memory is **not** the binding constraint for this file, so the earlier worry
about a ~13 GB decode does not apply and this design is aimed at the right
problem.

Two consequences worth carrying into implementation:

- The zero-copy work already covers this file's memory profile. Before it, the
  same load would have peaked near 7.5 GB.
- Because it is PCM, decoding costs almost nothing (1.87 s) and the waveform
  analysis is the real wait. That is what moved non-blocking analysis ahead of
  chunked playback in the delivery order.
