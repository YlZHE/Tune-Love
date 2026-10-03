import { test, expect, type Page } from "@playwright/test";

const OFF = { phase: "off", held: false, latencyMs: null, loadRatio: null, fallbackReason: null,
  sessionOverridden: false, inputSilent: false, waitingForPlayer: false, error: null };
const ID = "stemgenrt-hop128";
const MISSING = { id: ID, phase: "missing", receivedBytes: 0, totalBytes: 37_529_132, source: null, path: null, error: null, sourceError: null };

// Mocks the native boundary for the settings window. `window.models` is the status list the
// fake backend reports; `model_command` records the request and moves it the way the real one would.
async function prepare(page: Page, view = "/?view=settings", opts: { models?: Record<string, unknown>[]; devocal?: Record<string, unknown>; consent?: boolean } = {}) {
  await page.addInitScript(({ models, devocal, OFF, consent }) => {
    const state = window as any;
    // Download consent is pre-granted unless a test is about the consent box itself.
    if (consent) localStorage.setItem("helper-model-consent-v1", JSON.stringify({ keys: ["stemgenrt-hop128:77164d6a581fafb2a31f53fd8ffde44c07cf618472952a4cdba14e68dda3b8b9"] }));
    state.isTauri = true;
    state.models = models;
    state.modelRequests = [];
    state.modelFailure = null;
    state.modelDelayMs = 0;
    // false: the import file picker is closed without a choice, so nothing changes.
    state.importPicks = true;
    state.devocal = { ...OFF, ...devocal };
    state.openSettingsCalls = [];
    // Event bridge, shaped like @tauri-apps/api 2.x: listen() passes the id returned by
    // transformCallback as `handler`; unlisten() calls unregisterListener, then plugin:event|unlisten.
    state.callbacks = new Map();
    state.listeners = new Map();
    let nextId = 1;
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
    state.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "settings" } },
      transformCallback: (callback: (value: unknown) => void) => { const id = nextId++; state.callbacks.set(id, callback); return id; },
      invoke: async (command: string, args: any) => {
        if (command === "get_model_status") return structuredClone(state.models);
        if (command === "model_command") {
          state.modelRequests.push(args.request);
          await new Promise(resolve => setTimeout(resolve, state.modelDelayMs));
          if (state.modelFailure) throw state.modelFailure;
          const { id, action } = args.request;
          state.models = state.models.map((m: any) => {
            if (m.id !== id) return m;
            if (action === "download") return { ...m, phase: "downloading", receivedBytes: 12_897_485, source: "mirror:ghfast.top", error: null };
            if (action === "cancel") return { ...m, phase: "missing", error: null };
            if (action === "import") return state.importPicks ? { ...m, phase: "installed", receivedBytes: m.totalBytes, source: "import", error: null } : m;
            if (action === "delete") return { ...m, phase: "missing", receivedBytes: 0, source: null, path: null, error: null };
            return m;
          });
          return structuredClone(state.models);
        }
        if (command === "get_devocal_status") return structuredClone(state.devocal);
        if (command === "open_settings") { state.openSettingsCalls.push(args); return null; }
        if (command === "plugin:event|listen") { const eventId = nextId++; state.listeners.set(eventId, { event: args.event, handler: args.handler }); return eventId; }
        if (command === "plugin:event|unlisten") { state.listeners.delete(args.eventId); return null; }
        if (command === "get_now_playing") return { status: "idle", track: null, capturedAtMs: Date.now() };
        if (command === "get_audio_level" || command === "get_key_detection") return null;
        if (command === "plugin:window|is_always_on_top") return true;
        if (command === "autotune_command") return { ok: true, state: { phase: "disconnected", connectionId: null,
          target: null, capabilities: [], instanceCount: 0, delivery: null, error: null, audioVerified: false } };
        throw new Error(`Unexpected IPC: ${command}`);
      },
    };
  }, { models: opts.models ?? [MISSING], devocal: opts.devocal ?? {}, OFF, consent: opts.consent ?? true });
  await page.setViewportSize({ width: 760, height: 600 });
  await page.goto(view);
  return page.getByRole("group", { name: "StemgenRT（低延迟）" });
}

const requests = (page: Page) => page.evaluate(() => (window as any).modelRequests as Record<string, unknown>[]);
const setModels = (page: Page, models: Record<string, unknown>[]) => page.evaluate(m => { (window as any).models = m; }, models);
const request = (action: string) => ({ id: ID, action, autoEnable: false, mirrorPrefix: null });

test("settings: a missing model offers download with size and licence note", async ({ page }) => {
  const row = await prepare(page);
  await expect(row.getByRole("button", { name: "下载模型（约 36 MB）", exact: true })).toBeVisible();
  await expect(row.getByText(/许可待作者确认/)).toBeVisible();
  await expect(row.getByRole("button", { name: "来源与许可", exact: true })).toBeVisible();
  await expect(row.getByRole("button", { name: "从本地文件导入…", exact: true })).toBeVisible();
});

test("settings: before the first status arrives the row says it is reading", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { models: [] });
  await expect(row).toHaveCount(1);
  await expect(row.getByText("正在读取模型状态…")).toBeVisible();
  await expect(row.getByRole("button")).toHaveCount(0);
});

test("settings: download progress shows bytes, source and a cancel button", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { models: [
    { ...MISSING, phase: "downloading", receivedBytes: 12_897_485, source: "mirror:ghfast.top" }] });
  await expect(row.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "34");
  await expect(row.getByText("已下载 12.3 / 35.8 MB · 来源：镜像 ghfast.top", { exact: true })).toBeVisible();
  await row.getByRole("button", { name: "取消", exact: true }).click();
  expect(await requests(page)).toEqual([request("cancel")]);
  await expect(row.getByRole("button", { name: "继续下载", exact: true })).toBeVisible();
});

test("settings: clicking download sends the request and follows the new status", async ({ page }) => {
  const row = await prepare(page);
  await row.getByRole("button", { name: "下载模型（约 36 MB）", exact: true }).click();
  expect(await requests(page)).toEqual([request("download")]);
  await expect(row.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "34");
});

test("settings: a downloading row shows the last per-source failure as a hint", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { models: [
    { ...MISSING, phase: "downloading", receivedBytes: 1_000_000, source: "mirror:ghproxy.net", sourceError: "source_html" }] });
  await expect(row.getByText("某个来源失败（加速服务返回了网页），已自动换用下一个", { exact: true })).toBeVisible();
  await expect(row.getByText(/已下载 1\.0 \/ 35\.8 MB/)).toBeVisible();
  // Other phases do not show it.
  await setModels(page, [{ ...MISSING, sourceError: "source_html" }]);
  await expect(row.getByRole("button", { name: "下载模型（约 36 MB）" })).toBeVisible();
  await expect(row.getByText("加速服务返回了网页")).toHaveCount(0);
});

test("settings: a partial download offers resume and delete-partial", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { models: [{ ...MISSING, receivedBytes: 12_897_485 }] });
  await expect(row.getByRole("button", { name: "继续下载", exact: true })).toBeVisible();
  await expect(row.getByText("已下载 12.3 / 35.8 MB", { exact: true })).toBeVisible();
  await expect(row.getByRole("button", { name: "从本地文件导入…", exact: true })).toBeVisible();
  await row.getByRole("button", { name: "删除已下载部分", exact: true }).click();
  expect(await requests(page)).toEqual([request("delete")]);
  await expect(row.getByRole("button", { name: "下载模型（约 36 MB）", exact: true })).toBeVisible();
});

test("settings: a missing model can be imported from a local file", async ({ page }) => {
  const row = await prepare(page);
  await row.getByRole("button", { name: "从本地文件导入…", exact: true }).click();
  expect(await requests(page)).toEqual([request("import")]);
  await expect(row.getByText("模型已就绪", { exact: true })).toBeVisible();
});

test("settings: verifying and ready states", async ({ page }) => {
  const sha = "77164d6a581fafb2a31f53fd8ffde44c07cf618472952a4cdba14e68dda3b8b9";
  const path = "C:\\Users\\me\\AppData\\Local\\tune-love\\models\\stemgenrt-hop128\\model.onnx";
  const row = await prepare(page, "/?view=settings", { models: [{ ...MISSING, phase: "verifying", receivedBytes: 37_529_132 }] });
  await expect(row.getByText("正在校验…", { exact: true })).toBeVisible();
  await setModels(page, [{ ...MISSING, phase: "installed", receivedBytes: 37_529_132, source: "origin", path }]);
  await expect(row.getByText("模型已就绪", { exact: true })).toBeVisible();
  await row.getByText("详细信息", { exact: true }).click();
  await expect(row.getByText(path)).toBeVisible();
  await expect(row.getByText("37,529,132 字节")).toBeVisible();
  await expect(row.getByText(sha)).toBeVisible();
  await row.getByRole("button", { name: "删除模型", exact: true }).click();
  expect(await requests(page)).toEqual([request("delete")]);
});

test("settings: an env-provided model has no delete button", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { models: [
    { ...MISSING, phase: "installed", receivedBytes: 37_529_132, source: "env", path: "D:\\dev\\model.onnx" }] });
  await expect(row.getByText("模型已就绪（开发者环境变量）", { exact: true })).toBeVisible();
  await expect(row.getByRole("button", { name: "删除模型" })).toHaveCount(0);
});

test("settings: a failure shows the Chinese error with retry and import", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { models: [
    { ...MISSING, phase: "failed", error: "all_sources_failed", sourceError: "timeout" }] });
  const alert = row.getByRole("alert");
  await expect(alert).toHaveText("所有下载来源都失败了。可设置系统代理、填写加速前缀，或从本地文件导入。");
  await expect(row.getByText("最后一个来源失败：下载超时，请重试。", { exact: true })).toBeVisible();
  await page.evaluate(() => { (window as any).modelFailure = "already_running"; });
  await row.getByRole("button", { name: "重试", exact: true }).click();
  await expect(alert).toHaveText("正在下载中。");
  expect(await requests(page)).toEqual([request("download")]);
  await page.evaluate(() => { (window as any).modelFailure = null; });
  await row.getByRole("button", { name: "从本地文件导入…", exact: true }).click();
  await expect(row.getByRole("alert")).toHaveCount(0);
  expect(await requests(page)).toEqual([request("download"), request("import")]);
});

test("settings: a rejection disappears when the row changes phase", async ({ page }) => {
  const row = await prepare(page);
  await page.evaluate(() => { (window as any).modelFailure = "already_running"; });
  await row.getByRole("button", { name: "下载模型（约 36 MB）", exact: true }).click();
  await expect(row.getByRole("alert")).toHaveText("正在下载中。");
  await setModels(page, [{ ...MISSING, phase: "downloading", receivedBytes: 1_000_000, source: "origin" }]);
  await expect(row.getByRole("progressbar")).toBeVisible();
  await expect(row.getByRole("alert")).toHaveCount(0);
});

test("settings: a stale rejection does not hide a newer failure", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { models: [{ ...MISSING, phase: "failed", error: "timeout" }] });
  await page.evaluate(() => { (window as any).modelFailure = "already_running"; });
  await row.getByRole("button", { name: "重试", exact: true }).click();
  await expect(row.getByRole("alert")).toHaveText("正在下载中。");
  await setModels(page, [{ ...MISSING, phase: "downloading", receivedBytes: 1_000_000 }]);
  await expect(row.getByRole("progressbar")).toBeVisible();
  await setModels(page, [{ ...MISSING, phase: "failed", error: "sha256_mismatch" }]);
  await expect(row.getByRole("alert")).toHaveText("下载的文件校验不通过，已删除。请重试。");
});

test("settings: a failed status without an error code still reads sensibly", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { models: [{ ...MISSING, phase: "failed", error: null }] });
  await expect(row.getByRole("alert")).toHaveText("下载失败，请重试。");
});

test("settings: the row's buttons are disabled while a command is pending", async ({ page }) => {
  const row = await prepare(page);
  await page.evaluate(() => { (window as any).modelDelayMs = 600; });
  const download = row.getByRole("button", { name: "下载模型（约 36 MB）", exact: true });
  await download.click();
  await expect(download).toBeDisabled();
  await expect(row.getByRole("button", { name: "从本地文件导入…", exact: true })).toBeDisabled();
  await download.click({ force: true });
  await expect(row.getByRole("progressbar")).toBeVisible();
  expect(await requests(page)).toEqual([request("download")]);
  await expect(row.getByRole("button", { name: "取消", exact: true })).toBeEnabled();
});

const DOWNLOAD = "下载模型（约 36 MB）";
const consentStored = (page: Page) => page.evaluate(() => localStorage.getItem("helper-model-consent-v1"));

test("consent dialog lists source, commit, size, sha256, licence, training data, mirrors and location", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { consent: false });
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  const dialog = page.getByRole("alertdialog");
  await expect(dialog.getByText("下载去人声模型", { exact: true })).toBeVisible();
  await expect(dialog).toContainText("https://github.com/sweetspotsoundsystem/stemgen-rt");
  await expect(dialog).toContainText("61df8f4aa1555ef110308d01ea92b54ace770979");
  await expect(dialog).toContainText("35.8 MB（37,529,132 字节）");
  await expect(dialog).toContainText("77164d6a581fafb2a31f53fd8ffde44c07cf618472952a4cdba14e68dda3b8b9");
  await expect(dialog).toContainText("待确认");
  await expect(dialog).toContainText("MUSDB18-HQ（仅教育用途）");
  await expect(dialog).toContainText("MoisesDB（CC BY-NC-SA 4.0）");
  await expect(dialog).toContainText("非商业");
  await expect(dialog).toContainText("ghfast.top、ghproxy.net、ghproxy.vip");
  await expect(dialog).toContainText("models\\stemgenrt-hop128\\");
  await expect(dialog).not.toContainText("权重采用 MIT");
  await expect(dialog.getByRole("button", { name: "同意并下载", exact: true })).toBeVisible();
  await expect(dialog.getByRole("button", { name: "取消", exact: true })).toBeVisible();
  expect(await requests(page)).toEqual([]);
});

test("accepting consent remembers it and sends the request", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { consent: false });
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await page.getByRole("alertdialog").getByRole("button", { name: "同意并下载", exact: true }).click();
  await expect(page.getByRole("alertdialog")).toHaveCount(0);
  await expect(row.getByRole("progressbar")).toBeVisible();
  expect(await requests(page)).toEqual([request("download")]);
  expect(await consentStored(page)).toContain(ID);
});

test("cancelling consent sends no model_command", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { consent: false });
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await page.getByRole("alertdialog").getByRole("button", { name: "取消", exact: true }).click();
  await expect(page.getByRole("alertdialog")).toHaveCount(0);
  expect(await requests(page)).toEqual([]);
  expect(await consentStored(page)).toBeNull();
  await expect(row.getByRole("button", { name: DOWNLOAD, exact: true })).toBeEnabled();
});

test("consent is remembered per model sha256", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { consent: false });
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await page.getByRole("alertdialog").getByRole("button", { name: "同意并下载", exact: true }).click();
  await expect(row.getByRole("progressbar")).toBeVisible();
  // Back to missing: the second click goes straight through.
  await setModels(page, [MISSING]);
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect(page.getByRole("alertdialog")).toHaveCount(0);
  await expect.poll(async () => (await requests(page)).length).toBe(2);
  // A consent recorded for another hash does not count.
  await setModels(page, [MISSING]);
  await page.evaluate(() => localStorage.setItem("helper-model-consent-v1", JSON.stringify({ keys: ["stemgenrt-hop128:deadbeef"] })));
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect(page.getByRole("alertdialog")).toBeVisible();
  expect(await requests(page)).toHaveLength(2);
});

test("the source-and-licence button always opens the consent box", async ({ page }) => {
  const row = await prepare(page);
  await row.getByRole("button", { name: "来源与许可", exact: true }).click();
  await expect(page.getByRole("alertdialog")).toContainText("待确认");
  expect(await requests(page)).toEqual([]);
});

test("mirror prefix: invalid input is flagged and not sent; a valid one is sent and listed", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { consent: false });
  await page.getByText("高级", { exact: true }).click();
  const input = page.getByLabel("下载加速前缀（可选）");
  await expect(input).toHaveAttribute("placeholder", "https://example.com/");
  await input.fill("http://x/");
  await input.blur();
  await expect(page.getByRole("alert").filter({ hasText: "须以 https:// 开头、以 / 结尾；未保存，不使用自定义前缀" })).toBeVisible();
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await page.getByRole("alertdialog").getByRole("button", { name: "同意并下载", exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect((await requests(page))[0].mirrorPrefix).toBeNull();

  await setModels(page, [MISSING]);
  await input.fill("https://my.proxy/");
  await input.blur();
  await expect(page.getByRole("alert").filter({ hasText: "须以 https:// 开头" })).toHaveCount(0);
  await page.evaluate(() => localStorage.removeItem("helper-model-consent-v1"));
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect(page.getByRole("alertdialog")).toContainText("ghproxy.vip、你填写的 my.proxy");
  await page.getByRole("alertdialog").getByRole("button", { name: "同意并下载", exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(2);
  expect((await requests(page))[1].mirrorPrefix).toBe("https://my.proxy/");
});

test("mirror prefix: an invalid edit after a valid one clears the saved prefix", async ({ page }) => {
  const row = await prepare(page);
  await page.getByText("高级", { exact: true }).click();
  const input = page.getByLabel("下载加速前缀（可选）");
  await input.fill("https://a.proxy/");
  await input.blur();
  await input.fill("http://b/");
  await input.blur();
  await expect(page.getByRole("alert").filter({ hasText: "未保存，不使用自定义前缀" })).toBeVisible();
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect((await requests(page))[0].mirrorPrefix).toBeNull();
});

test("retry goes through the consent box too", async ({ page }) => {
  const row = await prepare(page, "/?view=settings", { consent: false, models: [{ ...MISSING, phase: "failed", error: "timeout" }] });
  await row.getByRole("button", { name: "重试", exact: true }).click();
  const dialog = page.getByRole("alertdialog");
  await expect(dialog).toBeVisible();
  expect(await requests(page)).toEqual([]);
  await dialog.getByRole("button", { name: "同意并下载", exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect(await requests(page)).toEqual([request("download")]);
});

// Delivers a Rust `emit_to` to every live listener for that event, the way the IPC bridge does.
const emitEvent = (page: Page, event: string, payload: unknown) => page.evaluate(({ event, payload }) => {
  const state = window as any;
  for (const [, listener] of state.listeners) {
    if (listener.event === event) state.callbacks.get(listener.handler)({ event, id: 1, payload });
  }
}, { event, payload });
const AUTO_ENABLE_NOTE = "下载完成后将自动开启去人声";

test("main: the missing-model warning opens settings at the model row", async ({ page }) => {
  await prepare(page, "/", { devocal: { phase: "unavailable", error: "model_not_found" } });
  const warning = page.locator(".devocal-warning[aria-live=polite]");
  const button = warning.getByRole("button", { name: "未找到去人声模型，请在设置中下载", exact: true });
  // Visible without hovering the window, and still the only announced text.
  await expect(button).toBeVisible();
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "0");
  await expect(page.locator("[aria-live]", { hasText: "未找到去人声模型" })).toHaveCount(1);
  await button.click();
  await expect.poll(() => page.evaluate(() => (window as any).openSettingsCalls)).toEqual([{ section: "devocal-model" }]);
  // An engine that cannot start offers no settings shortcut.
  await page.evaluate(() => { (window as any).devocal = { ...(window as any).devocal, error: "engine_unavailable: spawn failed" }; });
  await expect(page.locator(".devocal-warning")).toHaveText("去人声引擎无法启动");
  await expect(page.locator(".devocal-warning").getByRole("button")).toHaveCount(0);
});

test("main: the settings button still opens settings without a section", async ({ page }) => {
  await prepare(page, "/");
  await page.getByRole("button", { name: "设置", exact: true }).click();
  await expect.poll(() => page.evaluate(() => (window as any).openSettingsCalls)).toEqual([{}]);
});

test("settings opened from the missing-model hint downloads with autoEnable", async ({ page }) => {
  const row = await prepare(page, "/?view=settings&section=devocal-model", { consent: false });
  const section = page.getByRole("region", { name: "去人声" });
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  expect((await section.boundingBox())!.y).toBeLessThan(600);
  await expect(row.getByRole("button", { name: DOWNLOAD, exact: true })).toBeFocused();
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  // Opening the consent box does not use up the request; agreeing does. (The modal box hides
  // the row from the accessibility tree, so the note is found by text alone.)
  await expect(page.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  await page.getByRole("alertdialog").getByRole("button", { name: "同意并下载", exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect(await requests(page)).toEqual([{ ...request("download"), autoEnable: true }]);
  // Still pending while the download runs; the user's cancel drops it.
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  await row.getByRole("button", { name: "取消", exact: true }).click();
  await row.getByRole("button", { name: "继续下载", exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(3);
  expect(await requests(page)).toEqual([{ ...request("download"), autoEnable: true }, request("cancel"), request("download")]);
  await expect(row.getByText(AUTO_ENABLE_NOTE)).toHaveCount(0);
});

test("settings opened from the hint: cancelling consent keeps the request; an import carries it", async ({ page }) => {
  const row = await prepare(page, "/?view=settings&section=devocal-model", { consent: false });
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await page.getByRole("alertdialog").getByRole("button", { name: "取消", exact: true }).click();
  await expect(page.getByRole("alertdialog")).toHaveCount(0);
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  await row.getByRole("button", { name: "从本地文件导入…", exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect(await requests(page)).toEqual([{ ...request("import"), autoEnable: true }]);
});

test("settings opened normally downloads without autoEnable", async ({ page }) => {
  const row = await prepare(page);
  await expect(row.getByRole("button", { name: DOWNLOAD, exact: true })).toBeVisible();
  await expect(row.getByText(AUTO_ENABLE_NOTE)).toHaveCount(0);
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect(await requests(page)).toEqual([request("download")]);
});

test("a settings-section event in an open settings window focuses the model row", async ({ page }) => {
  const row = await prepare(page);
  await expect(row.getByRole("button", { name: DOWNLOAD, exact: true })).toBeVisible();
  await expect(row.getByText(AUTO_ENABLE_NOTE)).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => [...(window as any).listeners.values()]
    .filter((l: any) => l.event === "settings-section").length)).toBe(1);
  // Unknown sections are ignored.
  await emitEvent(page, "settings-section", "colors");
  await expect(row.getByText(AUTO_ENABLE_NOTE)).toHaveCount(0);
  await emitEvent(page, "settings-section", "devocal-model");
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  await expect(row.getByRole("button", { name: DOWNLOAD, exact: true })).toBeFocused();
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect(await requests(page)).toEqual([{ ...request("download"), autoEnable: true }]);
});

test("a plain open of the already open settings window drops the pending auto-enable", async ({ page }) => {
  const row = await prepare(page, "/?view=settings&section=devocal-model");
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  await expect(row.getByRole("button", { name: DOWNLOAD, exact: true })).toBeFocused();
  await expect.poll(() => page.evaluate(() => [...(window as any).listeners.values()]
    .filter((l: any) => l.event === "settings-section").length)).toBe(1);
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  // The settings button on an open window sends a null section: no scroll, no focus, no auto-enable.
  await emitEvent(page, "settings-section", null);
  await expect(row.getByText(AUTO_ENABLE_NOTE)).toHaveCount(0);
  await expect(row.getByRole("button", { name: DOWNLOAD, exact: true })).not.toBeFocused();
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect(await requests(page)).toEqual([request("download")]);
});

test("a rejected download keeps the pending auto-enable for the retry", async ({ page }) => {
  const row = await prepare(page, "/?view=settings&section=devocal-model");
  await page.evaluate(() => { (window as any).modelFailure = "network_unreachable"; });
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect(row.getByRole("alert")).toBeVisible();
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  await page.evaluate(() => { (window as any).modelFailure = null; });
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect(row.getByRole("progressbar")).toBeVisible();
  expect(await requests(page)).toEqual([{ ...request("download"), autoEnable: true }, { ...request("download"), autoEnable: true }]);
  // Kept until the model is installed.
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
});

test("a download from the hint that fails retries with autoEnable until the model is installed", async ({ page }) => {
  const row = await prepare(page, "/?view=settings&section=devocal-model");
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect(row.getByRole("progressbar")).toBeVisible();
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  // Every source failed.
  await setModels(page, [{ ...MISSING, phase: "failed", receivedBytes: 1_000_000, error: "all_sources_failed", sourceError: "timeout" }]);
  await expect(row.getByRole("button", { name: "重试", exact: true })).toBeVisible();
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  await row.getByRole("button", { name: "重试", exact: true }).click();
  await expect(row.getByRole("progressbar")).toBeVisible();
  expect(await requests(page)).toEqual([{ ...request("download"), autoEnable: true }, { ...request("download"), autoEnable: true }]);
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  // Installed: the request is used up, so a later download (after a delete) does not carry it.
  await setModels(page, [{ ...MISSING, phase: "installed", receivedBytes: 37_529_132, source: "origin", path: "C:\\m\\model.onnx" }]);
  await expect(row.getByText("模型已就绪", { exact: true })).toBeVisible();
  await expect(row.getByText(AUTO_ENABLE_NOTE)).toHaveCount(0);
  await row.getByRole("button", { name: "删除模型", exact: true }).click();
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(4);
  expect((await requests(page))[3]).toEqual(request("download"));
  await expect(row.getByText(AUTO_ENABLE_NOTE)).toHaveCount(0);
});

test("an import that leaves the model missing keeps the pending auto-enable", async ({ page }) => {
  const row = await prepare(page, "/?view=settings&section=devocal-model");
  await page.evaluate(() => { (window as any).importPicks = false; });
  await row.getByRole("button", { name: "从本地文件导入…", exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  await expect(row.getByRole("button", { name: "从本地文件导入…", exact: true })).toBeEnabled();
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect(row.getByRole("progressbar")).toBeVisible();
  expect(await requests(page)).toEqual([{ ...request("import"), autoEnable: true }, { ...request("download"), autoEnable: true }]);
});

test("a reload of a settings window opened from the hint does not re-arm auto-enable", async ({ page }) => {
  const row = await prepare(page, "/?view=settings&section=devocal-model");
  await expect(row.getByText(AUTO_ENABLE_NOTE, { exact: true })).toBeVisible();
  // The section is read once and then dropped from the address.
  await expect.poll(() => new URL(page.url()).search).toBe("?view=settings");
  await page.reload();
  await expect(row.getByRole("button", { name: DOWNLOAD, exact: true })).toBeVisible();
  await expect(row.getByText(AUTO_ENABLE_NOTE)).toHaveCount(0);
  await row.getByRole("button", { name: DOWNLOAD, exact: true }).click();
  await expect.poll(async () => (await requests(page)).length).toBe(1);
  expect(await requests(page)).toEqual([request("download")]);
});
