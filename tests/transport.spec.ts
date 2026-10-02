import { test, expect, type Page } from "@playwright/test";

async function prepare(page: Page) {
  await page.addInitScript(() => {
    const state = window as any;
    state.isTauri = true;
    state.requests = [];
    state.rejectTransport = null;
    state.media = { status: "ready", targetGeneration: 1, capturedAtMs: Date.now(), track: {
      sourceId: "player.exe", source: "Folia", title: "Song A", artist: "Artist", album: "Album",
      artworkDataUrl: null, sourceIconDataUrl: null, positionSeconds: 42, durationSeconds: 180,
      playbackStatus: "playing", playbackRate: 1, updatedAtMs: Date.now(),
      transport: { sessionId: "player.exe", canPlay: true, canPause: true, canPrevious: true, canNext: true },
    } };
    state.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "main" } },
      invoke: async (command: string, args: unknown) => {
        if (command === "get_now_playing") return structuredClone({ ...state.media, capturedAtMs: Date.now() });
        if (command === "plugin:window|is_always_on_top") return true;
        if (command === "get_audio_level") return null;
        if (command === "control_media") {
          state.requests.push(args);
          return new Promise((resolve, reject) => { state.finishTransport = () => state.rejectTransport ? reject({ code: state.rejectTransport }) : resolve(null); });
        }
        throw new Error(`Unexpected command: ${command}`);
      },
    };
  });
  await page.goto("/");
  await page.locator(".music-window").hover({ position: { x: 18, y: 45 } });
  await expect(page.getByRole("button", { name: "暂停", exact: true })).toBeEnabled();
}

test("pause targets the displayed session, prevents duplicate submissions and waits for real playback state", async ({ page }) => {
  await prepare(page);
  const pause = page.getByRole("button", { name: "暂停", exact: true });
  await pause.click();
  await expect.poll(() => page.evaluate(() => (window as any).requests)).toEqual([{ request: {
    action: "pause", sessionId: "player.exe", sourceId: "player.exe", trackKey: '["player.exe","Song A","Artist","Album"]', targetGeneration: 1,
  } }]);
  await expect(pause).toBeDisabled();
  await expect(page.getByRole("button", { name: "上一首", exact: true })).toBeDisabled();
  await expect(page.getByRole("button", { name: "下一首", exact: true })).toBeDisabled();
  await page.evaluate(() => (window as any).finishTransport());
  await expect(pause).toBeEnabled();
  await expect(page.getByRole("button", { name: "播放", exact: true })).toHaveCount(0);
  await page.evaluate(() => { (window as any).media.track.playbackStatus = "paused"; });
  const play = page.getByRole("button", { name: "播放", exact: true });
  await expect(play).toBeEnabled();
  await play.click();
  await expect.poll(() => page.evaluate(() => (window as any).requests.at(-1).request.action)).toBe("play");
  await page.evaluate(() => { (window as any).media.track.playbackStatus = "playing"; (window as any).finishTransport(); });
  await expect(pause).toBeEnabled();
});

test("late errors remain ignored when a player leaves and returns to the same song", async ({ page }) => {
  await prepare(page);
  await page.getByRole("button", { name: "暂停", exact: true }).click();
  await expect.poll(() => page.evaluate(() => (window as any).requests.length)).toBe(1);
  await page.evaluate(() => {
    (window as any).media.track.title = "Song B"; (window as any).media.targetGeneration = 2;
  });
  await expect(page.getByRole("heading", { name: "Song B", exact: true })).toBeVisible();
  await page.evaluate(() => {
    (window as any).media.track.title = "Song A"; (window as any).media.targetGeneration = 3;
  });
  await expect(page.getByRole("heading", { name: "Song A", exact: true })).toBeVisible();
  await page.evaluate(() => { (window as any).rejectTransport = "failed"; (window as any).finishTransport(); });
  await expect(page.getByRole("button", { name: "暂停", exact: true })).toBeEnabled();
  await expect(page.getByRole("status")).toHaveCount(0, { timeout: 300 });
});

test("skip controls dispatch explicit actions and use the updated song identity", async ({ page }) => {
  await prepare(page);
  await page.getByRole("button", { name: "下一首", exact: true }).click();
  await expect.poll(() => page.evaluate(() => (window as any).requests[0]?.request.action)).toBe("next");
  await page.evaluate(() => { (window as any).media.track.title = "Song B"; (window as any).media.targetGeneration = 2; (window as any).finishTransport(); });
  await expect(page.getByRole("heading", { name: "Song B", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "上一首", exact: true }).click();
  await expect.poll(() => page.evaluate(() => (window as any).requests.at(-1).request)).toEqual({
    action: "previous", sessionId: "player.exe", sourceId: "player.exe", trackKey: '["player.exe","Song B","Artist","Album"]', targetGeneration: 2,
  });
});

test("capabilities and missing sessions disable unavailable actions", async ({ page }) => {
  await prepare(page);
  await page.evaluate(() => { Object.assign((window as any).media.track.transport, { canPrevious: false, canPause: false }); });
  await expect(page.getByRole("button", { name: "上一首", exact: true })).toBeDisabled();
  await expect(page.getByRole("button", { name: "暂停", exact: true })).toBeDisabled();
  await expect(page.getByRole("button", { name: "下一首", exact: true })).toBeEnabled();
  await page.evaluate(() => { (window as any).media = { status: "idle", track: null }; });
  for (const name of ["上一首", "播放", "下一首"]) await expect(page.getByRole("button", { name, exact: true })).toBeDisabled();
  expect(await page.evaluate(() => (window as any).requests)).toEqual([]);
});

for (const code of ["stale_target", "unsupported", "timeout", "failed"]) {
  test(`a ${code} response releases the buttons and never fakes playback`, async ({ page }) => {
    await prepare(page);
    await page.getByRole("button", { name: "暂停", exact: true }).click();
    await expect.poll(() => page.evaluate(() => (window as any).requests.length)).toBe(1);
    await page.evaluate(code => { (window as any).rejectTransport = code; (window as any).finishTransport(); }, code);
    await expect(page.getByRole("status")).toBeVisible();
    await expect(page.getByRole("status")).not.toContainText("前端预览");
    await expect(page.getByRole("button", { name: "暂停", exact: true })).toBeEnabled();
    await expect(page.getByRole("button", { name: "播放", exact: true })).toHaveCount(0);
  });
}

test("late errors from a previous player cannot overwrite the new player's state", async ({ page }) => {
  await prepare(page);
  await page.getByRole("button", { name: "暂停", exact: true }).click();
  await expect.poll(() => page.evaluate(() => (window as any).requests.length)).toBe(1);
  await page.evaluate(() => {
    const state = window as any;
    state.media.track.sourceId = "other.exe"; state.media.track.transport.sessionId = "other.exe";
    state.media.track.title = "Other Song";
  });
  await expect(page.getByRole("heading", { name: "Other Song", exact: true })).toBeVisible();
  await page.evaluate(() => { (window as any).rejectTransport = "failed"; (window as any).finishTransport(); });
  await expect(page.getByRole("button", { name: "暂停", exact: true })).toBeEnabled();
  await expect(page.getByRole("status")).toHaveCount(0, { timeout: 300 });
});
