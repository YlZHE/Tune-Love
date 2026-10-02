# Candidate key-engine evaluation

The user approved the proposed comparison of the existing libkeyfinder pipeline,
Essentia and S-KEY, plus exposing candidate evidence and testing native compiler
optimization. This is offline experimental tooling, not a live-engine replacement.

## Invariants

- Preserve current GUI, IPC shape, capture source, six-second minimum, 3/5 vote rules,
  Auto-Tune integration and running application. Do not rebuild/restart the main
  executable or modify release outputs.
- No microphone/system-mix capture, recording, player actions, DAW actions or uploads.
- Captured PCM is never saved. Generated synthetic fixtures may be saved in artifacts.
  Explicitly supplied local audio may be decoded into bounded memory for evaluation.
- No invented real-song labels. Unknown truth yields null accuracy; synthetic results
  are reported separately and are not evidence of real-song accuracy.
- No global package/tool installation. Experimental Python/Node dependencies, public
  model/source downloads and caches live in workspace-local ignored directories.
- Preserve vendor source archives and licenses. No commercial assistant code/assets.
- Existing libkeyfinder enums are not Auto-Tune parameter enums.
- No Git repository initialization, commits, worktrees, or deletion of old evidence.

## Native diagnostics

Add opt-in score diagnostics to RollingDetector and the batch evaluation path. Reuse
libkeyfinder's original tone profiles and cosineSimilarity implementation; do not
substitute a new classifier. Return all 24 scores in C-major/C-minor through
B-major/B-minor order, best/runner-up keys and their gap. Scores are uncalibrated
similarities, not confidence probabilities. Existing analyze/detect calls preserve
their normal result and do not request diagnostic score computation.

The native optimization experiment must be opt-in and affect only the keyfinder
C++ build. Compare default optimization against level 2 with the same Rust debug
profile, fixture stream, and executable type. Retain separate benchmark executables,
logs and identities; do not overwrite or restart the running GUI.

## Candidate adapters

Use official Essentia.js 0.1.3 WASM on Windows, clearly labeled as WASM rather than
native C++. Use a pinned upstream S-KEY checkout and supplied checkpoint with a
workspace-local CPU-only Python environment. Inspect source/checkpoint loading
before executing third-party code. Record package versions, source commit, model
hash and upstream license. Setup may access public package/model repositories;
inference must not access the network.

One evaluation controller normalizes pitch labels and reports each engine's raw
evidence, compute time, runtime identity and exact audio window endpoints. All
engines receive the same channel selection, volume treatment and audio window;
their own documented sample-rate conversion is measured and disclosed. Evaluate
6..12-second endpoints using the last at most eight seconds. Warm runs are serial,
not concurrent. Loading/warm-up must be separated from per-window inference.

Keep all existing negative cases visible: silence, a single tone, noise, percussive
clicks, quiet music and anti-phase stereo. Do not hide undesirable engine results or
apply an unvalidated confidence threshold to manufacture success. Report the
distinction between single-window key candidates and stable confirmed output.

## Evidence

Tests cover score ordering/mapping, empty/invalid input, unchanged normal outputs,
normalization and label provenance. Run actual candidate inference on generated
fixtures, not mocks. Real-song evaluation remains incomplete until trusted labeled
audio is available. Finish with a concise report of measurements, unsupported paths
and unchanged main executable hashes.
