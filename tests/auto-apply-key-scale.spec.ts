import { test, expect, type BrowserContext, type Page } from "@playwright/test";

type Target = { key: number | null; scale: "major" | "minor" | "chromatic"; candidate?: { key: number; scale: "major" | "minor" } | null;
  uncoveredNotes?: number[]; evidenceSeconds: number; source: "analysis" | "cache" } | null;

// Controlled IPC boundary: a playing track, a connected bridge that exposes
// key/scale, and a key-detection snapshot whose recommendation the test sets.
async function prepare(context: BrowserContext) {
  await context.addInitScript(() => {
    const w = window as any;
    w.isTauri = true;
    w.bridgeRequests = [];
    w.keyTarget = null;
    const track = {
      sourceId: "player.exe", source: "Test Player", title: "Song", artist: "Artist", album: "Album",
      artworkDataUrl: null, sourceIconDataUrl: null, positionSeconds: 30, durationSeconds: 180,
      playbackStatus: "playing", playbackRate: 1, updatedAtMs: Date.now(),
    };
    w.media = { status: "ready", capturedAtMs: Date.now(), targetGeneration: 4, track };
    const trackKey = () => JSON.stringify([w.media.track.sourceId, w.media.track.title, w.media.track.artist, w.media.track.album]);
    w.bridgeState = {
      phase: "ready", connectionId: "ui-session", error: null, audioVerified: false, delivery: null, instanceCount: 1,
      target: { pid: 234, processName: "FixtureHost.exe", pluginName: "Auto-Tune Pro", profileId: "autotune-pro-38c42d0b-x64" },
      capabilities: ["retune", "flex", "vibrato", "humanize", "key", "scale"],
    };
    w.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
      invoke: async (command: string, args: any) => {
        if (command === "get_now_playing") return structuredClone({ ...w.media, capturedAtMs: Date.now(),
          track: { ...w.media.track, updatedAtMs: Date.now() } });
        if (command === "get_key_detection") return { sourceId: w.media.track.sourceId, trackKey: trackKey(),
          targetGeneration: w.media.targetGeneration, status: "analyzing", key: null, updatedAtMs: 100,
          autotuneTarget: w.keyTarget };
        if (command === "get_audio_level") return null;
        if (command === "plugin:window|is_always_on_top") return true;
        if (command === "get_devocal_status") return { phase:"off",held:false,latencyMs:null,loadRatio:null,fallbackReason:null,sessionOverridden:false,inputSilent:false,error: null };
        if (command === "plugin:window|is_visible") return true;
        if (command !== "autotune_command") return null;
        const request = args.request;
        w.bridgeRequests.push(request);
        if (request.op === "apply") {
          if (w.failWrite) return { ok: false, state: structuredClone(w.bridgeState), error: "管道连接已断开" };
          w.bridgeState = { ...w.bridgeState, delivery: { stage: "cached", sequence: request.sequence } };
        }
        return { ok: true, state: structuredClone(w.bridgeState), candidates: [], skippedCount: 0 };
      },
    };
  });
}

// With __TAURI_INTERNALS__ present the settings button invokes open_settings,
// so the settings view is opened as a second same-origin page instead.
async function settings(context: BrowserContext) {
  const result = await context.newPage();
  await result.setViewportSize({ width: 760, height: 540 });
  await result.goto("/?view=settings");
  return result;
}

// Rust-shaped target: candidate and uncoveredNotes are always present.
const setTarget = (page: Page, target: Target) => page.evaluate(value => {
  (window as any).keyTarget = value && { candidate: null, uncoveredNotes: [], ...value };
}, target);
const title = (page: Page) => page.locator(".key-title-text.is-detected:not([aria-hidden='true']):not([inert])");
const tooltip = async (page: Page) => {
  await page.mouse.move(300, 220); // leave first, so every call is a fresh hover
  await page.locator(".key-title-hit").hover();
  return page.locator(".warm-tooltip");
};
const applies = (page: Page) => page.evaluate(() => (window as any).bridgeRequests
  .filter((r: any) => r.op === "apply").map((r: any) => r.values));

test("the switch defaults off, persists across windows and reloads, and writes each pair once", async ({ page, context }) => {
  test.slow(); // many reloads and fixed waits; runs close to the default 30 s under load
  await prepare(context); await page.goto("/");
  await expect(page.getByRole("heading", { name: "Song", exact: true })).toBeVisible();
  await expect(page.locator(".autotune-target-status")).toHaveCount(0);
  await expect(page.locator(".brand")).toContainText("Tune Love");
  await setTarget(page, { key: 6, scale: "minor", evidenceSeconds: 12, source: "analysis" });
  await expect(title(page)).toHaveText("F# Minor", { timeout: 2500 });
  await expect(page.locator(".autotune-target-status")).toHaveCount(0);
  await expect(await tooltip(page)).toHaveText("已分析 12 秒。自动写入已关闭，可在设置中开启。");
  await page.waitForTimeout(1200);
  expect(await applies(page)).toEqual([]);

  const panel = await settings(context);
  const toggle = panel.getByRole("switch", { name: "自动写入 Key/Scale" });
  await expect(toggle).not.toBeChecked();
  await toggle.check();
  expect(await panel.evaluate(() => localStorage.getItem("helper-auto-apply-v1"))).toBe(JSON.stringify({ enabled: true }));
  await expect.poll(() => applies(page)).toEqual([{ key: "F#", scale: "Minor" }]);
  await expect(await tooltip(page)).toHaveText("已分析 12 秒。已写入当前连接的插件。");
  await expect(page.locator('[aria-label="写入失败"]')).toHaveCount(0);
  // The seconds in the open tooltip follow the analysis while the pair stays the same, and the
  // unchanged pair is not written again (the applies assertion below).
  await setTarget(page, { key: 6, scale: "minor", evidenceSeconds: 15, source: "analysis" });
  await expect(page.locator(".warm-tooltip")).toHaveText("已分析 15 秒。已写入当前连接的插件。", { timeout: 2500 });
  await page.screenshot({ path: "artifacts/auto-apply-status.png" });
  await panel.locator(".auto-apply-settings").evaluate(el => el.scrollIntoView({ block: "center" }));
  await panel.screenshot({ path: "artifacts/auto-apply-settings.png" });
  await page.waitForTimeout(2200);
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }]);

  // A missing target (no valid snapshot) writes nothing and the title falls back to the brand;
  // a new Major/Minor pair is written once.
  await setTarget(page, null);
  await expect(page.locator(".brand")).toContainText("Tune Love", { timeout: 2500 });
  await expect(page.locator(".key-title-trigger")).toHaveCount(0);
  await setTarget(page, { key: 11, scale: "major", evidenceSeconds: 20, source: "analysis" });
  await expect(title(page)).toHaveText("B Major", { timeout: 2500 });
  await expect.poll(() => applies(page)).toEqual([{ key: "F#", scale: "Minor" }, { key: "B", scale: "Major" }]);

  await panel.reload();
  await expect(toggle).toBeChecked();
  await toggle.uncheck();
  await expect(await tooltip(page)).toHaveText("已分析 20 秒。自动写入已关闭，可在设置中开启。");
  await setTarget(page, { key: 6, scale: "minor", evidenceSeconds: 30, source: "analysis" });
  await expect(title(page)).toHaveText("F# Minor", { timeout: 2500 });
  await page.waitForTimeout(1200);
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }, { key: "B", scale: "Major" }]);
  await panel.reload();
  await expect(toggle).not.toBeChecked();
});

test("Chromatic is the title and is written as a scale-only or key plus Chromatic pair, with the uncertainty in the tooltip", async ({ page, context }) => {
  await prepare(context);
  await context.addInitScript(() => localStorage.setItem("helper-auto-apply-v1", JSON.stringify({ enabled: true })));
  await page.goto("/");
  await setTarget(page, { key: null, scale: "chromatic", evidenceSeconds: 0, source: "analysis" });
  await expect(title(page)).toHaveText("Chromatic", { timeout: 2500 });
  await expect.poll(() => applies(page)).toEqual([{ scale: "Chromatic" }]);
  await expect(await tooltip(page)).toHaveText("已分析 0 秒。刚开始分析，暂用 Chromatic。已写入当前连接的插件。");

  await setTarget(page, { key: 5, scale: "chromatic", candidate: { key: 5, scale: "minor" }, uncoveredNotes: [1, 11],
    evidenceSeconds: 14, source: "analysis" });
  await expect(title(page)).toHaveText("F Chromatic", { timeout: 2500 });
  await expect.poll(() => applies(page)).toEqual([{ scale: "Chromatic" }, { key: "F", scale: "Chromatic" }]);
  const tip = await tooltip(page);
  await expect(tip).toContainText("拿不准：C# 与 B 都常用，暂用 Chromatic。排第一的候选是 F Minor。");
  await expect(tip).toContainText("已分析 14 秒。");

  await setTarget(page, { key: 5, scale: "chromatic", candidate: { key: 5, scale: "minor" }, uncoveredNotes: [1],
    evidenceSeconds: 15, source: "cache" });
  await expect(tip).toContainText("拿不准：C# 常用但不在候选调内，暂用 Chromatic。排第一的候选是 F Minor。");
  await expect(tip).toContainText("来自上次播放的分析结果（本次已分析 15 秒，足够后会重新确认）。");

  await setTarget(page, { key: 5, scale: "chromatic", candidate: { key: 5, scale: "minor" }, evidenceSeconds: 16, source: "cache" });
  await expect(tip).toContainText("暂用 Chromatic。排第一的候选是 F Minor。");
  await expect(tip).not.toContainText("拿不准");

  // Major/Minor targets carry no uncertainty sentence.
  await setTarget(page, { key: 5, scale: "minor", evidenceSeconds: 40, source: "analysis" });
  await expect(title(page)).toHaveText("F Minor", { timeout: 2500 });
  await expect(tip).not.toContainText("Chromatic");
});

test("a failed write is reported once and not retried, and a new song writes again", async ({ page, context }) => {
  await prepare(context);
  await context.addInitScript(() => {
    localStorage.setItem("helper-auto-apply-v1", JSON.stringify({ enabled: true }));
    (window as any).failWrite = true;
  });
  await page.goto("/");
  await setTarget(page, { key: 6, scale: "minor", evidenceSeconds: 12, source: "analysis" });
  await expect(title(page)).toHaveText("F# Minor", { timeout: 2500 });
  await expect(page.locator('[aria-label="写入失败"]')).toBeVisible({ timeout: 2500 });
  await expect(await tooltip(page)).toContainText("写入失败，不会自动重试。");
  await expect(page.getByRole("status").filter({ hasText: "未能自动写入 Key/Scale" })).toBeVisible();
  await page.waitForTimeout(2200);
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }]);

  await page.evaluate(() => {
    const w = window as any;
    w.failWrite = false;
    w.media = { ...w.media, targetGeneration: 5, track: { ...w.media.track, title: "Next" } };
  });
  await expect(page.getByRole("heading", { name: "Next", exact: true })).toBeVisible();
  await expect(page.locator('[aria-label="写入失败"]')).toHaveCount(0, { timeout: 2500 });
  await expect(await tooltip(page)).toContainText("已写入当前连接的插件。");
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }, { key: "F#", scale: "Minor" }]);
});
