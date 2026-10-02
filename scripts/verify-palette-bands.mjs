// Genuine Tauri IPC and renderer only. No playback control, synthetic audio,
// source replacement or user DAW access. Restores settings and leaves the app open.
import { chromium, expect } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
const out = `artifacts/palette-bands-native-${Date.now()}`;
await mkdir(out, { recursive: true });
const browser = await chromium.connectOverCDP("http://127.0.0.1:19224");
const context = browser.contexts()[0];
await expect.poll(() => context.pages().some(p => p.url() === "http://tauri.localhost/"), { timeout: 15000 }).toBe(true);
const page = context.pages().find(p => p.url() === "http://tauri.localhost/");
const errors = [];
page.on("pageerror", e => errors.push(e.message));
const report = { output: out, observations: [], pixelHashes: [], errors };
const original = await page.evaluate(() => localStorage.getItem("helper-colors-v1"));
let settings;
try {
  await expect(page.locator(".music-window")).toBeVisible();
  await expect(page.locator(".audio-strands canvas")).toBeVisible({ timeout: 15000 });
  for (let i = 0; i < 36; i++) {
    const sample = await page.evaluate(async () => {
      const audio = await window.__TAURI_INTERNALS__.invoke("get_audio_level");
      const media = await window.__TAURI_INTERNALS__.invoke("get_now_playing");
      const track = media.track;
      const canvas = document.querySelector(".audio-strands canvas");
      const gl = canvas?.getContext("webgl2");
      const program = gl?.getParameter(gl.CURRENT_PROGRAM);
      const location = program && gl.getUniformLocation(program, "uBands");
      return { audio, age: Date.now() - audio.updatedAtMs, source: track?.source,
        aligned: audio.sourceId === track?.sourceId && audio.trackKey === JSON.stringify([track?.sourceId, track?.title, track?.artist, track?.album]),
        rendered: location ? Array.from(gl.getUniform(program, location)) : null };
    });
    report.observations.push(sample);
    await page.waitForTimeout(70);
  }
  const active = report.observations.filter(o => o.aligned && o.age >= -50 && o.age <= 400 && o.audio.status === "capturing" && o.audio.rms > 0.00001);
  expect(active.length).toBeGreaterThan(3);
  for (const o of active) {
    expect(o.audio.bands).toHaveLength(3); expect(o.rendered).toHaveLength(3);
    expect(o.audio.bands.every(v => Number.isFinite(v) && v >= 0 && v <= 1)).toBe(true);
  }
  expect(active.some(o => o.rendered.some(v => v > 0))).toBe(true);
  expect(active.some(o => new Set(o.audio.bands).size > 1)).toBe(true);
  report.bandVariants = [0, 1, 2].map(i => new Set(active.map(o => o.audio.bands[i])).size);
  for (let i = 0; i < 6; i++) {
    const png = await page.locator(".audio-strands").screenshot({ path: `${out}/bands-${i}.png` });
    report.pixelHashes.push(createHash("sha256").update(png).digest("hex"));
    await page.waitForTimeout(120);
  }
  expect(new Set(report.pixelHashes).size).toBeGreaterThan(1);
  await page.getByRole("button", { name: "设置", exact: true }).click();
  await expect.poll(() => context.pages().some(p => p.url().includes("view=settings"))).toBe(true);
  settings = context.pages().find(p => p.url().includes("view=settings"));
  settings.on("pageerror", e => errors.push(e.message));
  const automatic = settings.getByRole("switch", { name: "从封面提取主色" });
  await automatic.check();
  await expect(settings.getByRole("button", { name: /^封面颜色/ })).toHaveCount(0);
  await expect(settings.getByText("已从当前封面提取", { exact: true })).toBeVisible();
  await expect.poll(async () => (await page.locator(".music-window").getAttribute("data-primary-color")) ===
    (await settings.locator(".active-color-value").innerText()).toLowerCase()).toBe(true);
  const palette = () => page.locator(".music-window").evaluate(el => [0, 1, 2].map(i => el.style.getPropertyValue(`--strand-${i}`)));
  const initial = await palette();
  expect(initial).toHaveLength(3);
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", initial[0]);
  report.automaticAccentWithoutColorSelection = true;
  report.coverPalette = initial;
  await settings.screenshot({ path: `${out}/settings-cover.png` });
  await page.screenshot({ path: `${out}/widget-cover.png` });
  await automatic.uncheck();
  await settings.getByRole("textbox", { name: "主色值" }).fill("#2255cc");
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#2255cc");
  report.manualPalette = await palette();
  expect(new Set(report.manualPalette).size).toBe(3);
  const bounds = await page.locator(".audio-strands").boundingBox();
  expect([bounds.width, bounds.height]).toEqual([46, 22]);
  report.bounds = bounds;
  const label = page.locator(".playback-label");
  const position = await label.evaluate(el => getComputedStyle(el).backgroundPosition);
  await page.waitForTimeout(250);
  report.textShine = (await label.evaluate(el => getComputedStyle(el).backgroundPosition)) !== position;
  expect(report.textShine).toBe(true);
  expect(errors).toEqual([]);
  report.passed = true;
} catch (error) {
  report.passed = false; report.failure = String(error);
} finally {
  // Dispatch only on the writer; the other WebView receives the real storage event.
  const writer = settings ?? page;
  await writer.evaluate(raw => {
    if (raw === null) localStorage.removeItem("helper-colors-v1");
    else localStorage.setItem("helper-colors-v1", raw);
    window.dispatchEvent(new StorageEvent("storage", { key: "helper-colors-v1", newValue: raw }));
  }, original);
  await expect.poll(() => page.evaluate(() => localStorage.getItem("helper-colors-v1"))).toBe(original);
  report.settingsRestored = true;
  if (settings) await settings.getByRole("button", { name: "关闭设置" }).click();
  await writeFile(`${out}/verification.json`, JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ ...report, observations: report.observations.length }, null, 2));
}
// Disconnect the test client without closing the user's native application.
process.exit(report.passed ? 0 : 1);
