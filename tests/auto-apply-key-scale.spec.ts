import { test, expect, type BrowserContext, type Page } from "@playwright/test";

type Target = { key: number; scale: "major" | "minor"; evidenceSeconds: number; source: "analysis" | "cache" } | null;

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
        if (command === "get_model_status") return [];
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

const setTarget = (page: Page, target: Target) => page.evaluate(value => { (window as any).keyTarget = value; }, target);
const applies = (page: Page) => page.evaluate(() => (window as any).bridgeRequests
  .filter((r: any) => r.op === "apply").map((r: any) => r.values));

test("the switch defaults off, persists across windows and reloads, and writes each pair once", async ({ page, context }) => {
  await prepare(context); await page.goto("/");
  await expect(page.getByRole("heading", { name: "Song", exact: true })).toBeVisible();
  await expect(page.locator(".autotune-target-status")).toHaveCount(0);
  await setTarget(page, { key: 6, scale: "minor", evidenceSeconds: 12, source: "analysis" });
  const status = page.locator(".autotune-target-status");
  await expect(status).toHaveText("建议 F♯ 小调 · 未写入", { timeout: 2500 });
  await page.waitForTimeout(1200);
  expect(await applies(page)).toEqual([]);

  const panel = await settings(context);
  const toggle = panel.getByRole("switch", { name: "自动写入 Key/Scale" });
  await expect(toggle).not.toBeChecked();
  await toggle.check();
  expect(await panel.evaluate(() => localStorage.getItem("helper-auto-apply-v1"))).toBe(JSON.stringify({ enabled: true }));
  await expect.poll(() => applies(page)).toEqual([{ key: "F#", scale: "Minor" }]);
  await expect(status).toHaveText("建议 F♯ 小调 · 已写入");
  await page.screenshot({ path: "artifacts/auto-apply-status.png" });
  await panel.locator(".auto-apply-settings").evaluate(el => el.scrollIntoView({ block: "center" }));
  await panel.screenshot({ path: "artifacts/auto-apply-settings.png" });
  await page.waitForTimeout(2200);
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }]);

  // A missing target (no evidence yet) writes nothing and hides the status;
  // a new Major/Minor pair is written once.
  await setTarget(page, null);
  await expect(status).toHaveCount(0, { timeout: 2500 });
  await setTarget(page, { key: 11, scale: "major", evidenceSeconds: 20, source: "analysis" });
  await expect(status).toHaveText("建议 B 大调 · 已写入", { timeout: 2500 });
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }, { key: "B", scale: "Major" }]);

  await panel.reload();
  await expect(toggle).toBeChecked();
  await toggle.uncheck();
  await expect(status).toHaveText("建议 B 大调 · 未写入");
  await setTarget(page, { key: 6, scale: "minor", evidenceSeconds: 30, source: "analysis" });
  await expect(status).toHaveText("建议 F♯ 小调 · 未写入", { timeout: 2500 });
  await page.waitForTimeout(1200);
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }, { key: "B", scale: "Major" }]);
  await panel.reload();
  await expect(toggle).not.toBeChecked();
});

test("a failed write is reported once and not retried, and a new song writes again", async ({ page, context }) => {
  await prepare(context);
  await context.addInitScript(() => {
    localStorage.setItem("helper-auto-apply-v1", JSON.stringify({ enabled: true }));
    (window as any).failWrite = true;
  });
  await page.goto("/");
  await setTarget(page, { key: 6, scale: "minor", evidenceSeconds: 12, source: "analysis" });
  const status = page.locator(".autotune-target-status");
  await expect(status).toHaveText("建议 F♯ 小调 · 写入失败", { timeout: 2500 });
  await expect(page.getByRole("status").filter({ hasText: "未能自动写入 Key/Scale" })).toBeVisible();
  await page.waitForTimeout(2200);
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }]);

  await page.evaluate(() => {
    const w = window as any;
    w.failWrite = false;
    w.media = { ...w.media, targetGeneration: 5, track: { ...w.media.track, title: "Next" } };
  });
  await expect(page.getByRole("heading", { name: "Next", exact: true })).toBeVisible();
  await expect(status).toHaveText("建议 F♯ 小调 · 已写入", { timeout: 2500 });
  expect(await applies(page)).toEqual([{ key: "F#", scale: "Minor" }, { key: "F#", scale: "Minor" }]);
});
