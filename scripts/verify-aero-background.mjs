// Genuine native IPC and WebGPU rendering. No audio/media fixtures, recording,
// player controls or DAW access. Only the background preference is exercised.
// Default: restore preferences. --leave-enabled: keep the preview enabled.
import { chromium, expect } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";

const leaveEnabled = process.argv.includes("--leave-enabled");
const out = `artifacts/aero-shards-native-${Date.now()}`;
await mkdir(out, { recursive: true });
const browser = await chromium.connectOverCDP("http://127.0.0.1:19224");
const pages = () => browser.contexts().flatMap(context => context.pages());
await expect.poll(() => pages().some(page => page.url() === "http://tauri.localhost/"), { timeout: 15000 }).toBe(true);
const page = pages().find(page => page.url() === "http://tauri.localhost/");
const errors = [];
page.on("pageerror", error => errors.push(error.message));
page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
const report = { output: out, leaveEnabled, errors, observations: [], pixelHashes: [], passed: false };
const original = await page.evaluate(() => ({
  colors: localStorage.getItem("helper-colors-v1"),
  background: localStorage.getItem("helper-background-v1"),
}));
let settings = pages().find(page => page.url().includes("view=settings"));
const settingsWasVisible = settings ? await settings.evaluate(() =>
  window.__TAURI_INTERNALS__.invoke("plugin:window|is_visible", { label: "settings" })) : false;

async function snapshot() {
  return page.evaluate(async () => {
    const [audio, media] = await Promise.all([
      window.__TAURI_INTERNALS__.invoke("get_audio_level"),
      window.__TAURI_INTERNALS__.invoke("get_now_playing"),
    ]);
    const track = media.track;
    const node = document.querySelector(".music-window");
    return {
      audio, age: Date.now() - audio.updatedAtMs, source: track?.source,
      playbackStatus: track?.playbackStatus,
      aligned: audio.sourceId === track?.sourceId &&
        audio.trackKey === JSON.stringify([track?.sourceId, track?.title, track?.artist, track?.album]),
      frame: window.__aeroProbe.frames.at(-1),
      finishing: window.__aeroProbe.finishing,
      shardInstances: window.__aeroProbe.shardInstances,
      colors: node && [0, 1, 2].map(i => getComputedStyle(node).getPropertyValue(`--strand-${i}`)),
      hidden: document.hidden,
    };
  });
}

try {
  await expect(page.locator(".music-window")).toBeVisible();
  await page.getByRole("button", { name: "设置", exact: true }).click();
  await expect.poll(() => pages().some(page => page.url().includes("view=settings"))).toBe(true);
  settings = pages().find(page => page.url().includes("view=settings"));
  settings.on("pageerror", error => errors.push(error.message));
  const toggle = settings.getByRole("switch", { name: "音频响应背景" });
  await toggle.uncheck();
  await expect(page.locator(".audio-background")).toHaveCount(0);

  // All wrapped GPU operations still call the real methods. Restore in finally.
  await page.evaluate(() => {
    if (!window.GPUQueue || !window.GPUAdapter || !window.GPUDevice)
      throw new Error("Native WebGPU unavailable; cannot validate dynamic background");
    const probe = window.__aeroProbe = { created: 0, destroyed: 0, writes: 0, frames: [], finishing: [], shardInstances: 0 };
    const write = GPUQueue.prototype.writeBuffer;
    const request = GPUAdapter.prototype.requestDevice;
    const destroy = GPUDevice.prototype.destroy;
    const draw = GPURenderPassEncoder.prototype.draw;
    GPUQueue.prototype.writeBuffer = function(buffer, offset, data, ...rest) {
      const result = write.call(this, buffer, offset, data, ...rest);
      if (buffer.label === "view.sharedUniform") {
        const bytes = ArrayBuffer.isView(data)
          ? data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) : data;
        const values = new Float32Array(bytes);
        probe.writes++;
        probe.frames.push({ at: performance.now(), viewport: Array.from(values.slice(0, 4)),
          shape: Array.from(values.slice(4, 8)), material: Array.from(values.slice(48, 52)),
          palette: Array.from(values.slice(60, 72)) });
        if (probe.frames.length > 240) probe.frames.shift();
      }
      if (buffer.label === "post.sharedUniform") {
        const bytes = ArrayBuffer.isView(data)
          ? data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) : data;
        probe.finishing = Array.from(new Float32Array(bytes).slice(8, 12));
      }
      return result;
    };
    GPURenderPassEncoder.prototype.draw = function(vertices, instances, ...rest) {
      if (instances && instances > 1) probe.shardInstances = instances;
      return draw.call(this, vertices, instances, ...rest);
    };
    GPUAdapter.prototype.requestDevice = async function(...args) {
      const device = await request.apply(this, args); probe.created++; return device;
    };
    GPUDevice.prototype.destroy = function() { probe.destroyed++; return destroy.call(this); };
    probe.restore = () => {
      GPUQueue.prototype.writeBuffer = write;
      GPUAdapter.prototype.requestDevice = request;
      GPUDevice.prototype.destroy = destroy;
      GPURenderPassEncoder.prototype.draw = draw;
      delete window.__aeroProbe;
    };
  });

  await toggle.check();
  await expect(page.locator(".audio-background .aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  await expect(page.locator(".audio-strands canvas")).toBeVisible();
  await settings.screenshot({ path: `${out}/settings.png` });
  const before = await page.locator(".audio-strands").boundingBox();
  expect([before.width, before.height]).toEqual([46, 22]);
  expect((await page.locator(".playback-progress-track").boundingBox()).height).toBe(6);
  const hit = await page.getByRole("button", { name: "设置", exact: true }).evaluate(el => {
    const bounds = el.getBoundingClientRect();
    return el.contains(document.elementFromPoint(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2));
  });
  expect(hit).toBe(true);
  report.foregroundUnchanged = true;

  for (let i = 0; i < 30; i++) {
    report.observations.push(await snapshot());
    if (i % 4 === 0) {
      const png = await page.locator(".audio-background canvas").evaluate(canvas => canvas.toDataURL());
      report.pixelHashes.push(createHash("sha256").update(png).digest("hex"));
    }
    await page.waitForTimeout(90);
  }
  const active = report.observations.filter(o => o.aligned && o.age >= -50 && o.age <= 400 &&
    o.playbackStatus === "playing" && o.audio.status === "capturing" && o.audio.rms > 0.00001);
  expect(active.length, "need fresh genuine current-player audio").toBeGreaterThan(5);
  expect(active.every(o => !o.hidden && o.frame && o.audio.bands.every(v => Number.isFinite(v) && v >= 0 && v <= 1))).toBe(true);
  expect(active.some(o => o.frame.shape[1] > 1)).toBe(true);
  for (const o of active) {
    expect(o.frame.viewport[2]).toBeCloseTo(1.15, 4);
    expect(o.frame.shape[0]).toBeCloseTo(1, 4);
    expect(o.frame.shape[1]).toBeGreaterThanOrEqual(1);
    expect(o.frame.shape[1]).toBeLessThanOrEqual(1.05001);
    expect(o.shardInstances).toBeGreaterThan(1000);
    expect(o.finishing[0]).toBeGreaterThanOrEqual(0.5199);
    expect(o.finishing[0]).toBeLessThanOrEqual(1.0401);
  }
  report.bandVariants = [0, 1, 2].map(i => new Set(active.map(o => o.audio.bands[i])).size);
  report.uniformVariants = {
    depth: new Set(active.map(o => o.frame.shape[1])).size,
    turbulence: new Set(active.map(o => o.frame.shape[2])).size,
    glow: new Set(active.map(o => o.frame.material[3])).size,
  };
  expect(Object.values(report.uniformVariants).every(count => count > 1)).toBe(true);
  const range = values => [Math.min(...values), Math.max(...values)];
  report.parameterRanges = {
    scale: range(active.map(o => o.frame.viewport[2])),
    spread: range(active.map(o => o.frame.shape[0])),
    depth: range(active.map(o => o.frame.shape[1])),
    glow: range(active.map(o => o.frame.material[3])),
    bloom: range(active.map(o => o.finishing[0])),
    shardInstances: range(active.map(o => o.shardInstances)),
  };
  // Derive speed from actual GPU travel over wider intervals to suppress
  // writeBuffer timestamp jitter without reaching into renderer internals.
  const rendered = await page.evaluate(() => window.__aeroProbe.frames);
  const speeds = [];
  for (let i = 12; i < rendered.length; i += 12) {
    const start = rendered[i - 12], end = rendered[i];
    const duration = (end.at - start.at) / 1000;
    if (duration > 0.1 && duration < 0.8)
      speeds.push((end.viewport[3] - start.viewport[3]) / (duration * 0.34));
  }
  expect(speeds.length).toBeGreaterThan(2);
  expect(speeds.every(speed => speed >= 0 && speed <= 0.52)).toBe(true);
  expect(speeds.some(speed => speed > 0.1)).toBe(true);
  report.observedSpeedRange = range(speeds);
  expect(new Set(report.pixelHashes).size).toBeGreaterThan(1);
  const pixels = await page.locator(".audio-background canvas").evaluate(async canvas => {
    const image = new Image(); image.src = canvas.toDataURL(); await image.decode();
    const surface = document.createElement("canvas"); surface.width = image.width; surface.height = image.height;
    const context = surface.getContext("2d"); context.drawImage(image, 0, 0);
    const data = context.getImageData(0, 0, surface.width, surface.height).data;
    const colors = new Set();
    for (let i = 0; i < data.length; i += 4) colors.add(`${data[i]},${data[i + 1]},${data[i + 2]}`);
    return { width: surface.width, height: surface.height, distinctColors: colors.size };
  });
  expect(pixels.distinctColors).toBeGreaterThan(20);
  report.canvas = pixels;
  await page.screenshot({ path: `${out}/widget.png` });

  await toggle.uncheck();
  await expect(page.locator(".audio-background")).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => window.__aeroProbe.created === window.__aeroProbe.destroyed)).toBe(true);
  report.afterDisable = await page.evaluate(() => ({ created: window.__aeroProbe.created, destroyed: window.__aeroProbe.destroyed }));
  expect(report.afterDisable.created).toBeGreaterThan(0);
  await expect(page.locator(".audio-strands canvas")).toBeVisible();
  await toggle.check();
  await expect(page.locator(".aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  report.afterReenable = await page.evaluate(() => ({ created: window.__aeroProbe.created, destroyed: window.__aeroProbe.destroyed }));
  expect(report.afterReenable.created).toBeGreaterThan(report.afterDisable.created);
  expect(report.afterReenable.created - report.afterReenable.destroyed).toBe(1);
  expect(errors).toEqual([]);
  report.passed = true;
} catch (error) {
  report.failure = String(error);
  await page.screenshot({ path: `${out}/failure.png` }).catch(() => {});
} finally {
  try {
    // A failed verification always restores the original setting.
    const wanted = leaveEnabled && report.passed ? JSON.stringify({ enabled: true }) : original.background;
    const writer = settings ?? page;
    await writer.evaluate(raw => {
      if (raw === null) localStorage.removeItem("helper-background-v1");
      else localStorage.setItem("helper-background-v1", raw);
      window.dispatchEvent(new StorageEvent("storage", { key: "helper-background-v1", newValue: raw }));
    }, wanted);
    await expect.poll(() => page.evaluate(() => localStorage.getItem("helper-background-v1"))).toBe(wanted);
    report.backgroundRestoredOrEnabled = true;
    report.colorSettingsUnchanged = original.colors === await page.evaluate(() => localStorage.getItem("helper-colors-v1"));
    expect(report.colorSettingsUnchanged).toBe(true);
    if (settings && !settingsWasVisible) await settings.getByRole("button", { name: "关闭设置" }).click();
  } catch (error) {
    report.passed = false; report.cleanupFailure = String(error);
  }
  try { await page.evaluate(() => window.__aeroProbe?.restore()); report.instrumentationRestored = true; }
  catch (error) { report.passed = false; report.instrumentationFailure = String(error); }
  await writeFile(`${out}/verification.json`, JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ ...report, observations: report.observations.length }, null, 2));
}
// Disconnect the probe only; keep the debug application open for the user.
process.exit(report.passed ? 0 : 1);
