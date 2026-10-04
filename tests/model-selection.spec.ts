import { test, expect, type Page } from "@playwright/test";

const OFF = { phase: "off", held: false, latencyMs: null, loadRatio: null, fallbackReason: null,
  sessionOverridden: false, inputSilent: false, waitingForPlayer: false, error: null,
  modelId: null, device: null, deviceNote: null };
const STEM = "stemgenrt-hop128", BYTE = "bytesep-mobilenet-1s", HT = "htdemucs-ft-vocals-1s";
const missing = (id: string) => ({ id, phase: "missing", receivedBytes: 0, totalBytes: 37_529_132, source: null, path: null, error: null, sourceError: null });
const installed = (id: string) => ({ ...missing(id), phase: "installed", receivedBytes: 37_529_132, source: "origin", path: "C:/m" });

// Mocks the native boundary. The selection is saved before load through localStorage; the
// fake engine reports `window.devocal` and records every devocal_command request.
async function prepare(page: Page, view: string, opts: { selection?: Record<string, string>; devocal?: Record<string, unknown>; models?: Record<string, unknown>[] } = {}) {
  await page.addInitScript(({ selection, devocal, models, OFF, label }) => {
    const state = window as any;
    // Seed once per tab, so a later reload or navigation keeps what the page saved itself.
    if (selection && !sessionStorage.getItem("seeded")) { localStorage.setItem("tune-love.devocal-model", JSON.stringify(selection)); sessionStorage.setItem("seeded", "1"); }
    state.isTauri = true;
    state.devocal = { ...OFF, ...devocal };
    state.models = models;
    state.devocalRequests = [];
    state.openSettingsCalls = [];
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
    let nextId = 1;
    state.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label } },
      transformCallback: () => nextId++,
      invoke: async (command: string, args: any) => {
        if (command === "get_devocal_status") return structuredClone(state.devocal);
        if (command === "devocal_command") {
          state.devocalRequests.push(args.request);
          if (args.request.action === "enable") state.devocal = { ...state.devocal, phase: "devocal", held: true, latencyMs: 45.4, error: null };
          return structuredClone(state.devocal);
        }
        if (command === "get_model_status") return structuredClone(state.models);
        if (command === "open_settings") { state.openSettingsCalls.push(args); return null; }
        if (command === "plugin:event|listen") return nextId++;
        if (command === "plugin:event|unlisten") return null;
        if (command === "get_now_playing") return { status: "idle", track: null, capturedAtMs: Date.now() };
        if (command === "get_audio_level" || command === "get_key_detection") return null;
        if (command === "plugin:window|is_always_on_top") return true;
        if (command === "autotune_command") return { ok: true, state: { phase: "disconnected", connectionId: null,
          target: null, capabilities: [], instanceCount: 0, delivery: null, error: null, audioVerified: false } };
        throw new Error(`Unexpected IPC: ${command}`);
      },
    };
  }, { selection: opts.selection ?? null, devocal: opts.devocal ?? {}, models: opts.models ?? [missing(STEM), missing(BYTE), missing(HT)], OFF,
    label: view.includes("settings") ? "settings" : "main" });
  await page.setViewportSize({ width: 760, height: 700 });
  await page.goto(view);
}

const SETTINGS = "/?view=settings";
const saved = (page: Page) => page.evaluate(() => JSON.parse(localStorage.getItem("tune-love.devocal-model") ?? "null"));
const setDevocal = (page: Page, patch: Record<string, unknown>) => page.evaluate(p => { (window as any).devocal = { ...(window as any).devocal, ...p }; }, patch);
const radio = (page: Page, name: string) => page.getByRole("radio", { name, exact: true });
const dialog = (page: Page) => page.getByRole("alertdialog", { name: "切换到高质量档" });

test("the model choices carry the specified labels and StemgenRT is the default", async ({ page }) => {
  await prepare(page, SETTINGS);
  await expect(radio(page, "StemgenRT（默认，低延迟）")).toBeChecked();
  await expect(radio(page, "bytesep（高质量）")).not.toBeChecked();
  await expect(radio(page, "HTDemucs（高质量，需要 GPU）")).not.toBeChecked();
  for (const name of ["自动", "CPU", "GPU"]) await expect(radio(page, name)).toBeVisible();
  await expect(radio(page, "自动")).toBeChecked();
});

test("choosing bytesep asks first, names the added latency for the device, and cancel keeps the choice", async ({ page }) => {
  await prepare(page, SETTINGS);
  await radio(page, "CPU").click();
  await radio(page, "bytesep（高质量）").click();
  await expect(dialog(page)).toBeVisible();
  await expect(dialog(page).getByText("附加延迟约 360 ms，歌词可能与播放不同步。", { exact: true })).toBeVisible();
  await expect(dialog(page).getByText("训练数据来源不明，仅限非商业使用。")).toHaveCount(0);
  await dialog(page).getByRole("button", { name: "取消", exact: true }).click();
  await expect(dialog(page)).toHaveCount(0);
  await expect(radio(page, "StemgenRT（默认，低延迟）")).toBeChecked();
  expect((await saved(page)).modelId).toBe(STEM);

  await radio(page, "GPU").click();
  await radio(page, "bytesep（高质量）").click();
  await expect(dialog(page).getByText("附加延迟约 230 ms，歌词可能与播放不同步。", { exact: true })).toBeVisible();
  await dialog(page).getByRole("button", { name: "切换", exact: true }).click();
  await expect(radio(page, "bytesep（高质量）")).toBeChecked();
  expect(await saved(page)).toEqual({ modelId: BYTE, device: "gpu" });
  // Re-choosing the device while a quality model is selected does not ask again.
  await radio(page, "CPU").click();
  await expect(dialog(page)).toHaveCount(0);
  expect((await saved(page)).device).toBe("cpu");
});

test("choosing HTDemucs adds the training-data licence line", async ({ page }) => {
  await prepare(page, SETTINGS);
  await radio(page, "HTDemucs（高质量，需要 GPU）").click();
  await expect(dialog(page).getByText("附加延迟约 230 ms，歌词可能与播放不同步。", { exact: true })).toBeVisible();
  await expect(dialog(page).getByText("训练数据来源不明，仅限非商业使用。", { exact: true })).toBeVisible();
  await dialog(page).getByRole("button", { name: "切换", exact: true }).click();
  expect((await saved(page)).modelId).toBe(HT);
});

test("going back to StemgenRT needs no confirmation", async ({ page }) => {
  await prepare(page, SETTINGS, { selection: { modelId: BYTE, device: "auto" } });
  await radio(page, "StemgenRT（默认，低延迟）").click();
  await expect(dialog(page)).toHaveCount(0);
  expect((await saved(page)).modelId).toBe(STEM);
});

test("an undownloaded selection says to download first, an installed one does not", async ({ page }) => {
  await prepare(page, SETTINGS, { selection: { modelId: BYTE, device: "auto" }, models: [installed(STEM), missing(BYTE), missing(HT)] });
  const row = page.getByRole("group", { name: "bytesep MobileNet（高质量）" });
  await expect(row.getByText("请先下载此模型再开启去人声", { exact: true })).toBeVisible();
  await expect(page.getByRole("group", { name: "StemgenRT（低延迟）" }).getByText(/请先下载/)).toHaveCount(0);
  await expect(page.getByRole("group", { name: "HTDemucs（高质量）" }).getByText(/请先下载/)).toHaveCount(0);
});

test("the quality-model download consent box says what the model is for", async ({ page }) => {
  await prepare(page, SETTINGS);
  await page.getByRole("group", { name: "bytesep MobileNet（高质量）" }).getByRole("button", { name: "来源与许可", exact: true }).click();
  const box = page.getByRole("alertdialog", { name: "下载去人声模型" });
  await expect(box.getByText("高质量去人声，有附加延迟", { exact: false })).toBeVisible();
  await expect(box.getByText("用于实时去人声")).toHaveCount(0);
});

test("the hint in the main window leads to the selected model's download row", async ({ page }) => {
  await prepare(page, "/", { selection: { modelId: BYTE, device: "auto" }, devocal: { phase: "unavailable", error: `model_not_found:${BYTE}` } });
  await page.getByRole("button", { name: "未找到去人声模型，请在设置中下载", exact: true }).click();
  await expect.poll(() => page.evaluate(() => (window as any).openSettingsCalls)).toEqual([{ section: "devocal-model" }]);
  // The settings window then lands on the selected model's row, and does not auto-enable it.
  await page.goto("/?view=settings&section=devocal-model");
  const row = page.getByRole("group", { name: "bytesep MobileNet（高质量）" });
  await expect(row.getByRole("button", { name: /^下载模型/ })).toBeFocused();
  await expect(row.getByText("下载完成后将自动开启去人声")).toHaveCount(0);
});

test("enabling devocal sends the saved model and device", async ({ page }) => {
  await prepare(page, "/", { selection: { modelId: BYTE, device: "gpu" } });
  await page.locator(".music-window").hover({ position: { x: 18, y: 45 } });
  await page.getByRole("button", { name: "开启去人声", exact: true }).click();
  await expect(page.getByRole("button", { name: "关闭去人声", exact: true })).toBeVisible();
  expect(await page.evaluate(() => (window as any).devocalRequests)).toEqual([{ action: "enable", modelId: BYTE, device: "gpu" }]);
});

test("a selection saved in another window is picked up", async ({ page }) => {
  await prepare(page, SETTINGS);
  await page.evaluate(() => {
    const value = JSON.stringify({ modelId: "bytesep-mobilenet-1s", device: "cpu" });
    localStorage.setItem("tune-love.devocal-model", value);
    window.dispatchEvent(new StorageEvent("storage", { key: "tune-love.devocal-model", newValue: value }));
  });
  await expect(radio(page, "bytesep（高质量）")).toBeChecked();
  await expect(radio(page, "CPU")).toBeChecked();
});

const lineCases: [string, Record<string, unknown>, string][] = [
  ["gpu", { phase: "devocal", held: true, modelId: BYTE, device: "gpu" }, "正在使用：GPU"],
  ["cpu", { phase: "devocal", held: true, modelId: BYTE, device: "cpu" }, "正在使用：CPU"],
  ["auto fell back", { phase: "devocal", held: true, modelId: BYTE, device: "cpu", deviceNote: "gpu_unavailable" }, "GPU 不可用，已改用 CPU"],
  ["self-check failed", { phase: "devocal", held: true, modelId: BYTE, device: "cpu", deviceNote: "gpu_check_failed" }, "GPU 不可用，已改用 CPU"],
  ["gpu required", { phase: "passthrough", held: true, error: "gpu_required: no usable GPU" }, "所选模型需要 GPU，当前不可用"],
  ["overload to cpu", { phase: "devocal", held: true, modelId: BYTE, device: "cpu", deviceNote: "gpu_overloaded" }, "GPU 负载过高，已改用 CPU"],
  ["overload to StemgenRT", { phase: "devocal", held: true, modelId: STEM, device: "cpu", deviceNote: "gpu_overloaded" }, "GPU 负载过高，已改用 StemgenRT"],
  ["explicit GPU failed to load", { phase: "passthrough", held: true, error: "model_load_failed: dml" }, "去人声模型加载失败，已保持原声"],
];
for (const [name, devocal, text] of lineCases)
  test(`status line: ${name}`, async ({ page }) => {
    // HTDemucs is selected for the StemgenRT overload switch; otherwise bytesep.
    const selected = text.endsWith("StemgenRT") ? HT : BYTE;
    await prepare(page, SETTINGS, { selection: { modelId: selected, device: "auto" }, devocal });
    await expect(page.locator(".devocal-device-status")).toHaveText(text);
  });

test("status line: nothing while the engine is not running; HTDemucs on CPU reports the GPU requirement", async ({ page }) => {
  await prepare(page, SETTINGS, { selection: { modelId: HT, device: "cpu" } });
  await expect(radio(page, "HTDemucs（高质量，需要 GPU）")).toBeChecked();
  await expect(page.locator(".devocal-device-status")).toHaveText("");
  await setDevocal(page, { phase: "passthrough", held: true, error: "gpu_required: no usable GPU" });
  await expect(page.locator(".devocal-device-status")).toHaveText("所选模型需要 GPU，当前不可用");
});
