# Key detection: reusable engine and incremental analysis

## Scope

Implements the approved first optimization step: reuse the existing pinned libkeyfinder engine, analyze new audio incrementally, retain the UI/IPC contract and existing confirmation thresholds, and establish reproducible comparison tooling.

No frontend source, audio capture source, player-control source, Auto-Tune integration, vendor library source, dependency versions, or release binary was changed. This is a debug build, not a release or an accuracy-equivalence claim against Auto-Key.

## Implementation

- `src-tauri/src/key_detection/stream.rs`: one worker-local `RollingDetector`, validated cursor/epoch ownership and finite 48 kHz stereo input. Normally feeds only the newly appended frames. Duplicate cursors cannot yield another candidate.
- `src-tauri/native/keyfinder_bridge.cpp`: owned native context retains the original KeyFinder factories and two FFT workspaces. Completed chroma hops are trimmed; preview finalization copies the bounded unprocessed tail instead of adding padding to the live stream.
- `src-tauri/src/key_detection/engine.rs`: shared input validation and mono-profile calculation. The original batch `detect` remains the reference and nonlinear-clipping fallback.
- `src-tauri/src/key_detection/mod.rs`: worker reuses the detector; identity checks remain before and after native analysis. The nominal one-second cadence now accounts for compute time instead of adding a full second after every computation. Long computations still get a 50 ms yielding floor and never create a backlog.

### Continuity and lifecycle

Source, track, media target generation, capture generation, cursor rewind, missed overlap, or channel-selection changes reseed the rolling analysis. Idle/unready capture and errors invalidate worker continuity. A native error is still `unavailable`; silent successful windows clear only pending votes and retain a confirmed same-track key, as before. The stabilizer itself is unchanged: at least six seconds of PCM, three consistent distinct windows to confirm, five to replace.

`RollingDetector::reset` invalidates logical continuity; it does not reconstruct FFT plans. The next native feed clears the prior audio/chroma workspace before reseeding. Until that feed or destruction, the old native tail remains bounded and cannot be classified/published. Drop releases the context under the native mutex. This is not a secure-memory-erasure guarantee.

### Normalization

RMS floor `5e-5`, target RMS `0.10`, maximum gain `64x`, anti-phase selection and input immutability are retained. Whole-window gain is applied to preview chroma, avoiding independently normalized chunks with inconsistent relative weighting. If that gain would clip a sample, use the original batch normalization/clamp and invalidate incremental continuity. The next ordinary window reseeds; following windows return to delta-only feeding.

### Bounded state and deliberate differences

- Existing capture ring and input snapshot remain at most eight seconds of stereo PCM. They are not saved or uploaded.
- Native retained chroma is hop-aligned within the most recent eight seconds. With the pinned 48 kHz setup, preview has at most nine hops; live downsampled buffer plus raw remainder is at most 16,394 samples.
- The live FFT grid is anchored to the stream epoch. Trimming can exclude less than one hop (about 0.939 seconds) at the left edge compared with the old newly anchored batch window.
- libkeyfinder's own progressive preprocessing restarts its low-pass filter at call boundaries. We did not patch this upstream behavior. Thus stream results are not guaranteed to be sample-for-sample identical to arbitrary batch windowing.
- Preview defers the live downsampling remainder (at most ten raw frames, about 0.21 ms). The upstream finalizer also does not append its flushed remainder; no extra custom flush was introduced.
- The bounded long-run tests do not establish whole-process leak freedom. The retained native tail and fixed FFT/factory caches do not grow with song duration.

## Reproducible offline evaluation

Run from `E:\autotune helper\now-playing\src-tauri`:

```powershell
cargo test --lib
cargo test --example key_benchmark
cargo run --example key_benchmark -- --synthetic
cargo run --example key_benchmark -- E:\evaluation\manifest.json
```

The evaluator reuses the production stabilizer source, evaluates both paths at identical one-second audio endpoints, and reports cold/warm compute time, candidates, confirmed key changes, first confirmation, first correct confirmation (when labeled), and final correctness. Audio endpoint time is not GUI wall-clock latency.

Manifest format (paths relative to the manifest file are accepted):

```json
[
  {
    "id": "independently-labeled-local-audio",
    "path": "labeled-song.wav",
    "expected": { "pitchClass": 0, "mode": "major" },
    "labelSource": "Describe the independent score or validated annotation here"
  },
  {
    "id": "unlabeled-performance-only",
    "path": "another-song.flac"
  }
]
```

The shown C-major value is a schema example, not a label for an actual file. Use only local audio you have permission to evaluate and supply its real annotation. Expected labels without a non-empty independent source are rejected. Unknown songs receive `null` correctness fields and cannot enter accuracy statistics. Supplying a source string records provenance; it cannot automatically authenticate that annotation.

The optional local-file path uses the already installed FFmpeg decoder via a pipe, capped to the first 32 seconds, stereo 48 kHz float PCM, with input protocols restricted to `file,pipe`. No decoded PCM is written. The manifest is capped at 1 MiB/100 items and each file is processed separately. Synthetic evaluation needs no FFmpeg. Nothing enumerates personal music libraries, records current playback, or uploads files.

## Measured synthetic comparison

Evidence: `artifacts/key-detection-optimization/synthetic-benchmark.json`. Debug profile on this Windows machine, four constructed 32-second fixtures, 27 windows each (6 through 32 seconds), first window excluded from warm means.

| Fixture | Batch warm mean | Incremental warm mean | First correct confirmed audio time, batch / incremental | Final label, both |
|---|---:|---:|---:|---|
| C-major cadence | 225.59 ms | 69.51 ms | 9 s / 9 s | C major |
| A-minor cadence | 223.71 ms | 67.46 ms | 9 s / 9 s | A minor |
| Quiet C-major cadence | 222.70 ms | 66.72 ms | 9 s / 9 s | C major |
| Anti-phase C-major cadence | 219.11 ms | 65.41 ms | 9 s / 9 s | C major |

Average warm cost: **222.78 → 67.27 ms**, about **69.8% less compute time / 3.31x throughput**. Cold calls remained about 167–177 ms. Each path had zero confirmed flips in these fixtures. This is not “real-song accuracy improved by 70%,” nor “first detection is 70% faster.” No independently labeled real-song evaluation set was supplied in this run.

## Verification evidence

- Rust library: **83 passed**, including nine streaming tests (delta-only input, duplicates, quiet/anti-phase input, bounded 88-second run and harmony replacement, epochs/gaps/rewinds, invalid input, silence, irregular increments, clipping fallback recovery, and mix-selection changes). Log: `artifacts/key-detection-optimization/rust-tests.txt`.
- Evaluator: **15 passed** (12 reused production stabilizer tests plus three provenance/unknown-label tests).
- Frontend unit tests: **143 passed**.
- Browser regressions: **132 passed**, no retries in this run.
- Debug build completed: `artifacts/key-detection-optimization/debug-build.txt`. Existing Vite >500 kB chunk warning and production-unused `Stabilizer::begin` warning remain visible.
- Independent read-only review found no blocking defect. Its two requested coverage additions—irregular deltas and fallback/channel-change recovery—were added and passed.

### Native observation and strict-run limitation

Debug process was launched only after confirming no helper was running. Identity was recorded in `artifacts/key-detection-acceptance/run-1790695607267/process-evidence.json` (PID 79504, creation `2026-09-29T15:26:49.5456210Z`; these are historical evidence, not future operation authorization).

`artifacts/key-detection-native-1790695610716/verification.json` observed fresh current-player capture, matching source/track/generation, a new **B major** publication, and the actual **B 大调** title. Publication was **8.862 seconds after process creation**; the matching label observation was at **9.418 seconds**. This song has no independently verified label, so correctness is not asserted.

The first native call consumed 289,440 frames in 182 ms. Subsequent calls consumed about 48,000 new frames in 64–105 ms during the initial observed segment. A later real capture-generation change also reseeded rather than mixing the old stream. The title stayed 14 px / weight 650 in the 41 px title bar with no horizontal overlap.

**The strict native run exited 1 and is not recorded as an overall pass.** During its guard interval two `plugin:window|start_dragging` requests occurred and were blocked by the pre-existing read-only allowlist, producing two console `Object` errors. The guard reported intact ownership and successful restoration. The fresh detection/progress observations are usable evidence of the engine path, but do not turn the failed full run into a clean acceptance. No player or host commands were sent by the verifier and no forced process cleanup occurred.

A final separate read-only snapshot (`artifacts/key-detection-optimization/final-read-only-state.json`) confirmed the same debug process remained open, visible at 468×242, with **no active verifier guard**, fresh current-player audio, matching source/track/generation, and the then-current **A♯ 小调** label. This is a health/cleanup check, not a replacement for the failed strict run or proof of that song's true key.

## Retained artifacts and boundary

- Pre-edit scoped copies: `artifacts/key-detection-optimization/before/`.
- New debug binary SHA-256: `17D3FB8309039AEF4094B125EB0B7D4EDCE2BACE84CFA29D3267C2B094F3DC4A`.
- Release SHA-256 remained `78FB7563BBEA3EC48E1478CFFC9AAFC3415D7CD65DC817140FC8CD5C550396CB`.
- No Kaka/plugin-integration flow, parameter submit path, licensing behavior, or host lifecycle implementation was changed.

Next accuracy work needs independently labeled representative songs, including long intros, relative major/minor ambiguity, modulation and percussion-heavy passages. Existing single-tone/noise/percussion false positives remain a known limitation; this optimization does not claim to solve them.
