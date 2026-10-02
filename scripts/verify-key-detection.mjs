// Read-only verification for the genuine current-player key pipeline.
// It never injects media fixtures, saves PCM/base64, changes playback, or writes settings.
import { chromium, expect } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import { createLiveEvidence, installKeyDetectionGuard, observeLiveEvidence, restoreKeyDetectionGuard } from "./key-detection-verifier-logic.mjs";

const timeoutMs = 40_000;
const sampleIntervalMs = 500;
const out = `artifacts/key-detection-native-${Date.now()}`;
await mkdir(out, { recursive: true });
const browser = await chromium.connectOverCDP("http://127.0.0.1:19224");
const pages = () => browser.contexts().flatMap(context => context.pages());
await expect.poll(() => pages().some(page => page.url() === "http://tauri.localhost/"),
  { timeout: 15_000 }).toBe(true);
const page = pages().find(page => page.url() === "http://tauri.localhost/");
if (!page) throw new Error("AutoTune Helper main page was not found");

const errors = [];
page.on("pageerror", error => errors.push(error.message));
page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
const report = { output: out, timeoutMs, sampleIntervalMs, observations: [], errors, passed: false };
let guardToken = null;
let liveEvidence = null;

try {
  guardToken = await page.evaluate(installKeyDetectionGuard, "native-key-detection");

  await expect(page.locator(".music-window")).toBeVisible();
  const deadline = Date.now() + timeoutMs;
  let matched = null;
  while (Date.now() < deadline) {
    const observation = await page.evaluate(async () => {
      const observedAtMs = Date.now();
      const [media, detection, audio] = await Promise.all([
        window.__TAURI_INTERNALS__.invoke("get_now_playing"),
        window.__TAURI_INTERNALS__.invoke("get_key_detection"),
        window.__TAURI_INTERNALS__.invoke("get_audio_level"),
      ]);
      const track = media.track;
      const trackKey = track ? JSON.stringify([track.sourceId, track.title, track.artist, track.album]) : null;
      const sourceMatches = !!track && detection.sourceId === track.sourceId;
      const trackMatches = !!trackKey && detection.trackKey === trackKey;
      const generationMatches = detection.targetGeneration === media.targetGeneration;
      const presentNodes = [...document.querySelectorAll('.key-title-text.is-detected:not([aria-hidden="true"]):not([inert])')];
      return {
        observedAtMs,
        mediaStatus: media.status,
        playbackStatus: track?.playbackStatus ?? null,
        mediaGeneration: media.targetGeneration ?? null,
        detectionGeneration: detection.targetGeneration ?? null,
        detectionStatus: detection.status,
        sourceMatches,
        trackMatches,
        generationMatches,
        matchedIdentity: sourceMatches && trackMatches && generationMatches,
        pitchClass: detection.key?.pitchClass ?? null,
        mode: detection.key?.mode ?? null,
        detectionUpdatedAtMs: detection.updatedAtMs ?? null,
        detectionAgeMs: Number.isFinite(detection.updatedAtMs) ? observedAtMs - detection.updatedAtMs : null,
        mediaAgeMs: Number.isFinite(media.capturedAtMs) ? observedAtMs - media.capturedAtMs : null,
        audioStatus: audio?.status ?? null,
        audioSourceMatches: !!audio && !!track && audio.sourceId === track.sourceId,
        audioTrackMatches: !!audio && !!trackKey && audio.trackKey === trackKey,
        audioAgeMs: Number.isFinite(audio?.updatedAtMs) ? observedAtMs - audio.updatedAtMs : null,
        audioRms: audio?.rms ?? null,
        audioLevel: audio?.level ?? null,
        presentKeyCount: presentNodes.length,
        displayedLabel: presentNodes.length === 1 ? presentNodes[0].textContent?.trim() ?? null : null,
      };
    });
    report.observations.push(observation);
    if (!liveEvidence || observation.mediaGeneration !== liveEvidence.initialMediaGeneration
      || observation.detectionGeneration !== liveEvidence.initialGeneration) {
      liveEvidence = createLiveEvidence(observation);
      report.observations.at(-1).baselineReset = true;
      await page.waitForTimeout(sampleIntervalMs);
      continue;
    }
    const evidence = observeLiveEvidence(liveEvidence, observation);
    report.observations.at(-1).evidence = evidence;
    if (evidence.accepted) {
      matched = observation;
      break;
    }
    await page.waitForTimeout(sampleIntervalMs);
  }

  expect(matched, "need a genuine matching detected key within the bounded sample window").not.toBeNull();
  const expectedLabel = observeLiveEvidence(liveEvidence, matched).expectedLabel;
  expect(matched.displayedLabel).toBe(expectedLabel);
  await expect.poll(async () => page.locator('.key-title-text.is-detected:not([aria-hidden="true"]):not([inert])').count(),
    { timeout: 2000 }).toBe(1);
  await page.waitForFunction(() => new Promise(resolve => {
    let stableFrames = 0;
    const check = () => {
      const stage = document.querySelector(".key-title-stage");
      const node = document.querySelector('.key-title-text.is-detected:not([aria-hidden="true"]):not([inert])');
      const allPresent = document.querySelectorAll('.key-title-text:not([aria-hidden="true"]):not([inert])').length === 1;
      const noExitingFrames = document.querySelectorAll('.key-title-text[aria-hidden="true"], .key-title-text[inert]').length === 0;
      const bounds = node?.getBoundingClientRect();
      const stageBounds = stage?.getBoundingClientRect();
      const opacity = node ? Number.parseFloat(getComputedStyle(node).opacity) : 0;
      const settled = !!node && !!bounds && !!stageBounds && allPresent && noExitingFrames && opacity >= 0.99
        && Math.abs(bounds.top - stageBounds.top) < 1;
      stableFrames = settled ? stableFrames + 1 : 0;
      if (stableFrames >= 2) resolve(true); else requestAnimationFrame(check);
    };
    requestAnimationFrame(check);
  }), undefined, { timeout: 2000 });
  const presentation = await page.locator('.key-title-text.is-detected:not([aria-hidden="true"]):not([inert])').evaluate(element => {
    const bounds = element.getBoundingClientRect();
    const titlebar = element.closest(".titlebar").getBoundingClientRect();
    const actions = document.querySelector(".window-actions").getBoundingClientRect();
    const style = getComputedStyle(element);
    return { fontSize: style.fontSize, fontWeight: style.fontWeight, titlebarHeight: titlebar.height,
      presentCount: document.querySelectorAll('.key-title-text.is-detected:not([aria-hidden="true"]):not([inert])').length,
      horizontalOverlap: bounds.left < titlebar.left || bounds.right > actions.left };
  });
  expect(presentation).toEqual({ fontSize: "14px", fontWeight: "650", titlebarHeight: 41, presentCount: 1, horizontalOverlap: false });
  report.presentation = presentation;
  report.matchedObservation = matched;
  await page.screenshot({ path: `${out}/key-detection.png` });
  expect(errors).toEqual([]);
  report.passed = true;
} catch (error) {
  report.failure = String(error);
  await page.screenshot({ path: `${out}/failure.png` }).catch(() => {});
} finally {
  try {
    report.instrumentation = guardToken
      ? await page.evaluate(restoreKeyDetectionGuard, guardToken)
      : { commands: {}, blocked: [], intact: false, restored: false, owned: false };
  } catch (error) {
    report.instrumentation = { commands: {}, blocked: [String(error)], intact: false, restored: false };
  }
  if (!report.instrumentation.intact || !report.instrumentation.restored
    || report.instrumentation.blocked.length) report.passed = false;
  report.observationCount = report.observations.length;
  await writeFile(`${out}/verification.json`, JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ ...report, observations: undefined }, null, 2));
}

// Disconnect this CDP client only; keep the debug helper and music application open.
process.exit(report.passed ? 0 : 1);
