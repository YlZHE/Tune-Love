// Read-only native renderer verification. Uses genuine media IPC, never fixtures
// or player controls; leaves user settings and the debug application unchanged.
import { chromium, expect } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";

const out = `artifacts/progress-star-border-native-${Date.now()}`;
await mkdir(out, { recursive: true });
const browser = await chromium.connectOverCDP("http://127.0.0.1:19224");
const pages = () => browser.contexts().flatMap(context => context.pages());
await expect.poll(() => pages().some(page => page.url() === "http://tauri.localhost/"), { timeout: 15000 }).toBe(true);
const page = pages().find(page => page.url() === "http://tauri.localhost/");
const errors = [];
page.on("pageerror", error => errors.push(error.message));
page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
const report = { output: out, errors, frames: [], passed: false };
const originalSettings = await page.evaluate(() => localStorage.getItem("helper-colors-v1"));

async function readState() {
  return page.evaluate(async () => {
    const media = await window.__TAURI_INTERNALS__.invoke("get_now_playing");
    const track = media.track;
    const bar = document.querySelector(".playback-progress-track");
    const progress = document.querySelector('[role="progressbar"]');
    const lights = [...document.querySelectorAll(".playback-progress-track > [class^=border-gradient-]")];
    return {
      status: media.status, source: track?.source, playbackStatus: track?.playbackStatus,
      songKey: track && JSON.stringify([track.sourceId, track.title, track.artist, track.album]),
      position: track?.positionSeconds, duration: track?.durationSeconds,
      hidden: document.hidden, reducedMotion: matchMedia("(prefers-reduced-motion: reduce)").matches,
      width: bar?.getBoundingClientRect().width, height: bar?.getBoundingClientRect().height,
      value: Number(progress?.getAttribute("aria-valuenow")), valueText: progress?.getAttribute("aria-valuetext"),
      innerWidth: progress?.getBoundingClientRect().width,
      clip: bar && getComputedStyle(bar).overflowX,
      lights: lights.map(light => {
        const style = getComputedStyle(light);
        const animation = light.getAnimations()[0];
        return { layer: light.className, x: new DOMMatrix(style.transform).m41,
          gradient: style.backgroundImage, state: animation?.playState, time: Number(animation?.currentTime) };
      }),
      accent: bar && getComputedStyle(bar).getPropertyValue("--accent"),
    };
  });
}

async function readPixels(png) {
  return page.evaluate(async data => {
    const image = new Image(); image.src = `data:image/png;base64,${data}`;
    await image.decode();
    const canvas = document.createElement("canvas");
    canvas.width = image.naturalWidth; canvas.height = image.naturalHeight;
    const context = canvas.getContext("2d"); context.drawImage(image, 0, 0);
    const rgba = context.getImageData(0, 0, canvas.width, canvas.height).data;
    return { width: canvas.width, height: canvas.height,
      rows: Array.from({ length: canvas.height }, (_, y) =>
        Array.from({ length: canvas.width }, (_, x) => {
          const i = (y * canvas.width + x) * 4;
          return rgba[i] + rgba[i + 1] + rgba[i + 2];
        })) };
  }, png.toString("base64"));
}

try {
  await expect(page.locator(".music-window")).toBeVisible();
  await expect(page.locator(".playback-progress-track > .border-gradient-top")).toBeAttached({ timeout: 15000 });
  await page.waitForTimeout(700);
  const bar = page.locator(".playback-progress-track");
  report.initial = await readState();
  expect(report.initial.status).toBe("ready");
  expect(report.initial.playbackStatus).toBe("playing");
  expect(report.initial.height).toBe(6);
  expect(report.initial.width).toBeGreaterThan(200);
  expect(report.initial.hidden).toBe(false);
  expect(report.initial.reducedMotion).toBe(false);
  expect(report.initial.duration).toBeGreaterThan(0);
  const projectedPercent = report.initial.position / report.initial.duration * 100;
  // IPC and renderer updates are asynchronous; allow at most two seconds' drift.
  expect(Math.abs(report.initial.value - projectedPercent)).toBeLessThanOrEqual(200 / report.initial.duration + 0.1);
  for (let i = 0; i < 42; i++) {
    const state = await readState();
    const png = await bar.screenshot({ path: `${out}/border-${i}.png`, scale: "css" });
    report.frames.push({ ...state, pixels: await readPixels(png) });
    const phase = state.lights[0]?.time % 12000;
    if (!report.widgetCaptured && ((phase > 2400 && phase < 3600) || (phase > 8400 && phase < 9600))) {
      await page.screenshot({ path: `${out}/widget.png` });
      report.widgetCaptured = true;
    }
    await page.waitForTimeout(130);
  }
  expect(report.frames.every(frame => frame.songKey === report.initial.songKey)).toBe(true);
  for (const frame of report.frames) {
    expect(frame.height).toBe(6);
    expect(frame.clip).toBe("hidden");
    expect(frame.width).toBe(report.initial.width);
    expect(frame.innerWidth).toBe(frame.width - 2);
    expect(frame.lights).toHaveLength(2);
    expect(frame.lights.every(light => light.state === "running")).toBe(true);
    expect(Math.abs(frame.lights[0].x + frame.lights[1].x)).toBeLessThan(0.1);
  }
  expect(new Set(report.frames.map(frame => frame.lights[0].x)).size).toBeGreaterThan(6);
  const pixelWidth = report.frames[0].pixels.width;
  const playedLimit = Math.floor((pixelWidth - 2) * Math.min(...report.frames.map(frame => frame.value)) / 100) - 4;
  const unplayedStart = Math.ceil((pixelWidth - 2) * Math.max(...report.frames.map(frame => frame.value)) / 100) + 5;
  const changes = row => Array.from({ length: pixelWidth }, (_, x) => {
    const values = report.frames.map(frame => frame.pixels.rows[row][x]);
    return Math.max(...values) - Math.min(...values);
  });
  report.changedEdgeColumns = [0, 5].map(row => changes(row).slice(5, -5).filter(value => value > 30).length);
  report.changedStableInteriorColumns = [2, 3].map(row => {
    const delta = changes(row);
    return [...delta.slice(5, Math.max(5, playedLimit)), ...delta.slice(unplayedStart, pixelWidth - 5)]
      .filter(value => value > 12).length;
  });
  expect(report.changedEdgeColumns.every(count => count > 8)).toBe(true);
  expect(report.changedStableInteriorColumns).toEqual([0, 0]);
  report.final = await readState();
  expect(report.final.position).toBeGreaterThan(report.initial.position);
  if (!report.widgetCaptured) await page.screenshot({ path: `${out}/widget.png` });
  expect(errors).toEqual([]);
  report.passed = true;
} catch (error) {
  report.failure = String(error);
} finally {
  report.settingsUnchanged = originalSettings === await page.evaluate(() => localStorage.getItem("helper-colors-v1"));
  if (!report.settingsUnchanged) report.passed = false;
  await writeFile(`${out}/verification.json`, JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ ...report, frames: report.frames.length }, null, 2));
}
// Disconnect only the test client. Do not close the user's native application.
process.exit(report.passed ? 0 : 1);
