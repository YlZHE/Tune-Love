# Key Engine Evaluation Implementation Plan

> **For agentic workers:** Use subagent-driven-development to implement task-by-task.

**Goal:** Compare existing libkeyfinder, Essentia WASM and S-KEY without changing live behavior.

**Architecture:** Opt-in native diagnostics remain separate from the live command contract. An isolated offline lab owns public candidate dependencies and runs identical bounded audio windows. The main application's native settings and binaries remain unchanged.

**Tech Stack:** Rust, C++ libkeyfinder/FFTW, Node.js Essentia WASM, Python CPU S-KEY, FFmpeg.

**Spec:** `docs/superpowers/specs/2026-09-29-key-engine-evaluation-design.md`.

## Global Constraints

- Preserve GUI, IPC, capture source, six-second minimum, 3/5 vote rules and Auto-Tune integration.
- Do not rebuild/restart the main executable or modify release outputs.
- No recording, microphone/system-mix capture, player/DAW actions or uploads.
- Candidate dependencies/caches/checkpoints are workspace-local; inference is offline.
- Unknown truth yields null accuracy. Synthetic and real-song metrics stay separate.
- No Git initialization/commits/worktrees and no deletion of existing evidence.
- Source/model pins and license provenance are recorded; no vendor-source edits.

### Task 1: Opt-in native diagnostics and optimization control

Files: modify `src-tauri/native/keyfinder_bridge.cpp`, `src-tauri/src/key_detection/{engine.rs,stream.rs,types.rs,mod.rs}`, `src-tauri/build.rs`, and the existing `src-tauri/examples/key_benchmark.rs` only where needed; create focused diagnostics tests and `src-tauri/examples/key_window_probe.rs`.

Interfaces: preserve `detect` and `RollingDetector::analyze`. Add `detect_with_diagnostics(samples, rate, channels)` and `RollingDetector::analyze_with_diagnostics(window)` returning the ordinary candidate plus optional diagnostics. Diagnostics contain exactly 24 finite C-major/C-minor,...,B-major/B-minor scores, `best`, `runnerUp`, `margin` and `scoreKind: cosine_similarity`. Silence/duplicate input produces no diagnostics. Expose diagnostics only to Rust/example consumers, never IPC/UI.

`key_window_probe` reads a bounded stereo 48 kHz f32le buffer from stdin (maximum 32 seconds) and outputs JSON records for requested/available integer endpoints 6..12; each window is at most eight seconds. It supports batch and incremental diagnostic modes. Output includes candidate, diagnostics, exact start/end frames, per-call duration and compile-time optimization identity. It must not open capture devices or read arbitrary files.

- [x] Write focused tests before implementing diagnostics: native results unchanged, all 24 map correctly, best/runner-up sorted globally, margin equals the two highest values, silence/duplicates absent, nonfinite/misaligned inputs fail. Reuse upstream ToneProfile cosineSimilarity, not a copied replacement implementation.
- [x] Run focused tests and retain the initial expected failure, then implement.
- [x] Add `KEYFINDER_NATIVE_OPT_LEVEL` build override accepting only `0` or `2`; unset retains the prior behavior. Register rerun-if-env-changed and expose the effective/native-requested build label to probe JSON. Reject other values before invoking CMake/compiler.
- [x] Test probe stdin size/shape and argument failures. Run `cargo test --lib` and focused example tests; do not build the GUI or run performance comparisons concurrently with other benchmarks.
- [x] Save report including signatures, JSON schema, RED/GREEN logs, files and risks to the task report. Preserve main debug/release hashes.

### Task 2: Isolated candidate adapters and comparable offline runner

Files: create `experiments/key-evaluation/{package.json,package-lock.json,essentia-worker.cjs,skey-worker.py,compare.mjs,contract.mjs,contract.test.mjs,README.md}` and local `.gitignore` for runtime/download artifacts. All installed environments and checkpoints go under `artifacts/key-engine-evaluation/` or ignored lab `node_modules`, not the production package manifests.

Interfaces: invoke Task 1's `key_window_probe` JSON for the same 6..12 endpoints/trailing-eight-second windows. Candidate workers read bounded float PCM plus metadata from pipes and output mapped candidates/evidence/timing JSON; process startup/model load is separate. The comparison controller accepts `--synthetic` or a manifest using existing `id/path/expected/labelSource` semantics. It records case kind, label eligibility, source versions and serial timings. No live capture API exists in the lab.

- [x] Write tests for enharmonic key mapping, score-mode normalization, unknown/provenanced labels, same-window boundaries and bounded/invalid inputs; run RED before adapter code.
- [x] Use official Essentia.js 0.1.3 KeyExtractor in a reused worker; copy/delete WASM vectors correctly and surface errors. Record exact defaults/runtime version. Use pinned official S-KEY source and checkpoint in CPU mode, following the upstream predictor's feature/inference path; inference network calls are forbidden.
- [x] Ensure shared anti-phase-safe downmix and existing RMS/gain/clamp treatment reach every engine consistently. No early real-song accuracy threshold changes. Surface raw engine results on tone/noise/click negatives.
- [x] Run the contract tests, real Essentia and real S-KEY smoke inference, and one serial synthetic comparison. Do not silently substitute fake outputs if a backend cannot run. Save actionable failure diagnostics.
- [x] Document setup, reproducing the probe/default-versus-O2 experiment, license/source/model provenance and limits. Report file/JSON contracts and exact tests in the task report.

### Task 3: Measured runs and evidence report

Files: `docs/key-engine-evaluation.md`, plan checkboxes/ledger; generated results under `artifacts/key-engine-evaluation/`.

- [x] Snapshot/hash default and O2 probe executables. Run the same native synthetic corpus serially under both settings; compare all candidate/score outcomes and timing distributions. Restore the command environment afterward.
- [x] Run the three-engine experiment, retain negative findings and distinguish load/cold/warm costs. Use real songs only if independently labeled audio was explicitly supplied; otherwise state that gap.
- [x] Run scoped native/lab regression tests and confirm both main executable hashes, GUI/IPC/stability sources and running process identity unchanged. Never equate an offline test with a new native GUI acceptance.
- [x] Publish measured conclusions and limitations. Complete task-scoped and whole-change review with retained evidence. No release or merge action.

Execution boundary incident: the initial RED integration-test Cargo command also attempted to relink the running main exe; Windows blocked replacement. The incident and failure log are retained in the reports. All later Cargo runs select only library or explicit example targets; final debug/release exe hashes are unchanged.
