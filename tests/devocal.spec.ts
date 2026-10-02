import { test, expect, type Page } from "@playwright/test";

const OFF = { phase: "off", held: false, latencyMs: null, loadRatio: null, fallbackReason: null,
  sessionOverridden: false, inputSilent: false, error: null };

// Mocks the native boundary for the main window. `window.devocal` is the status
// the fake engine reports; commands move it the way the real backend would.
async function prepare(page: Page, view = "/", initial: Record<string, unknown> = {}) {
  await page.addInitScript(({ initial, OFF }) => {
    const state = window as any;
    state.isTauri = true;
    state.devocal = { ...OFF, ...initial };
    state.devocalRequests = [];
    state.statusPolls = 0;
    state.commandDelayMs = 0;
    state.commandFailure = null;
    state.commandErrorMessage = null;
    state.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "main" } },
      invoke: async (command: string, args: any) => {
        if (command === "get_devocal_status") { state.statusPolls++; return structuredClone(state.devocal); }
        if (command === "devocal_command") {
          state.devocalRequests.push(args.request);
          await new Promise(resolve => setTimeout(resolve, state.commandDelayMs));
          if (state.commandErrorMessage) throw new Error(state.commandErrorMessage);
          if (state.commandFailure) throw state.commandFailure;
          const action = args.request.action;
          if (action === "enable") state.devocal = { ...state.devocal, phase: "devocal", held: true, latencyMs: 45.4 };
          if (action === "disable") state.devocal = { ...state.devocal, phase: "passthrough", held: true, latencyMs: null };
          if (action === "release") state.devocal = { ...OFF };
          return structuredClone(state.devocal);
        }
        if (command === "get_now_playing") return { status: "idle", track: null, capturedAtMs: Date.now() };
        if (command === "get_audio_level" || command === "get_key_detection") return null;
        if (command === "plugin:window|is_always_on_top") return true;
        if (command === "autotune_command") return { ok: true, state: { phase: "disconnected", connectionId: null,
          target: null, capabilities: [], instanceCount: 0, delivery: null, error: null, audioVerified: false } };
        throw new Error(`Unexpected IPC: ${command}`);
      },
    };
  }, { initial, OFF });
  await page.goto(view);
}

const reveal = (page: Page) => page.locator(".music-window").hover({ position: { x: 18, y: 45 } });
const requests = (page: Page) => page.evaluate(() => (window as any).devocalRequests as { action: string }[]);

test("the toggle sends enable then disable and follows the reported status", async ({ page }) => {
  await prepare(page); await reveal(page);
  const button = page.getByRole("button", { name: "开启去人声", exact: true });
  await expect(button).toHaveAttribute("aria-pressed", "false");
  await button.click();
  const on = page.getByRole("button", { name: "关闭去人声", exact: true });
  await expect(on).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByText("去人声中 · 延迟 45 ms", { exact: true })).toBeVisible();
  expect(await requests(page)).toEqual([{ action: "enable" }]);
  await on.click();
  await expect(page.getByRole("button", { name: "开启去人声", exact: true })).toHaveAttribute("aria-pressed", "false");
  expect(await requests(page)).toEqual([{ action: "enable" }, { action: "disable" }]);
  await expect(page.getByText(/去人声中/)).toHaveCount(0);
  await expect(page.getByText("原声直通", { exact: true })).toBeVisible();
});

test("the status text is a polite live region", async ({ page }) => {
  await prepare(page, "/", { phase: "devocal", held: true, latencyMs: 30 }); await reveal(page);
  const label = page.getByText("去人声中 · 延迟 30 ms", { exact: true });
  await expect(label).toBeVisible();
  await expect(label).toHaveAttribute("aria-live", "polite");
  // The compact 468 px window must keep the label clear of the transport buttons.
  const text = (await label.boundingBox())!;
  const transport = (await page.locator(".transport-controls").boundingBox())!;
  expect(text.x + text.width).toBeLessThanOrEqual(transport.x);
});

test("the button shows pressed while enabling and ignores clicks until the request ends", async ({ page }) => {
  await prepare(page);
  await page.evaluate(() => { (window as any).commandDelayMs = 600; });
  await reveal(page);
  await page.getByRole("button", { name: "开启去人声", exact: true }).click();
  const pending = page.getByRole("button", { name: "关闭去人声", exact: true });
  await expect(pending).toHaveAttribute("aria-pressed", "true");
  await pending.click({ clickCount: 3, delay: 20 });
  await expect(page.getByText("去人声中 · 延迟 45 ms", { exact: true })).toBeVisible();
  expect(await requests(page)).toEqual([{ action: "enable" }]);
});

test("a rejected enable restores the button and tells the user", async ({ page }) => {
  await prepare(page);
  await page.evaluate(() => { (window as any).commandFailure = "engine_unavailable: spawn failed"; });
  await reveal(page);
  await page.getByRole("button", { name: "开启去人声", exact: true }).click();
  await expect(page.getByRole("status")).toContainText("去人声未能切换");
  await expect(page.getByRole("button", { name: "开启去人声", exact: true })).toHaveAttribute("aria-pressed", "false");
  expect(await requests(page)).toEqual([{ action: "enable" }]);
});

test("a rejected enable shows the Error message when there is one", async ({ page }) => {
  await prepare(page);
  await page.evaluate(() => { (window as any).commandErrorMessage = "引擎还没准备好"; });
  await reveal(page);
  await page.getByRole("button", { name: "开启去人声", exact: true }).click();
  await expect(page.getByRole("status")).toContainText("引擎还没准备好");
});

test("a warning stays visible without hover but a neutral label waits for hover", async ({ page }) => {
  await prepare(page, "/", { phase: "devocal", held: true, latencyMs: 45.4, sessionOverridden: true });
  const warning = page.getByText("播放器音量被调整，请调本应用音量", { exact: true });
  await expect(warning).toBeVisible();
  await expect(warning).toHaveAttribute("aria-live", "polite");
  await expect(warning).toHaveCSS("opacity", "1");
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "0");
  // Inside the 448 px card, clear of the title row and of the controls row.
  const text = (await warning.boundingBox())!;
  const card = (await page.locator(".music-window").boundingBox())!;
  const controls = (await page.locator(".footer-controls").boundingBox())!;
  const titlebar = (await page.locator(".titlebar").boundingBox())!;
  expect(text.x).toBeGreaterThanOrEqual(card.x);
  expect(text.x + text.width).toBeLessThanOrEqual(card.x + card.width);
  expect(text.y).toBeGreaterThanOrEqual(titlebar.y + titlebar.height);
  expect(text.y + text.height).toBeLessThanOrEqual(controls.y + 1);
  await page.screenshot({ path: "artifacts/devocal-warning-no-hover.png" });
  // Switching to a neutral state removes the warning; its text is hidden until hover.
  await page.evaluate(() => { (window as any).devocal = { ...(window as any).devocal, sessionOverridden: false }; });
  const neutral = page.getByText("去人声中 · 延迟 45 ms", { exact: true });
  await expect(page.locator(".devocal-warning")).toHaveText("");
  // The neutral label lives in the controls row, which is transparent until hover.
  await expect(neutral).toBeAttached();
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "0");
  await reveal(page);
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
  await expect(neutral).toBeVisible();
});

test("only one polite live region carries text at a time", async ({ page }) => {
  await prepare(page, "/", { phase: "unavailable", error: "model_not_found" });
  await expect(page.getByText("未找到去人声模型", { exact: true })).toHaveCount(1);
  await expect.poll(() => page.locator(".devocal-warning, .devocal-status").evaluateAll(els => els.filter(el => el.textContent).length)).toBe(1);
  await page.evaluate(() => { (window as any).devocal = { ...(window as any).devocal, phase: "passthrough", held: true, error: null }; });
  await expect(page.getByText("原声直通", { exact: true })).toHaveCount(1);
  await expect.poll(() => page.locator(".devocal-warning, .devocal-status").evaluateAll(els => els.filter(el => el.textContent).length)).toBe(1);
  await expect(page.getByText("未找到去人声模型")).toHaveCount(0);
});

test("the status is polled while the window is open", async ({ page }) => {
  await prepare(page);
  await expect.poll(() => page.evaluate(() => (window as any).statusPolls)).toBeGreaterThanOrEqual(3);
  await page.evaluate(() => { (window as any).devocal = { ...(window as any).devocal, phase: "unavailable", error: "engine_unavailable: x" }; });
  await reveal(page);
  await expect(page.getByText("去人声引擎无法启动", { exact: true })).toBeVisible();
});

test("the accessible note no longer calls vocal removal a preview", async ({ page }) => {
  await prepare(page);
  await expect(page.locator(".controls-accessible-note", { hasText: "去人声会接管当前播放器的声音输出" })).toHaveCount(1);
  await expect(page.getByText(/去人声仍为前端预览/)).toHaveCount(0);
});

test("settings: release player is disabled until the player is held, then sends release", async ({ page }) => {
  await page.setViewportSize({ width: 760, height: 600 });
  await prepare(page, "/?view=settings");
  const section = page.getByRole("region", { name: "去人声" });
  await expect(section.getByRole("heading", { name: "去人声", exact: true })).toBeVisible();
  await expect(section.getByText(/音量合成器里播放器那一栏接近 0 是正常的/)).toBeVisible();
  await expect(section.getByRole("button", { name: "释放播放器", exact: true })).toBeDisabled();
  await page.evaluate(() => { (window as any).devocal = { ...(window as any).devocal, phase: "devocal", held: true, latencyMs: 20 }; });
  const release = section.getByRole("button", { name: "释放播放器", exact: true });
  await expect(release).toBeEnabled();
  await release.click();
  await expect(release).toBeDisabled();
  expect(await requests(page)).toEqual([{ action: "release" }]);
});
