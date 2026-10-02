# Offline key-engine evaluation lab

This is experimental tooling, not a deployed replacement for the live detector.
It compares the current libkeyfinder 2.2.6 incremental adapter with Essentia WASM
and Deezer S-KEY CPU on the same requested audio windows. It neither calls capture
APIs nor controls a player, DAW or Auto-Tune. The application UI and its six-second
minimum / initial 3, replacement 5 vote rule are unchanged.

## Pinned dependencies and provenance

| Component | Pin | Upstream license / source |
|---|---|---|
| Existing libkeyfinder | 2.2.6, a409c7447e9f440a12627ff4a540a43e41b48a55 | GPL-3.0-or-later; existing vendor copy |
| Existing FFTW | 3.3.10 | retained existing vendor license |
| Essentia WASM | npm essentia.js 0.1.3 | package declares AGPL-3.0; https://github.com/MTG/essentia.js |
| S-KEY | 918b83d273568d5041569bb8068843d19a335726 | MIT; https://github.com/deezer/skey |
| Private Python | 3.11.16 | workspace-local runtime |
| CPU Torch / torchaudio | 2.7.1+cpu | official PyTorch CPU wheels |
| numpy / nnAudio / einops | 2.2.6 / 0.3.3 / 0.8.1 | retained package metadata |

S-KEY archive SHA256:
`8A4F50CEC1AE2A94EA24C8844642CF5C62A8A5FD827ACEA10CE951EB643D09F9`.

S-KEY checkpoint `skey/models/skey.pt`, 765465 bytes, SHA256:
`78DFD0AD4FA9434BF7CEC70A25934B7C575BDA9C80E994700140770AD3A5EAD4`.

Essentia npm integrity:
`sha512-vVEPgeVMEBLRXbM5o5H5Rgu53EPHu25vyFKYg+flWLzI/nEoegJQez9FKRv8GR/KxIBwm+fXDEFL+MkQeoHaLw==`.

All runtime/source/checkpoint downloads remain in ignored `artifacts/`.
Keep upstream licenses. The production package manifests were not changed.
This lab does not change or resolve the application's distribution licensing.

## Reproduce setup on this Windows workspace

Run from `E:\autotune helper\now-playing` using the existing Node.js, uv, Cargo,
MSVC and pinned CMake/vendor toolchain. These are process-local settings, not
machine-wide installs. Public downloads are setup only, not part of inference.

```powershell
$lab = 'E:\autotune helper\now-playing\artifacts\key-engine-evaluation'
$env:UV_CACHE_DIR = "$lab\uv-cache"
$env:UV_PYTHON_INSTALL_DIR = "$lab\python-runtime"
uv python install 3.11.16
uv venv --python 3.11.16 "$lab\venv"
uv pip install --python "$lab\venv\Scripts\python.exe" --index-url https://download.pytorch.org/whl/cpu torch==2.7.1+cpu torchaudio==2.7.1+cpu
uv pip install --python "$lab\venv\Scripts\python.exe" numpy==2.2.6 nnAudio==0.3.3 einops==0.8.1 tqdm==4.67.1 soundfile==0.13.1
npm install --prefix "$lab\essentia-runtime" --save-exact --ignore-scripts --no-audit --no-fund --cache "$lab\npm-cache" essentia.js@0.1.3
```

The official S-KEY archive is downloaded from
`https://github.com/deezer/skey/archive/918b83d273568d5041569bb8068843d19a335726.zip`
and expanded under `$lab\sources`. Verify the hashes above before use. The
worker expects `sources/skey-918b83d273568d5041569bb8068843d19a335726/` and verifies
the checkpoint hash on every launch. Full resolved Python versions from this run
are in `artifacts/key-engine-evaluation/python-freeze.txt`.

The checkpoint was inspected statically and is loaded with `weights_only=True`,
plus the narrow NumPy scalar/dtype/Float64DType allowlist. Upstream's unrestricted
`load_checkpoint` is deliberately not used. Vendor source is not patched.

## Run tests and comparison

```powershell
# These targets do not rebuild the main executable. Do not use --all-targets
# or a Cargo integration-test target while the existing GUI must be preserved.
cargo test --manifest-path src-tauri/Cargo.toml --lib --example key_window_probe --example key_benchmark
cargo build --manifest-path src-tauri/Cargo.toml --example key_window_probe --example key_benchmark
npm test --prefix experiments/key-evaluation
node experiments/key-evaluation/compare.mjs --synthetic --repeat 3 --output artifacts/key-engine-evaluation/new-result.json
# Optional native batch reconstruction, with separate output evidence:
node experiments/key-evaluation/compare.mjs --synthetic --native-mode batch --repeat 1 --output artifacts/key-engine-evaluation/new-batch-result.json
```

Output files are exclusive-create: choose a new name to preserve older evidence.
All engines run serially. Do not compile, run other test suites or benchmark a
second engine concurrently. Existing user applications are not stopped, so this
is an interactive-desktop measurement, not an isolated hardware benchmark.

To evaluate explicitly supplied local files, replace `--synthetic` with
`--manifest path/to/manifest.json`. The manifest is an array, maximum 100 entries:

```json
[
  {"id":"unlabeled","path":"track.wav"},
  {"id":"labeled","path":"another.wav","expected":{"pitchClass":0,"mode":"major"},"labelSource":"Independent score or annotated dataset record"}
]
```

Paths are relative to the manifest. Only existing local regular files are accepted.
The narrow single-file allowlist is WAV (`.wav`, `.wave`), FLAC (`.flac`),
MP3 (`.mp3`), Ogg (`.ogg`, `.oga`), raw ADTS AAC (`.aac`) and AIFF
(`.aif`, `.aiff`). The file header must match its extension before FFmpeg is
invoked; FFmpeg receives the corresponding explicit input `-f` demuxer, only
`file,pipe` protocols, and a 32-second output cap. Playlists, concat scripts,
HLS, external-reference containers (including MP4/M4A), and unknown formats
are excluded. This is a lab input policy, not a product format promise.
Manifest metadata is validated up front, then just one case is decoded and
processed at a time; decoded PCM is not retained in the report or for later
cases. One in-flight case plus its seven at-most-eight-second preprocessed
windows and bounded process buffers determine peak PCM memory, independent of
the 100-case manifest limit. PCM stays in memory.
The lab never searches a music directory. An expected label without provenance
is rejected. Unknown truth produces null accuracy, not a guess. The aggregate
`realSongAccuracy` field is intentionally null; per-labeled-case correctness is
available in the detailed rows/replay and must be interpreted with provenance.

## Native optimization comparison

Only the libkeyfinder C++/bridge translation units receive the optional override;
FFTW retains its existing Release build and Rust stays in the same debug profile.
Unset preserves Cargo/cc's original default. Only `0` and `2` are accepted.
The executable embeds requested/effective native optimization and Rust profile.

```powershell
# First keep default binaries separately (do not overwrite older evidence).
Remove-Item Env:KEYFINDER_NATIVE_OPT_LEVEL -ErrorAction SilentlyContinue
cargo build --manifest-path src-tauri/Cargo.toml --example key_window_probe --example key_benchmark
# Copy the two examples to a fresh artifacts bin/default directory.
$env:KEYFINDER_NATIVE_OPT_LEVEL = '2'
cargo build --manifest-path src-tauri/Cargo.toml --example key_window_probe --example key_benchmark
# Copy these to a fresh artifacts bin/o2 directory and hash both sets.
Remove-Item Env:KEYFINDER_NATIVE_OPT_LEVEL
# Restore default example artifacts as the final build, without building GUI.
cargo build --manifest-path src-tauri/Cargo.toml --example key_window_probe --example key_benchmark
```

Select an immutable copy with `--probe path/to/key_window_probe.exe`. Use
`--native-only` for the same corpus without launching the other two runtimes.
`--native-mode incremental|batch` defaults to incremental and is recorded in
result metadata. Batch reconstructs native context on every endpoint; its rows
are all reported as cold reconstructions and `warmTiming` is null. Batch is not
described as a warm incremental context.
The existing `key_benchmark --synthetic` measures ordinary non-diagnostic calls,
including both batch and incremental modes over 32-second fixtures.

## Input and score contracts

- Probe stdin: bounded little-endian float32 stereo at 48 kHz, 6..32 seconds.
  Arguments `--mode batch|incremental` and `--end 6..30` (default remains 12).
  The longer endpoint extends observation only: each call still uses at most
  eight seconds, and shorter input stops at its final available whole second.
  Saved pre-extension probes still accept only 6..12. JSON has build identity
  and rows with exact requested start/end frames, candidate and opt-in diagnostics.
- Workers: one JSON line per request, at most 2200000 bytes, fields `id`,
  `sampleRate:48000`, `channels:1`, `pcm` (base64 f32le, 6..8 seconds). One request
  at a time. First stdout line is readiness; replies echo id. Errors are explicit.
  Startup and requests time out; cleanup affects only worker children started by
  this lab. Upstream setup chatter is captured separately in stderr.
- Display case IDs remain unique and nonblank under the 1 MiB manifest limit;
  worker request IDs are separate compact `c<case>r<repeat>w<window>` values,
  so even long display IDs cannot cross the 128-character worker protocol bound.
- Shared stereo policy matches Rust: strongest channel if average RMS is less
  than 25% of the strongest channel RMS; otherwise average. Gate RMS <= 5e-5;
  gain clamp(0.10 / RMS, 1, 64); clamp [-1,1]. Rust consumes f64 mono while model/
  WASM consume f32, so this is equivalent policy, not bit-identical arithmetic.
- S-KEY uses checkpoint rate 22050 Hz and torchaudio resampling, one CPU thread.
  Its high-level file loader's extra peak normalization is omitted to preserve
  shared gain policy. The original feature model, weights and unusual minor-label
  ordering are retained. Its checkpoint audio config includes duration 15 seconds;
  this experiment deliberately measures the shorter live-compatible 6..8 seconds.
- Essentia uses KeyExtractor defaults except explicit 48000 Hz. Actual positional
  arguments are retained in each result's runtime record. This is WASM performance,
  not a native Essentia C++ benchmark.
- libkeyfinder scores: original cosine similarity, C-major/C-minor .. B-major/
  B-minor. Score ties retain upstream A-major-first priority. Best, runner-up and
  gap are diagnostic only. Ordinary live calls do not request extra score work.
- Essentia strength, S-KEY softmax and libkeyfinder similarity are different,
  uncalibrated quantities; they are not interchangeable confidence percentages.

## Reading the result

Raw candidates/evidence and effective pipeline candidates are separate. Silence
can produce a raw model guess; the existing shared silence gate suppresses it.
Pure tones, seeded noise and percussive clicks remain visible, without a new
abstention threshold. They have no valid known key, so their accuracy is null.

Confirmation is an **offline simulation** of the unchanged 3/5 consecutive-vote
rule. `firstConfirmedSignalSecond` is an audio endpoint, not wall-clock GUI latency.
The native incremental FFT grid is anchored to stream start and can omit less
than one hop at the left edge; shared requested windows do not imply identical
internal features or FFT grids between algorithms.

Worker timing excludes shared JS downmix, validation and pipe/base64 overhead;
it includes model/WASM inference and disclosed preprocessing. Native probe timing
includes Rust preprocessing and diagnostic scores. Native process wall time is
also recorded. Worker cold warmup/load and native first-context costs are separate.
For a deployment decision, compare ordinary native calls too, and benchmark the
eventual production integration rather than treating these lab runtimes as equal.

No labeled real-song corpus was supplied for the recorded experiment. Four easy
synthetic cadence fixtures are not a general accuracy study or grounds to replace
the live detector. The detailed evidence report is `docs/key-engine-evaluation.md`.
