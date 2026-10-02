# Player Audio Strands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the playback icon with official audio-reactive Strands, backed by current-player-only PCM suitable for future key analysis.

**Architecture:** A Windows process resolver binds the current media source to a unique process tree. A WASAPI worker keeps bounded PCM and exposes only lightweight level snapshots; an isolated React component drives the official shader without rerendering the song tree.

**Tech Stack:** Tauri 2, Rust, wasapi 0.24, Windows bindings, React 19, OGL and React Bits Strands.

**Spec:** `docs/superpowers/specs/2026-09-28-player-audio-strands-design.md` (approved in chat).

## Global Constraints

- Current player process tree only; no system mix, microphone, injection, audio files or uploads.
- Fail closed on missing/ambiguous source. Browser scope is application process tree, not individual tabs.
- Preserve layout/theme/metadata animation/tooltips; no additional settings.
- PCM: 48 kHz, stereo f32, maximum 8 seconds in memory; clear on source/song changes, pause and failure.
- IPC `get_audio_level` has sourceId, trackKey, status, rms, peak, level and updatedAtMs as defined in the spec.
- Official Strands source/attribution; reduced-motion, stale data and GPU failure safe fallback.
- Only now-playing files. No Git exists; use source snapshots for review and preserve reports, no commits or worktree creation.

## Task 1: Frontend Strands integration

**Files:** create `src/components/AudioStrands.tsx`, `src/components/reactbits/Strands.tsx`, `src/components/reactbits/Strands.css`, `src/audioLevel.ts`, `src/audioLevel.test.ts`, `tests/audio-strands.spec.ts`; modify `src/App.tsx`, `src/styles.css`, dependency lockfile and attribution.

**Interface:** consume `get_audio_level` contract; `AudioStrands({sourceId, trackKey, playing})`; own high-frequency updates, not App state.

- [x] Add failing tests for frame acceptance and visual stale/paused behavior. Example: `expect(validLevel({...frame, sourceId: 'other'}, 'current', key, now)).toBe(0)`; `expect(validLevel({...frame, updatedAtMs: now - 401}, 'current', key, now)).toBe(0)`.
- [x] Run focused Vitest / Playwright and record RED.
- [x] Install pinned OGL, vendor official registry files with `apply_patch`; adapt lifecycle and ref-driven uniforms. Use computed CSS accent, fixed phase speed, attack/release smoothing, capped DPR. No invented music in browser preview.
- [x] Implement the IPC reader and replace only the playback glyph. Cleanup pending listeners/timers/RAF on unmount; use static fallback on reduced motion/WebGL failure.
- [x] Run unit, UI and build checks; record GREEN and test evidence in task report.

## Task 2: Player process capture backend

**Files:** create `src-tauri/src/audio/{mod.rs,signal.rs,process.rs,capture.rs}` and `src-tauri/examples/audio_probe.rs`; modify `src-tauri/src/{lib.rs,media.rs}`, `Cargo.toml`, `Cargo.lock`.

**Interface:** `AudioState::snapshot() -> AudioLevel`, `AudioState::analysis_window() -> Option<PcmWindow>`; command `get_audio_level`; `MediaState::audio_target() -> Option<AudioTarget>` with source_id, track_key, playing.

- [x] Add failing Rust tests for RMS/peak, silence, NaN/Infinity, overflow, switching, strict identity selection and ambiguity. Hand-derived sample expectation: stereo `[0.25,-0.25,0.25,-0.25]` has RMS and peak 0.25; silence has both zero; a different executable path cannot match a registered AUMID target.
- [x] Run `cargo test --lib`, record RED before implementation.
- [x] Add target-specific `wasapi = "=0.24.0"`, use existing Windows bindings for source-to-process resolution; do not create another audio engine. Query only necessary process rights, validate creation time and keep handle alive. Use `new_application_loopback_client(pid, true)` with shared event capture and bounded waits; never call a system-loopback fallback.
- [x] Feed f32 samples into a bounded rolling window and windowed RMS/peak. Limit IPC cadence, expire frames, clear on disconnection and target generation changes, and reject late capture data. Refresh source matching without doing Shell work in a realtime packet callback.
- [x] Connect startup/exit and media selection; preserve the existing 700 ms metadata polling and source identity features. Add read-only `audio_probe` that reports only source identity and aggregate PCM evidence, supports an explicit test PID, times out and stops cleanly.
- [x] Run fmt/tests and the Folia probe. Record actual sample evidence or an explicit failure; never claim success from compilation.

## Task 3: Isolation and native acceptance

**Files:** `src-tauri/examples/audio_isolation.rs` (only if needed for controlled playback validation), `scripts/verify-native.mjs`, `README.md`, `artifacts/audio-verification.json`.

**Interface:** exercise production resolver/capture API and genuine Tauri command, not a parallel implementation.

- [x] Test controlled low-amplitude target and non-target playback processes: with target silent and other sound active, captured RMS remains below silence threshold; with target active, RMS exceeds it. Report numeric results and identities, not PCM.
- [x] Build debug; identify and normally close only this app's current release before starting debug with existing verification config. Do not control or terminate Folia, Auto-Tune or DAW processes.
- [x] Extend native CDP acceptance to check current source/track binding, fresh real levels and a rendered Strands canvas. Run existing native settings/color/source checks with restoration in finally.
- [x] Run `npm test`, `npm run test:ui`, `npm run build`, `cargo test --lib` and native verification. Build release only after a clean test shutdown, then start it normally.
- [x] Document architecture, OS/DRM/browser limits and measured tests; retain license and original baseline artifacts. Request a final read-only review; fix important findings before completion.
