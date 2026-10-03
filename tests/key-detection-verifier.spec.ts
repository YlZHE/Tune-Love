import { expect, test } from "@playwright/test";
import {
  createLiveEvidence,
  installKeyDetectionGuard,
  observeLiveEvidence,
  restoreKeyDetectionGuard,
} from "../scripts/key-detection-verifier-logic.mjs";

const liveDetected = {
  observedAtMs: 10_000,
  mediaStatus: "ready",
  playbackStatus: "playing",
  mediaAgeMs: 100,
  mediaGeneration: 4,
  detectionGeneration: 4,
  detectionStatus: "detected",
  matchedIdentity: true,
  // libKeyFinder key and timestamp: progress evidence only.
  pitchClass: 9,
  mode: "minor",
  detectionUpdatedAtMs: 9_000,
  // The Key/Scale target the title is built from.
  targetKey: 9,
  targetScale: "minor",
  audioStatus: "capturing",
  audioSourceMatches: true,
  audioTrackMatches: true,
  audioAgeMs: 100,
  audioRms: 0.2,
  audioPeak: 0.4,
  audioLevel: 0.3,
  presentKeyCount: 1,
  displayedLabel: "A Minor",
};

test("cached same-key detection cannot pass without progress observed in this verifier run", () => {
  const evidence = createLiveEvidence(liveDetected);
  const result = observeLiveEvidence(evidence, { ...liveDetected, observedAtMs: 10_500 });
  expect(result.accepted).toBe(false);
  expect(result.progressed).toBe(false);
});

test("matching non-detected to detected progress with fresh live audio is accepted", () => {
  const evidence = createLiveEvidence({ ...liveDetected, detectionStatus: "idle", pitchClass: null,
    mode: null, targetKey: null, targetScale: null, displayedLabel: null, presentKeyCount: 0 });
  expect(observeLiveEvidence(evidence, { ...liveDetected, detectionStatus: "analyzing", pitchClass: null,
    mode: null, targetKey: null, targetScale: null, displayedLabel: null, presentKeyCount: 0 }).accepted).toBe(false);
  const result = observeLiveEvidence(evidence, liveDetected);
  expect(result).toMatchObject({ accepted: true, progressed: true, expectedLabel: "A Minor" });
});

test("cached detected output on a new target generation still requires a later publication", () => {
  const evidence = createLiveEvidence({ ...liveDetected, mediaGeneration: 8, detectionGeneration: 8 });
  const newTarget = { ...liveDetected, mediaGeneration: 9, detectionGeneration: 9 };
  const reset = createLiveEvidence(newTarget);
  expect(observeLiveEvidence(reset, newTarget).accepted).toBe(false);
  expect(observeLiveEvidence(reset, { ...newTarget, detectionStatus: "analyzing",
    pitchClass: null, mode: null, targetKey: null, targetScale: null, displayedLabel: null, presentKeyCount: 0 }).accepted).toBe(false);
  expect(observeLiveEvidence(reset, { ...newTarget, detectionUpdatedAtMs: 9_500 }).accepted).toBe(true);
  expect(observeLiveEvidence(evidence, newTarget).accepted).toBe(false);
});

test("a newly advanced detection timestamp can prove replacement progress", () => {
  const evidence = createLiveEvidence(liveDetected);
  const result = observeLiveEvidence(evidence, { ...liveDetected, detectionUpdatedAtMs: 9_500,
    pitchClass: 1, mode: "major", targetKey: 1, targetScale: "major", displayedLabel: "C# Major" });
  expect(result).toMatchObject({ accepted: true, progressed: true, expectedLabel: "C# Major" });
});

test("the title is the Key/Scale target, not the libKeyFinder key", () => {
  const evidence = createLiveEvidence({ ...liveDetected, detectionStatus: "idle" });
  const changes = { detectionUpdatedAtMs: 9_500 };
  // libKeyFinder says A minor, the target says Chromatic: the title must read as the target.
  expect(observeLiveEvidence(evidence, { ...liveDetected, ...changes, targetKey: null, targetScale: "chromatic",
    displayedLabel: "Chromatic" })).toMatchObject({ accepted: true, expectedLabel: "Chromatic" });
  expect(observeLiveEvidence(evidence, { ...liveDetected, ...changes, targetKey: 5, targetScale: "chromatic",
    displayedLabel: "F Chromatic" })).toMatchObject({ accepted: true, expectedLabel: "F Chromatic" });
  // The old libKeyFinder-derived label is no longer accepted.
  expect(observeLiveEvidence(evidence, { ...liveDetected, ...changes, displayedLabel: "A 小调" }).accepted).toBe(false);
  expect(observeLiveEvidence(evidence, { ...liveDetected, ...changes, targetKey: 5, targetScale: "chromatic",
    displayedLabel: "A Minor" }).accepted).toBe(false);
});

for (const [name, changes] of [
  ["paused media", { playbackStatus: "paused" }],
  ["stale media", { mediaAgeMs: 8_001 }],
  ["stale audio", { audioAgeMs: 401 }],
  ["non-capturing audio", { audioStatus: "idle" }],
  ["silent RMS", { audioRms: 0 }],
  ["silent level", { audioLevel: 0 }],
  ["wrong audio source", { audioSourceMatches: false }],
  ["wrong audio track", { audioTrackMatches: false }],
  ["out-of-range pitch", { pitchClass: 12 }],
  ["fractional pitch", { pitchClass: 9.5 }],
  ["malformed mode", { mode: "dorian" }],
  ["outgoing-only label", { presentKeyCount: 0 }],
  ["ambiguous present labels", { presentKeyCount: 2 }],
  ["wrong displayed label", { displayedLabel: "A Major" }],
  ["no Key/Scale target", { targetKey: null, targetScale: null }],
  ["null key on a Major/Minor target", { targetKey: null }],
  ["out-of-range target key", { targetKey: 12 }],
  ["unknown target scale", { targetScale: "dorian" }],
]) test(`rejects ${name} even after timestamp progress`, () => {
  const evidence = createLiveEvidence(liveDetected);
  const result = observeLiveEvidence(evidence, { ...liveDetected, detectionUpdatedAtMs: 9_500, ...changes });
  expect(result.accepted).toBe(false);
});

test("guard restoration requires the exact owner token and never tears down a preexisting verifier", async ({ page }) => {
  await page.goto("about:blank");
  await page.evaluate(() => {
    const state = window as any;
    state.foreignRestoreCalls = 0;
    state.fetch = async () => new Response("null", { headers: { "Tauri-Response": "ok" } });
    state.chrome = { webview: { postMessage: () => {} } };
    state.__TAURI_INTERNALS__ = { runCallback: () => {} };
    state.__keyDetectionVerification = { token: "foreign", restore: () => { state.foreignRestoreCalls++; } };
  });
  await expect(page.evaluate(installKeyDetectionGuard, "ours")).rejects.toThrow("Another native verifier is active");
  const result = await page.evaluate(restoreKeyDetectionGuard, "ours");
  expect(result).toMatchObject({ owned: false, restored: false });
  expect(await page.evaluate(() => ({ calls: (window as any).foreignRestoreCalls,
    token: (window as any).__keyDetectionVerification.token }))).toEqual({ calls: 0, token: "foreign" });
});

test("guard ignores a wrong token and restores wrappers only for its owner", async ({ page }) => {
  await page.goto("about:blank");
  await page.evaluate(() => {
    const state = window as any;
    state.fetch = async () => new Response("null", { headers: { "Tauri-Response": "ok" } });
    window.fetch = state.fetch;
    state.chrome = { webview: { postMessage: () => {} } };
    state.__TAURI_INTERNALS__ = { runCallback: () => {} };
    state.originalFetch = window.fetch;
    state.originalPost = window.chrome.webview.postMessage;
  });
  const token = await page.evaluate(installKeyDetectionGuard, "owner");
  expect(await page.evaluate(restoreKeyDetectionGuard, "wrong")).toMatchObject({ owned: false, restored: false });
  expect(await page.evaluate(() => (window as any).__keyDetectionVerification.token)).toBe(token);
  expect(await page.evaluate(restoreKeyDetectionGuard, token)).toMatchObject({ owned: true, intact: true, restored: true });
  expect(await page.evaluate(() => {
    const state = window as any;
    return state.fetch === state.originalFetch && state.chrome.webview.postMessage === state.originalPost
      && !state.__keyDetectionVerification;
  })).toBe(true);
});
