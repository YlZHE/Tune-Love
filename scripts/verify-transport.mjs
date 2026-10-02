// This opt-in verifier changes real playback using the widget's visible buttons.
// It never controls a host/plugin, edits settings, or sends global media keys.
import { chromium, expect } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import { installTransportGuard } from "./transport-guard.mjs";

const args = process.argv.slice(2);
const sourceId = args[args.indexOf("--source-id") + 1];
if (!args.includes("--allow-playback-changes") || !args.includes("--source-id") || !sourceId || sourceId.startsWith("--")) {
  throw new Error("Explicit scope required: --allow-playback-changes --source-id <current source ID>");
}
const out = `artifacts/transport-native-${Date.now()}`;
await mkdir(out, { recursive: true });
const browser = await chromium.connectOverCDP("http://127.0.0.1:19224");
const page = browser.contexts().flatMap(context => context.pages()).find(candidate => candidate.url() === "http://tauri.localhost/");
if (!page) { await browser.close(); throw new Error("Expected the debug helper main window"); }
const errors = [];
page.on("pageerror", error => errors.push(error.message));
const report = { output: out, sourceId, passed: false, errors, actions: [] };
const read = () => page.evaluate(() => window.__TAURI_INTERNALS__.invoke("get_now_playing"));
const settings = () => page.evaluate(() => ({ colors: localStorage.getItem("helper-colors-v1"), background: localStorage.getItem("helper-background-v1") }));
const key = track => JSON.stringify([track.sourceId, track.title, track.artist, track.album]);
const compact = snapshot => ({ generation: snapshot.targetGeneration, sourceId: snapshot.track?.sourceId,
  title: snapshot.track?.title, artist: snapshot.track?.artist, position: snapshot.track?.positionSeconds,
  status: snapshot.track?.playbackStatus, controls: snapshot.track?.transport });
const initialSettings = await settings();
let initial;
let snapshot;
let playbackTouched = false;
await page.evaluate(installTransportGuard, sourceId);

async function current() {
  const value = await read();
  if (value.status !== "ready" || value.track?.sourceId !== sourceId || Date.now() - value.capturedAtMs > 8000)
    throw new Error("Displayed media source changed or expired; no further playback action permitted");
  return value;
}
async function click(name) {
  const before = await current();
  await page.locator(".music-window").hover({ position: { x: 18, y: 45 } });
  const button = page.getByRole("button", { name, exact: true });
  await expect(button).toBeEnabled();
  const expected = { action: { "播放": "play", "暂停": "pause", "上一首": "previous", "下一首": "next" }[name],
    sessionId: before.track.transport.sessionId, sourceId, trackKey: key(before.track), targetGeneration: before.targetGeneration };
  const index = await page.evaluate(expected => {
    const state = window.__transportVerification;
    state.expected = expected;
    return state.records.length;
  }, expected);
  playbackTouched = true;
  await button.click();
  report.actions.push({ name, before: compact(before) });
  await expect.poll(() => page.evaluate(index => window.__transportVerification.records[index]?.outcome ?? "pending", index),
    { timeout: 7000 }).not.toBe("pending");
  const result = await page.evaluate(index => window.__transportVerification.records[index], index);
  expect(result.outcome, JSON.stringify(result)).toBe("ok");
  report.actions.at(-1).result = result.outcome;
  await expect(page.locator(".transport-controls")).toHaveAttribute("aria-busy", "false", { timeout: 7000 });
  // A stale-target/unsupported request must not be mistaken for success.
  await expect(page.getByRole("status")).toHaveCount(0, { timeout: 300 });
}
async function awaitState(status) {
  await expect.poll(async () => {
    snapshot = await current(); return snapshot.track.playbackStatus;
  }, { timeout: 8000 }).toBe(status);
  await expect(page.locator(".transport-primary")).toHaveAttribute("aria-label", status === "playing" ? "暂停" : "播放");
  await page.mouse.move(0, 0);
  await expect(page.getByText(status === "playing" ? "正在播放" : "已暂停", { exact: true })).toBeVisible();
}

try {
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  initial = await current();
  report.initial = compact(initial);
  const controls = initial.track.transport;
  if (!controls?.canNext || !controls?.canPrevious) throw new Error("Current player does not advertise both skip operations");
  if (!["playing", "paused"].includes(initial.track.playbackStatus)) throw new Error("Expected playing or paused media");
  if (initial.track.playbackStatus === "paused") { await click("播放"); await awaitState("playing"); }
  await click("暂停"); await awaitState("paused");
  const pausedPosition = snapshot.track.positionSeconds;
  const pausedTime = snapshot.capturedAtMs;
  await expect.poll(async () => {
    const value = await current();
    expect(value.track.playbackStatus).toBe("paused");
    expect(Math.abs(value.track.positionSeconds - pausedPosition)).toBeLessThan(0.2);
    return value.capturedAtMs - pausedTime;
  }, { timeout: 5000 }).toBeGreaterThan(1400);
  await page.screenshot({ path: `${out}/paused.png` });
  await click("播放"); await awaitState("playing");
  const resumed = snapshot.track.positionSeconds;
  await expect.poll(async () => (await current()).track.positionSeconds, { timeout: 5000 }).toBeGreaterThan(resumed + 1);
  report.pauseResume = { realState: true, pausedClockStable: true, resumedClockAdvances: true };
  await page.screenshot({ path: `${out}/resumed.png` });
  const beforeNext = await current();
  if (key(beforeNext.track) !== key(initial.track)) throw new Error("Song changed before skip test; stop without further skipping");
  await click("下一首");
  await expect.poll(async () => { snapshot = await current(); return key(snapshot.track); }, { timeout: 8000 }).not.toBe(key(beforeNext.track));
  report.next = compact(snapshot);
  await expect(page.getByRole("heading", { name: snapshot.track.title, exact: true })).toBeVisible();
  await page.screenshot({ path: `${out}/next.png` });
  await click("上一首");
  await expect.poll(async () => { snapshot = await current(); return key(snapshot.track); }, { timeout: 8000 }).toBe(key(initial.track));
  report.previous = compact(snapshot);
  await expect(page.getByRole("heading", { name: initial.track.title, exact: true })).toBeVisible();
  await page.screenshot({ path: `${out}/previous.png` });
  report.originalTrackRestored = true;
  report.passed = true;
} catch (error) {
  report.failure = String(error);
  await page.screenshot({ path: `${out}/failure.png` }).catch(() => {});
} finally {
  // Restore playing/paused only for the same source. Do not guess at playlist
  // navigation, change shuffle/repeat, or seek after an unsuccessful skip.
  if (initial && playbackTouched) {
    try {
      const value = await current();
      if (value.track.playbackStatus !== initial.track.playbackStatus) {
        await click(initial.track.playbackStatus === "playing" ? "播放" : "暂停");
        await awaitState(initial.track.playbackStatus);
      }
    } catch (error) { report.restoreFailure = String(error); report.passed = false; }
  }
  try {
    const final = await current();
    report.final = compact(final);
    report.playbackStateRestored = !!initial && final.track.playbackStatus === initial.track.playbackStatus;
    report.originalTrackRestored = !!initial && key(final.track) === key(initial.track);
    report.settingsUnchanged = JSON.stringify(await settings()) === JSON.stringify(initialSettings);
    await page.mouse.move(0, 0);
    await page.evaluate(() => { if (document.activeElement instanceof HTMLElement) document.activeElement.blur(); });
  } catch (error) { report.finalFailure = String(error); report.passed = false; }
  try { report.instrumentation = await page.evaluate(() => window.__transportVerification.restore()); }
  catch (error) { report.instrumentationFailure = String(error); report.passed = false; }
  if (!report.settingsUnchanged || !report.playbackStateRestored || !report.originalTrackRestored || errors.length
    || !report.instrumentation?.restored || !report.instrumentation?.intact || report.instrumentation?.blocked.length
    || report.instrumentation?.records.some(record => record.outcome !== "ok")) report.passed = false;
  await writeFile(`${out}/verification.json`, JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report, null, 2));
  await browser.close();
}
process.exitCode = report.passed ? 0 : 1;
