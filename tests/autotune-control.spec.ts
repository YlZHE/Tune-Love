import { test, expect, type Page } from "@playwright/test";

async function installBridge(page: Page, connected = false) {
  await page.addInitScript(({ connected }) => {
    const w = window as any;
    w.isTauri = true;
    w.bridgeRequests = [];
    const disconnected = { phase: "disconnected", connectionId: null, target: null, capabilities: [], instanceCount: 0, delivery: null, error: null, audioVerified: false };
    const ready = { ...disconnected, phase: "ready", connectionId: "ui-session", target: { pid: 234, processName: "FixtureHost.exe", pluginName: "Auto-Tune Pro", profileId: "autotune-pro-38c42d0b-x64" }, capabilities: ["retune", "flex", "vibrato", "humanize"], instanceCount: 2 };
    w.bridgeState = connected ? ready : disconnected;
    w.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "main" } },
      invoke: async (command: string, args: any) => {
        if (command === "get_now_playing") return { status: "idle", track: null, capturedAtMs: Date.now() };
        if (command === "get_audio_level" || command === "get_key_detection") return null;
        if (command === "plugin:window|is_always_on_top") return true;
        if (command !== "autotune_command") throw new Error(`Unexpected command ${command}`);
        const request = args.request;
        w.bridgeRequests.push(request);
        if (request.op === "connect") w.bridgeState = structuredClone(ready);
        if (request.op === "disconnect") w.bridgeState = structuredClone(disconnected);
        if (request.op === "apply") {
          if (w.failWrite) { w.bridgeState = { ...disconnected, phase: "error", error: "管道连接已断开" }; return { ok: false, state: w.bridgeState, error: "管道连接已断开" }; }
          w.bridgeState = { ...w.bridgeState, delivery: { stage: "cached", sequence: request.sequence } };
        }
        if (request.op === "clear") w.bridgeState = { ...w.bridgeState, delivery: null };
        return { ok: true, state: structuredClone(w.bridgeState), candidates: request.op === "scan" ? [{ candidateId: "candidate-1", pid: 234, processName: "FixtureHost.exe", pluginName: "Auto-Tune Pro", profileId: "autotune-pro-38c42d0b-x64", compatible: true, reason: null }] : undefined, skippedCount: 0 };
      },
    };
  }, { connected });
}

test("settings scan never connects until a target is selected, then connects without publishing defaults", async ({ page }) => {
  await page.setViewportSize({ width: 760, height: 600 });
  await installBridge(page);
  await page.goto("/?view=settings");
  await expect(page.getByText("未连接 Auto-Tune", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "扫描插件", exact: true }).click();
  await expect(page.getByRole("button", { name: "连接", exact: true })).toBeDisabled();
  await page.getByRole("combobox", { name: "Auto-Tune 目标进程" }).click();
  await page.getByRole("option", { name: /FixtureHost/ }).click();
  expect(await page.evaluate(() => (window as any).bridgeRequests.some((r: any) => r.op === "connect"))).toBe(false);
  await page.getByRole("button", { name: "连接", exact: true }).click();
  await expect(page.getByText("已连接 · 调试模式", { exact: true })).toBeVisible();
  await expect(page.getByText(/2 个匹配实例/)).toBeVisible();
  expect(await page.evaluate(() => (window as any).bridgeRequests.filter((r: any) => r.op === "apply"))).toEqual([]);
  await page.getByRole("button", { name: "清空助手缓存", exact: true }).click();
  expect(await page.evaluate(() => (window as any).bridgeRequests.at(-1))).toEqual({ op: "clear", connectionId: "ui-session" });
  await expect(page.getByText("已连接 · 调试模式", { exact: true })).toBeVisible();
  await page.screenshot({ path: "artifacts/autotune-settings-connected.png" });
  await page.getByRole("button", { name: "停止控制", exact: true }).click();
  await expect(page.getByText("未连接 Auto-Tune", { exact: true })).toBeVisible();
});

test("real dial and strength components send only the changed roles, and failures become visible", async ({ page }) => {
  await installBridge(page, true);
  await page.goto("/");
  await page.locator(".music-window").hover({ position: { x: 18, y: 45 } });
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  await expect(page.getByText("已连接 · 调试模式", { exact: true })).toBeVisible();
  const dial = page.getByRole("slider", { name: "Flex-Tune", exact: true });
  await dial.press("ArrowRight");
  await expect.poll(() => page.evaluate(() => (window as any).bridgeRequests.filter((r: any) => r.op === "apply").at(-1)?.values)).toEqual({ flex: .01 });
  await expect(page.getByText("参数待插件接收", { exact: true })).toBeVisible();
  await page.screenshot({ path: "artifacts/autotune-parameters-cached.png" });
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  const strength = page.getByRole("slider", { name: "电音强度", exact: true });
  await strength.press("End");
  await expect.poll(() => page.evaluate(() => (window as any).bridgeRequests.filter((r: any) => r.op === "apply").at(-1)?.values)).toEqual({ retune: 1 });
  await page.evaluate(() => { (window as any).failWrite = true; });
  await strength.press("ArrowLeft");
  await expect(page.getByText("管道连接已断开", { exact: true })).toBeVisible();
  const count = await page.evaluate(() => (window as any).bridgeRequests.filter((r: any) => r.op === "apply").length);
  await strength.press("ArrowLeft");
  expect(await page.evaluate(() => (window as any).bridgeRequests.filter((r: any) => r.op === "apply").length)).toBe(count);
});

test("browser preview provides a clear desktop-only connection state without runtime errors", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", e => errors.push(e.message));
  await page.setViewportSize({ width: 760, height: 600 });
  await page.goto("/?view=settings");
  await expect(page.getByText(/浏览器仅展示界面/)).toBeVisible();
  await expect(page.getByRole("button", { name: "扫描插件", exact: true })).toBeDisabled();
  expect(errors).toEqual([]);
});
