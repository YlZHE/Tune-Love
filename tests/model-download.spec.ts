import { test, expect, type Page } from "@playwright/test";

const OFF = { phase: "off", held: false, latencyMs: null, loadRatio: null, fallbackReason: null,
  sessionOverridden: false, inputSilent: false, waitingForPlayer: false, error: null };
const ID = "stemgenrt-hop128";
const MISSING = { id: ID, phase: "missing", receivedBytes: 0, totalBytes: 37_529_132, source: null, path: null, error: null, sourceError: null };

// Mocks the native boundary for the settings window. `window.models` is the status list the
// fake backend reports; `model_command` records the request and moves it the way the real one would.
async function prepare(page: Page, view = "/?view=settings", opts: { models?: Record<string, unknown>[]; devocal?: Record<string, unknown> } = {}) {
  await page.addInitScript(({ models, devocal, OFF }) => {
    const state = window as any;
    state.isTauri = true;
    state.models = models;
    state.modelRequests = [];
    state.modelFailure = null;
    state.modelDelayMs = 0;
    state.devocal = { ...OFF, ...devocal };
    state.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "settings" } },
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
            if (action === "import") return { ...m, phase: "installed", receivedBytes: m.totalBytes, source: "import", error: null };
            if (action === "delete") return { ...m, phase: "missing", receivedBytes: 0, source: null, path: null, error: null };
            return m;
          });
          return structuredClone(state.models);
        }
        if (command === "get_devocal_status") return structuredClone(state.devocal);
        if (command === "get_now_playing") return { status: "idle", track: null, capturedAtMs: Date.now() };
        if (command === "get_audio_level" || command === "get_key_detection") return null;
        if (command === "plugin:window|is_always_on_top") return true;
        if (command === "autotune_command") return { ok: true, state: { phase: "disconnected", connectionId: null,
          target: null, capabilities: [], instanceCount: 0, delivery: null, error: null, audioVerified: false } };
        throw new Error(`Unexpected IPC: ${command}`);
      },
    };
  }, { models: opts.models ?? [MISSING], devocal: opts.devocal ?? {}, OFF });
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
  await expect(row.getByText("加速服务返回了网页，已换下一个来源", { exact: true })).toBeVisible();
  await expect(row.getByText(/已下载 1\.0 \/ 35\.8 MB/)).toBeVisible();
  // Other phases do not show it.
  await setModels(page, [{ ...MISSING, sourceError: "source_html" }]);
  await expect(row.getByRole("button", { name: "下载模型（约 36 MB）" })).toBeVisible();
  await expect(row.getByText("加速服务返回了网页，已换下一个来源")).toHaveCount(0);
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
  await expect(row.getByText("下载超时，请重试。", { exact: true })).toBeVisible();
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
