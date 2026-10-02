# Live Key Detection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Identify the current player's musical key from its isolated PCM and replace the top-left brand with a stable animated key label.

**Architecture:** A native libkeyfinder adapter processes a bounded PCM copy on one independent worker. Capture/media generations and a sample cursor protect a pure stabilizer from stale results. A read-only IPC snapshot drives a validated, low-frequency frontend poll and the existing song-title motion.

**Tech Stack:** Tauri 2, Rust, libkeyfinder v2.2.6, FFTW, React 19, TypeScript, Motion, Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-09-29-live-key-detection-design.md` (approved September 29, 2026).

## Global Constraints

- Only change `now-playing`; no Auto-Tune writes, DAW interaction, player injection, microphone, system-mix fallback, or saved/uploaded PCM.
- Use original libkeyfinder v2.2.6 commit `a409c7447e9f440a12627ff4a540a43e41b48a55`; keep upstream sources, notices, and provenance. No replacement detection algorithm.
- Prefer `libkeyfinder-sys`; its pkg-config-only build cannot supply Windows MSVC libraries itself. If unsuitable, use the approved thin native build/FFI adapter. Pin FFTW and preserve its license/source. Do not use unverified binary mirrors or add global toolchains.
- PCM is 48,000 Hz stereo f32, maximum 8 seconds; minimum analysis input is 6 seconds. Work from a copy with a bounded normalization gain.
- One analysis at a time, one-second delay after completion, no backlog. Same sample cursor cannot vote twice. Initial result requires 3 consistent new candidates; replacement requires 5.
- Source/track/media generation changes clear results; pause keeps a confirmed same-track result but clears votes and PCM. Capture discontinuity, staleness, and reconnection invalidate pending analysis. Reject observed A→B→A late results.
- IPC is exactly `get_key_detection` with sourceId, trackKey, targetGeneration, status, key, updatedAtMs. Key is C=0 through B=11 and mode major/minor, not Auto-Tune enum values.
- Title fallback is `AutoTune Helper`; detected labels use sharps and 大调/小调, around 14 px / 650 weight. Preserve 41 px titlebar, 468×242 native window, icon, drag area, actions, existing controls, settings, and song animations.
- No statistics presented as calibrated confidence; no claim of real-song correctness without independent labels. Synthetic correctness and live pipeline function are separate evidence.
- Debug only. No Git repository exists: do not initialize, commit, branch, or create worktrees. Use scoped pre-edit copies and no-index diffs for reviews; retain reports/evidence.
- Existing native app/user settings must be preserved. Recheck process identity before normal debug restart. Do not kill user processes or bypass a launch policy denial. Native verification for this task is read-only.
- Authored edits use apply_patch. Implementers do not spawn agents. Controller owns reviews. Do not edit/build frontend while browser tests run.

## File Structure and Interfaces

- `src-tauri/src/key_detection/types.rs`: serializable MusicalKey, Mode and DetectionSnapshot.
- `src-tauri/src/key_detection/engine.rs`: validated bounded input and original-library adapter; engine test fixtures remain under cfg(test).
- `src-tauri/native/keyfinder_bridge.cpp`: exception-safe C ABI; no algorithm replacement.
- `src-tauri/vendor/libkeyfinder/`, `src-tauri/vendor/fftw/`: pinned upstream dependency sources/licenses as needed.
- `src-tauri/build.rs`, `Cargo.toml`, `Cargo.lock`: reproducible static native build and dependencies.
- `src-tauri/src/key_detection/stability.rs`: pure identity/voting state machine.
- `src-tauri/src/key_detection/mod.rs`: one worker, state, stop lifecycle, read-only IPC.
- `src-tauri/src/audio/mod.rs`, `media.rs`, `lib.rs`: PCM identity metadata and setup/teardown only.
- `src/keyDetection.ts`, `src/useKeyDetection.ts`: runtime validation and visible-window polling.
- `src/components/textTransition.ts`, `KeyTitle.tsx`, `TrackTransition.tsx`: shared title motion and accessible brand replacement.
- `src/App.tsx`, `src/styles.css`: minimal integration.
- `src/keyDetection.test.ts`, `tests/key-detection.spec.ts`, `scripts/verify-key-detection.mjs`: test and read-only native evidence.

### Task 1: Original Engine and Safe Native Boundary

**Files:** Create types.rs, engine.rs, initial key_detection/mod.rs, native bridge, upstream source/license directories, dependency provenance doc. Modify build.rs, Cargo.toml/Cargo.lock and add `pub mod key_detection` in lib.rs. Do not start a worker yet.

**Interfaces:**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode { Major, Minor }
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicalKey { pub pitch_class: u8, pub mode: Mode }
// Consumes interleaved f32 PCM; no caller state or persisted audio.
pub fn detect(samples: &[f32], sample_rate: u32, channels: u16)
    -> Result<Option<MusicalKey>, String>;
```

- [x] Write engine/mapping and invalid-input tests before the adapter. Examples:
```rust
assert_eq!(map_upstream(0), Some(MusicalKey { pitch_class: 9, mode: Mode::Major }));
assert_eq!(map_upstream(7), Some(MusicalKey { pitch_class: 0, mode: Mode::Minor }));
assert_eq!(map_upstream(24), None);
assert_eq!(detect(&vec![0.0; 48_000 * 2 * 8], 48_000, 2).unwrap(), None);
assert!(detect(&[f32::NAN; 32], 48_000, 2).is_err());
```
  Add literal expected mappings for all 24 enums. Use generated major/minor chord/cadence audio fixtures with hand-derived notes, low-volume copies, insufficient input, and bounded input-size checks. Record negative single-tone/noise/percussion results without asserting an unjustified rejection capability.
- [x] Run `cargo test --manifest-path src-tauri/Cargo.toml key_detection --lib`, record the expected missing implementation failure.
- [x] Implement the adapter with exception/error containment; validate channel/rate/size/nonfinite data before FFI. Normalize private input above a measured noise floor, cap gain, and do not mutate caller PCM. Avoid anti-phase stereo cancellation by choosing a usable channel if mono averaging would cancel; cover this behavior with a test.
- [x] Build original libkeyfinder and pinned FFTW without runtime commercial libraries. Existing bindings are the first option; if they cannot supply the required Windows self-contained build, retain the reason and use only a thin FFI/build adapter. A workspace-local official portable CMake is allowed if needed; no global installations. Cache native builds, not network fetches on each Cargo invocation.
- [x] Re-run focused tests, then `cargo test --manifest-path src-tauri/Cargo.toml --lib`. Record exact source hashes, library versions, license files, timing, and limitations in the task report. Do not claim real-music accuracy from synthesized fixtures.

### Task 2: Generation-Safe Background Analysis

**Files:** Modify audio/mod.rs, media.rs, key_detection/mod.rs, lib.rs. Create key_detection/stability.rs and focused Rust tests.

**Interfaces:** Consume Task 1 detect and types. Extend `AudioTarget` with `target_generation: u64`. Extend `PcmWindow` with `target_generation`, `capture_generation`, `sample_end_sequence` and a capture timestamp. Provide a cheap identity/status read without copying PCM for publication checks. Keep capture worker cancellation generation distinct from discontinuity generation so a packet reset does not invalidate every subsequent packet of the current capture.
```rust
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionSnapshot {
    pub source_id: Option<String>, pub track_key: Option<String>,
    pub target_generation: u64, pub status: &'static str,
    pub key: Option<MusicalKey>, pub updated_at_ms: u64,
}
// KeyDetectionState owns only analysis lifecycle/result, never capture.
// new(audio: AudioState), start(), stop(), snapshot() -> DetectionSnapshot
```

- [x] First add failing tests for generation and samples: observed A→B→A, reset packet, long stall, reconnect, pause and stop invalidate old analysis; fresh windows have monotonically advancing sample cursors. Keep existing band visualization tests unchanged except explicit new target field.
- [x] Add failing pure-state tests proving three distinct-window votes publish, duplicate windows do not vote, different/invalid candidates break the consecutive streak, five new matching candidates replace a result, same-track pause preserves only a confirmed result, track change clears immediately, mismatched generations/tokens cannot publish. A stale result must not clear or increment votes belonging to the current target.
- [x] Run focused tests to establish RED, then implement capture metadata and stabilizer. Read current media under existing lock ordering; never hold a state mutex during library analysis. Checked freshness must include current media/capture identity at result commit, not only at computation start.
- [x] Add one independent named analysis thread, at most one computation, one-second post-completion scheduling and responsive stop polling. Use only windows with new sample cursors and at least six seconds. Contain panic/native errors as unavailable/no label; do not silently kill audio capture. Snapshot refresh clears identity mismatches even while analysis is in progress. Start and stop via Tauri alongside existing audio/media lifecycle.
- [x] Expose `get_key_detection` read-only and serialize exact camelCase contract. Run `cargo test --manifest-path src-tauri/Cargo.toml --lib`, recording behavior tests and measurements.

### Task 3: Animated Header, Runtime Guards, and Evidence

**Files:** Create keyDetection.ts, useKeyDetection.ts, KeyTitle.tsx, textTransition.ts, unit/browser tests and verify-key-detection.mjs. Modify App.tsx, TrackTransition.tsx, styles.css and relevant native verification guards. Update dependency notices and validation report if needed.

**Interfaces:** Consume exact DetectionSnapshot IPC plus current Snapshot/songKey. Runtime validator derives a visible label only from matching valid data:
```ts
type DetectedKey = { pitchClass: number; mode: "major" | "minor" };
// Inputs are untrusted IPC; return null on invalid, stale ownership, or unavailable.
export function keyLabel(value: unknown, sourceId: string | null,
  trackKey: string, targetGeneration: number | undefined): string | null;
// useKeyDetection receives the current media identity, polls at ~1 second,
// ignores late responses, and suspends reads while document.hidden.
```

- [x] Write unit tests with literal labels for all 24 keys and negative malformed key/mode/status, old generation/source/song, missing current generation, invalid identity/timestamps. Write browser tests showing initial brand, valid A minor, old-result rejection after song switch, pause retention, identical results not restarting DOM motion, reduced-motion fallback, button/drag preservation and no clipping at native size.
```ts
expect(keyLabel({ sourceId: "player", trackKey: "song", targetGeneration: 4,
  status: "detected", key: { pitchClass: 9, mode: "minor" }, updatedAtMs: 100 },
  "player", "song", 4)).toBe("A 小调");
```
- [x] Run focused unit/browser tests for RED. Only mock the native IPC boundary; render real components and use real Motion. Never fake PCM detection in the native evidence script.
- [x] Implement validated non-overlapping polling with lifecycle/visibility guards; errors clear label. Extract the exact existing song-title motion settings into a helper shared with KeyTitle. Preserve the artist's existing timing. KeyTitle uses AnimatePresence and inert/aria-hidden on exiting text. Key by displayed label, not polling object/timestamp.
- [x] Integrate at original brand location; preserve icon/actions/drag region. Detected label uses 14 px, 650 weight; brand styling stays current. No new user settings or diagnostic decorations.
- [x] Allow only `get_key_detection` in existing read-only IPC guards; add guard regression coverage. Implement read-only native script using current CDP main page: record matching media/real detection snapshots over time, screenshot final label, assert zero writes, restore any wrappers. Save numeric metadata/evidence only, never audio.
- [x] Run `npm test`, `npm run build`, `npm run test:ui` (no concurrent source editing/build), and Rust regression suite. Build debug with `npm run tauri -- build --debug --no-bundle --config src-tauri/tauri.verify.conf.json`; do not modify release output.
- [x] Before replacing a running debug build, preserve current UI preview/settings and verify process identity. If safe restart is denied/unavailable, report build ready and request manual opening; no alternative launch bypass. Run read-only native verification after the new binary is open and actual player audio is available. Distinguish pipeline completion, measured latency, and unverified real-song accuracy in `docs/2026-09-29-live-key-detection-validation.md`.

## Review and Completion

- Every task: RED/GREEN report, scoped source diff, separate spec/quality review, fixes through original implementer.
- One final cross-task review checks capture/commit lock ordering, IPC ownership, shutdown, native dependency reproducibility, and UI regression.
- Retain plan, reports, dependency provenance and validation evidence. No git-backed scratch cleanup is possible in this non-Git workspace.
