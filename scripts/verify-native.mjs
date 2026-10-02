// Run only against a test instance of this app launched with WebView2 CDP on 19224.
// Uses genuine Tauri IPC and Windows media data. Never controls the media player.
import { chromium, expect } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
await expect.poll(async () => {
  try { return (await fetch("http://127.0.0.1:19224/json/version")).ok; }
  catch { return false; }
}, { timeout: 15000 }).toBe(true);
const browser = await chromium.connectOverCDP("http://127.0.0.1:19224");
const page = browser.contexts().flatMap(c => c.pages()).find(p => p.url().includes("tauri.localhost") && !p.url().includes("view=settings"));
if (!page) throw new Error("Our Tauri test window was not found");
const errors = [];
page.on("pageerror", e => errors.push(e.message));
await expect(page.locator(".music-window")).toBeVisible();
await expect(page.locator(".has-track")).toBeVisible({ timeout: 12000 });
// TrackTransition keeps the outgoing frame mounted briefly; wait for the
// initial reveal to settle before querying strict metadata locators.
await expect(page.locator(".artist")).toHaveCount(1, { timeout: 12000 });
await expect(page.locator(".title-line h1")).toHaveCount(1, { timeout: 12000 });
const info = await page.locator(".song-information").innerText();
const firstProgress = await page.getByRole("progressbar").getAttribute("aria-valuetext");
const wasPlaying = await page.getByText("正在播放", { exact: true }).count() > 0;
if (wasPlaying) {
  await expect.poll(() => page.getByRole("progressbar").getAttribute("aria-valuetext"), { timeout: 6000 }).not.toBe(firstProgress);
}
const cover = await page.locator(".album-cover img").evaluate(img => ({ complete: img.complete, width: img.naturalWidth }));
if (!cover.complete || !cover.width) throw new Error("Native album cover did not load");
await mkdir("artifacts", { recursive: true });
await expect(page.locator(".music-window")).toHaveAttribute("data-theme", "dark");
await expect(page.getByRole("button", { name: /[深浅]色外观/ })).toHaveCount(0);
await expect(page.locator(".source img")).toBeVisible({ timeout: 15000 });
const sourceIdentity = await page.locator(".source").evaluate(element => {
  const icon = element.querySelector("img");
  return { name: element.textContent.trim(), iconLoaded: !!icon?.complete && icon.naturalWidth > 0,
    width: icon?.naturalWidth, renderedWidth: icon?.getBoundingClientRect().width,
    embeddedPng: icon?.src.startsWith("data:image/png;base64,"), genericIcon: !!element.querySelector("svg") };
});
if (!sourceIdentity.iconLoaded || !sourceIdentity.embeddedPng || sourceIdentity.genericIcon) throw new Error("Native source icon did not load");
if (/\.exe$|[\\/]/i.test(sourceIdentity.name)) throw new Error("Source label contains an executable extension or path");
if (process.env.HELPER_EXPECT_SOURCE_NAME && sourceIdentity.name !== process.env.HELPER_EXPECT_SOURCE_NAME) throw new Error("Native source name did not match the expected application");
const audioSamples = [];
for (let index = 0; index < 24; index++) {
  const observation = await page.evaluate(async () => {
    const [media, audio] = await Promise.all([
      window.__TAURI_INTERNALS__.invoke("get_now_playing"),
      window.__TAURI_INTERNALS__.invoke("get_audio_level"),
    ]);
    const track = media.track;
    return {
      audio, ageMs: Date.now() - audio.updatedAtMs,
      playing: media.status === "ready" && track?.playbackStatus === "playing",
      aligned: !!track && audio.sourceId === track.sourceId && audio.trackKey === JSON.stringify([track.sourceId, track.title, track.artist, track.album]),
    };
  });
  audioSamples.push(observation);
  await page.waitForTimeout(80);
}
const freshAudio = audioSamples.filter(sample => sample.playing && sample.aligned && sample.audio.status === "capturing"
  && sample.ageMs >= -50 && sample.ageMs <= 400
  && [sample.audio.rms, sample.audio.peak, sample.audio.level].every(value => Number.isFinite(value) && value >= 0 && value <= 1));
const activeAudio = freshAudio.filter(sample => sample.audio.rms > 0.00001);
const expectsAudio = process.env.HELPER_EXPECT_AUDIO === "1";
if (expectsAudio && activeAudio.length < 3) throw new Error("Expected fresh, source-bound real PCM levels, but fewer than three active samples arrived");
if (expectsAudio && new Set(activeAudio.map(sample => sample.audio.level)).size < 2) throw new Error("Expected varying PCM-derived levels, but all fresh levels were identical");
const audioPixels = [];
if (activeAudio.length >= 3 && await page.locator(".audio-strands canvas").count()) {
  for (let index = 0; index < 5; index++) {
    const bytes = await page.locator(".audio-strands canvas").screenshot({ path: `artifacts/native-audio-${index}.png` });
    const pixels = await page.evaluate(async data => {
      const image = new Image();
      image.src = `data:image/png;base64,${data}`;
      await image.decode();
      const canvas = document.createElement("canvas");
      canvas.width = image.naturalWidth; canvas.height = image.naturalHeight;
      const context = canvas.getContext("2d");
      context.drawImage(image, 0, 0);
      const rgba = context.getImageData(0, 0, canvas.width, canvas.height).data;
      let hash = 2166136261;
      const colors = new Set();
      for (let offset = 0; offset < rgba.length; offset += 4) {
        colors.add(`${rgba[offset]},${rgba[offset + 1]},${rgba[offset + 2]}`);
        for (let channel = 0; channel < 4; channel++) hash = Math.imul(hash ^ rgba[offset + channel], 16777619) >>> 0;
      }
      return { width: canvas.width, height: canvas.height, colors: colors.size, hash };
    }, bytes.toString("base64"));
    audioPixels.push(pixels);
    await page.waitForTimeout(150);
  }
}
const audioReducedMotion = await page.evaluate(() => matchMedia("(prefers-reduced-motion: reduce)").matches);
if (expectsAudio && !audioReducedMotion && (audioPixels.length < 3 || !audioPixels.every(sample => sample.colors > 4) || new Set(audioPixels.map(sample => sample.hash)).size < 2)) {
  throw new Error("Real PCM arrived but native Strands did not produce nonempty changing pixels");
}
const audioCapture = {
  expected: expectsAudio, sourceId: freshAudio[0]?.audio.sourceId ?? null,
  samples: audioSamples.length, freshBoundSamples: freshAudio.length, activeSamples: activeAudio.length,
  rmsMin: activeAudio.length ? Math.min(...activeAudio.map(sample => sample.audio.rms)) : 0,
  rmsMax: activeAudio.length ? Math.max(...activeAudio.map(sample => sample.audio.rms)) : 0,
  levelVariants: new Set(activeAudio.map(sample => sample.audio.level)).size,
  reducedMotion: audioReducedMotion, renderedPixels: audioPixels,
};
const textShine = { active: wasPlaying && !audioReducedMotion, screenshotVariants: 0, positionVariants: 0, stableBounds: true };
if (textShine.active) {
  const label = page.locator(".playback-state > span:last-child");
  await expect(label).toHaveText("正在播放");
  await expect(label).not.toHaveCSS("background-image", "none");
  const bounds = await label.boundingBox();
  const hashes = new Set();
  const positions = new Set();
  for (let index = 0; index < 8; index++) {
    const bytes = await label.screenshot({ path: `artifacts/native-shine-${index}.png` });
    hashes.add(createHash("sha256").update(bytes).digest("hex"));
    positions.add(await label.evaluate(el => getComputedStyle(el).backgroundPosition));
    expect(await label.boundingBox()).toEqual(bounds);
    await page.waitForTimeout(160);
  }
  textShine.screenshotVariants = hashes.size;
  textShine.positionVariants = positions.size;
  if (hashes.size < 3 || positions.size < 3) throw new Error("Playing text did not visibly sweep in the native window");
}
await page.screenshot({ path: "artifacts/native-dark.png" });
await page.locator(".source").screenshot({ path: "artifacts/native-source.png" });
const settingsTrigger = page.getByRole("button", { name: "设置", exact: true });
for (const selector of ['.track-copy[aria-hidden="false"] .artist', ".source", '.track-copy[aria-hidden="false"] .title-line h1']) {
  const trigger = page.locator(selector);
  const content = (await trigger.innerText()).trim();
  if (selector === ".source") {
    await page.mouse.move(0, 0);
    await trigger.focus();
  } else await trigger.hover();
  const hint = page.locator(".warm-tooltip");
  await expect(hint).toHaveText(content);
  await expect(hint.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
  await expect(hint.locator(".warm-tooltip__arrow")).toHaveCount(0);
  if (selector !== ".source") {
    await expect.poll(async () => {
      const center = await trigger.evaluate(element => {
        const range = document.createRange();
        range.selectNodeContents(element);
        const text = range.getBoundingClientRect();
        return (text.left + Math.min(text.right, element.getBoundingClientRect().right)) / 2;
      });
      const bounds = await hint.boundingBox();
      return bounds ? Math.abs(bounds.x + bounds.width / 2 - center) : Infinity;
    }).toBeLessThan(2);
  }
  await page.mouse.move(40, 210);
  await expect(hint).toHaveCount(0);
}
const reducedMotion = await page.evaluate(() => matchMedia("(prefers-reduced-motion: reduce)").matches);
await page.getByRole("button", { name: /^(取消置顶|窗口置顶)$/ }).hover();
const tooltip = page.locator(".warm-tooltip");
await expect(tooltip).toBeVisible();
await expect(tooltip.locator(".warm-tooltip__arrow")).toHaveCount(0);
await expect(tooltip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
await expect(tooltip.locator(".warm-tooltip__box")).toHaveCSS("opacity", "1");
const tooltipId = await tooltip.getAttribute("id");
const tooltipBefore = await tooltip.boundingBox();
await page.evaluate(() => {
  window.__tooltipFrames = [];
  const capture = () => {
    const tip = document.querySelector(".warm-tooltip");
    const bounds = tip?.getBoundingClientRect();
    const box = tip?.querySelector(".warm-tooltip__box");
    const matrix = new DOMMatrix(box ? getComputedStyle(box).transform : undefined);
    window.__tooltipFrames.push({ x: bounds?.x, width: bounds?.width, tilt: Math.atan2(matrix.b, matrix.a) * 180 / Math.PI });
    if (window.__tooltipFrames.length < 40) requestAnimationFrame(capture);
  };
  const tooltip = document.querySelector(".warm-tooltip");
  const observer = new MutationObserver(() => {
    if (!tooltip.textContent?.includes("设置")) return;
    observer.disconnect();
    capture();
  });
  observer.observe(tooltip, { childList: true, characterData: true, subtree: true });
});
await settingsTrigger.hover();
await expect(tooltip).toHaveText("设置");
await expect(tooltip).toHaveAttribute("id", tooltipId);
await expect.poll(() => page.evaluate(() => window.__tooltipFrames.length)).toBe(40);
const tooltipAfter = await tooltip.boundingBox();
const tooltipFrames = await page.evaluate(() => window.__tooltipFrames);
if (tooltipFrames.some(frame => frame.x === undefined)) throw new Error("Shared tooltip disappeared between controls");
if (!reducedMotion) {
  if (!tooltipFrames.some(frame => frame.x > tooltipBefore.x + 2 && frame.x < tooltipAfter.x - 2)) throw new Error("Tooltip did not glide between buttons");
  if (!tooltipFrames.some(frame => frame.width < tooltipBefore.width - 2 && frame.width > tooltipAfter.width + 2)) throw new Error("Tooltip did not smoothly resize");
  if (!tooltipFrames.some(frame => Math.abs(frame.tilt) > 0.1)) throw new Error("Tooltip lean was not active");
}
await page.screenshot({ path: "artifacts/native-warm-tooltip.png" });
const warmTooltip = { blackSurface: true, shared: true, arrow: false, metadata: ["title", "artist", "source"], metadataAnchoredToText: true, reducedMotion, animatedTravel: !reducedMotion, lean: !reducedMotion, before: tooltipBefore, after: tooltipAfter };
await settingsTrigger.click();
const findSettings = () => browser.contexts().flatMap(context => context.pages()).find(candidate => candidate.url().includes("view=settings"));
await expect.poll(() => !!findSettings(), { timeout: 15000 }).toBe(true);
const settingsPage = findSettings();
settingsPage.on("pageerror", e => errors.push(e.message));
await expect(settingsPage.getByRole("heading", { name: "设置", exact: true })).toBeVisible();
const settingsVisible = () => page.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|is_visible", { label: "settings" }));
await expect.poll(settingsVisible).toBe(true);
const settingsBounds = await settingsPage.locator(".settings-page").boundingBox();
if (!settingsBounds || settingsBounds.width < 700 || settingsBounds.height < 500) throw new Error("Settings is not a separate spacious window");
const settingsResizable = await settingsPage.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|is_resizable", { label: "settings" }));
if (!settingsResizable) throw new Error("Settings is not resizable");
await settingsPage.screenshot({ path: "artifacts/native-settings-page.png" });
const originalColors = await settingsPage.evaluate(() => localStorage.getItem("helper-colors-v1"));
let colorSettings;
try {
  const automatic = settingsPage.getByRole("switch", { name: "从封面提取主色" });
  await automatic.uncheck();
  await settingsPage.getByRole("textbox", { name: "主色值" }).fill("#55aaff");
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#55aaff");
  await expect(page.locator(".brand > svg")).toHaveCSS("color", "rgb(85, 170, 255)");
  await page.evaluate(() => {
    window.__accentFrames = [];
    window.__accentFramesDone = false;
    const started = performance.now();
    const capture = () => {
      window.__accentFrames.push({
        icon: getComputedStyle(document.querySelector(".brand > svg")).color,
        progress: getComputedStyle(document.querySelector(".rt-ProgressIndicator")).backgroundColor,
      });
      if (performance.now() - started < 1200) requestAnimationFrame(capture);
      else window.__accentFramesDone = true;
    };
    requestAnimationFrame(capture);
  });
  await settingsPage.getByRole("button", { name: "珊瑚", exact: true }).click();
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#f4a58c");
  await expect.poll(() => page.evaluate(() => window.__accentFramesDone)).toBe(true);
  const accentFrames = await page.evaluate(() => window.__accentFrames);
  const accentTransition = {
    reducedMotion,
    iconColors: new Set(accentFrames.map(frame => frame.icon)).size,
    progressColors: new Set(accentFrames.map(frame => frame.progress)).size,
  };
  if (!reducedMotion && (accentTransition.iconColors <= 3 || accentTransition.progressColors <= 3)) {
    throw new Error("Accent colors jumped without intermediate frames");
  }
  await expect(page.locator(".brand > svg")).toHaveCSS("color", "rgb(244, 165, 140)");
  await expect(page.locator(".rt-ProgressIndicator")).toHaveCSS("background-color", "rgb(244, 165, 140)");
  await settingsPage.screenshot({ path: "artifacts/native-settings-colors.png" });
  await automatic.check();
  await expect(automatic).toHaveCSS("background-color", "rgba(0, 0, 0, 0)");
  const switchTrack = await automatic.evaluate(el => {
    const track = getComputedStyle(el, "::before");
    return { radius: parseFloat(track.borderTopLeftRadius), height: parseFloat(track.height) };
  });
  if (switchTrack.radius < switchTrack.height / 2) throw new Error("Color switch track is not fully rounded");
  await expect(settingsPage.getByText("已从当前封面提取", { exact: true })).toBeVisible();
  await expect(page.locator(".music-window")).toHaveAttribute("data-color-mode", "cover");
  await expect.poll(async () => {
    const display = (await settingsPage.locator(".active-color-value").innerText()).toLowerCase();
    return (await page.locator(".music-window").getAttribute("data-primary-color")) === display;
  }).toBe(true);
  await settingsPage.screenshot({ path: "artifacts/native-settings-cover-colors.png" });
  await automatic.uncheck();
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#f4a58c");
  await settingsPage.reload();
  await expect(settingsPage.getByRole("textbox", { name: "主色值" })).toHaveValue("#f4a58c");
  colorSettings = { manual: true, presets: true, crossWindowSync: true, coverExtraction: true, restoresManualColor: true, saved: true, roundedSwitchWithoutSquareBackground: true, accentTransition };
} finally {
  await settingsPage.evaluate(raw => {
    if (raw === null) localStorage.removeItem("helper-colors-v1");
    else localStorage.setItem("helper-colors-v1", raw);
    window.dispatchEvent(new StorageEvent("storage", { key: "helper-colors-v1", newValue: raw }));
  }, originalColors);
}
await settingsTrigger.click();
// Verify that even concurrent open requests cannot create duplicate settings windows.
await page.evaluate(() => Promise.all([
  window.__TAURI_INTERNALS__.invoke("open_settings"),
  window.__TAURI_INTERNALS__.invoke("open_settings"),
]));
const labels = await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|get_all_windows"));
if (labels.filter(label => label === "settings").length !== 1) throw new Error("Duplicate settings windows");
await settingsPage.getByRole("button", { name: "关闭设置" }).click();
await expect.poll(settingsVisible).toBe(false);
await expect(page.locator(".music-window")).toBeVisible();
await settingsTrigger.click();
await expect.poll(settingsVisible).toBe(true);
await settingsPage.keyboard.press("Escape");
await expect.poll(settingsVisible).toBe(false);
const originallyPinned = await page.getByRole("button", { name: "取消置顶", exact: true }).count() > 0;
await page.getByRole("button", { name: originallyPinned ? "取消置顶" : "窗口置顶", exact: true }).click();
await expect(page.getByRole("button", { name: originallyPinned ? "窗口置顶" : "取消置顶", exact: true })).toBeVisible();
await page.getByRole("button", { name: originallyPinned ? "窗口置顶" : "取消置顶", exact: true }).click();
await expect(page.getByRole("button", { name: originallyPinned ? "取消置顶" : "窗口置顶", exact: true })).toBeVisible();
const metrics = await page.locator(".music-window").evaluate(el => ({
  width: el.clientWidth, height: el.clientHeight,
  overflowsX: el.scrollWidth > el.clientWidth, overflowsY: el.scrollHeight > el.clientHeight,
}));
if (metrics.overflowsX || metrics.overflowsY) throw new Error("The widget overflows");
if (metrics.width / metrics.height < 1.8) throw new Error("The new horizontal layout was not built");
const report = { info, sourceIdentity, audioCapture, textShine, firstProgress, latestProgress: await page.getByRole("progressbar").getAttribute("aria-valuetext"), cover, metrics, errors, nativePinToggle: true, fixedDarkAppearance: true, warmTooltip, colorSettings, settingsWindow: { bounds: settingsBounds, resizable: settingsResizable, singleton: true, reopen: true, escape: true, closeButton: true } };
await writeFile("artifacts/native-verification.json", JSON.stringify(report, null, 2));
console.log(JSON.stringify(report, null, 2));
if (errors.length) throw new Error(errors.join("\n"));
// Close with settings still open: the OS process must exit, not leave an orphan window.
await settingsTrigger.click();
await expect.poll(settingsVisible).toBe(true);
await page.getByRole("button", { name: "关闭", exact: true }).click();
process.exit(0);
