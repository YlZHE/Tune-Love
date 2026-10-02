import { test, expect, type BrowserContext, type Page } from "@playwright/test";

async function prepare(context: BrowserContext, enabled = false) {
  await context.addInitScript(({ enabled }) => {
    if (localStorage.getItem("helper-background-v1") === null)
      localStorage.setItem("helper-background-v1", JSON.stringify({ enabled }));
    const state = window as any;
    state.audioPending = 0; state.audioMaxPending = 0; state.audioCalls = 0;
    state.audioDelay = 8;
    state.backgroundFrames = [];
    state.backgroundWrites = 0;
    state.gpuCreated = 0; state.gpuDestroyed = 0;
    if ((window as any).GPUQueue) {
      const originalWrite = GPUQueue.prototype.writeBuffer;
      GPUQueue.prototype.writeBuffer = function(buffer, offset, data, ...rest) {
        const result = originalWrite.call(this, buffer, offset, data, ...rest);
        if (buffer.label === "view.sharedUniform") {
          const bytes = ArrayBuffer.isView(data)
            ? data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) : data;
          const values = new Float32Array(bytes as ArrayBuffer);
          state.backgroundWrites++;
          state.backgroundFrames.push({ at: performance.now(), viewport: Array.from(values.slice(0, 4)), shape: Array.from(values.slice(4, 8)),
            material: Array.from(values.slice(48, 52)), palette: Array.from(values.slice(60, 72)) });
          if (state.backgroundFrames.length > 120) state.backgroundFrames.shift();
        }
        if (buffer.label === "post.sharedUniform") {
          const bytes = ArrayBuffer.isView(data)
            ? data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) : data;
          state.backgroundFinishing = Array.from(new Float32Array(bytes as ArrayBuffer).slice(8, 12));
        }
        return result;
      };
      const originalDraw = GPURenderPassEncoder.prototype.draw;
      GPURenderPassEncoder.prototype.draw = function(vertices, instances, ...rest) {
        if (instances && instances > 1) state.shardInstances = instances;
        return originalDraw.call(this, vertices, instances, ...rest);
      };
      const originalRequest = GPUAdapter.prototype.requestDevice;
      GPUAdapter.prototype.requestDevice = async function(...args) {
        const device = await originalRequest.apply(this, args); state.gpuCreated++; return device;
      };
      const originalDestroy = GPUDevice.prototype.destroy;
      GPUDevice.prototype.destroy = function() { state.gpuDestroyed++; return originalDestroy.call(this); };
    }
    state.bands = [0.75, 0.45, 0.25];
    state.media = { status: "ready", capturedAtMs: Date.now(), track: {
      sourceId: "player.exe", source: "Test Player", title: "Song", artist: "Artist", album: "Album",
      artworkDataUrl: null, sourceIconDataUrl: null, positionSeconds: 30, durationSeconds: 180,
      playbackStatus: "playing", playbackRate: 1, updatedAtMs: Date.now(),
    } };
  }, { enabled });
  await context.route("**/src/useNowPlaying.ts", async route => {
    const source = (await (await route.fetch()).text()).split("export function useNowPlaying()")[0];
    await route.fulfill({ contentType: "application/javascript", body: source + `
      export function useNowPlaying() {
        const [snapshot, setSnapshot] = useState(window.media);
        useEffect(() => {
          const update = event => setSnapshot(event.detail);
          window.addEventListener('test:media', update);
          return () => window.removeEventListener('test:media', update);
        }, []);
        return { ...snapshot, capturedAtMs: Date.now() };
      }` });
  });
  await context.route("**/src/audioLevel.ts", async route => {
    const source = (await (await route.fetch()).text()).split("export async function readAudioLevel")[0];
    await route.fulfill({ contentType: "application/javascript", body: source + `
      export async function readAudioLevel() {
        if (requestInFlight) return null;
        requestInFlight = true;
        window.audioCalls++; window.audioPending++;
        window.audioMaxPending = Math.max(window.audioMaxPending, window.audioPending);
        try {
          await new Promise(resolve => setTimeout(resolve, window.audioDelay));
          return { sourceId: 'player.exe', trackKey: '["player.exe","Song","Artist","Album"]',
            status: 'capturing', rms: .3, peak: .8, level: .7,
            bands: window.bands, updatedAtMs: Date.now(), ...window.audioOverride };
        } finally { window.audioPending--; requestInFlight = false; }
      }` });
  });
}

async function settings(page: Page) {
  const popup = page.waitForEvent("popup");
  await page.getByRole("button", { name: "设置", exact: true }).click();
  const result = await popup;
  await result.setViewportSize({ width: 760, height: 540 });
  return result;
}

// Read the canvas itself: a locator screenshot would include the animated
// foreground progress/text and could falsely report background movement.
const canvasPixels = (page: Page) => page.locator(".audio-background canvas").evaluate((canvas: HTMLCanvasElement) => canvas.toDataURL());
const writes = (page: Page) => page.evaluate(() => (window as any).backgroundWrites as number);
const frame = (page: Page) => page.evaluate(() => (window as any).backgroundFrames.at(-1) as {
  at: number; viewport: number[]; shape: number[]; material: number[]; palette: number[];
});

test("background switch persists across windows and disabling removes its canvas", async ({ page, context }) => {
  await prepare(context); await page.goto("/");
  await expect(page.locator(".audio-background canvas")).toHaveCount(0);
  const panel = await settings(page);
  const toggle = panel.getByRole("switch", { name: "音频响应背景" });
  await expect(toggle).not.toBeChecked();
  await toggle.check();
  await expect(page.locator(".audio-background")).toBeAttached();
  await page.reload(); await panel.reload();
  await expect(toggle).toBeChecked();
  await expect(page.locator(".audio-background")).toBeAttached();
  await toggle.uncheck();
  await expect(page.locator(".audio-background")).toHaveCount(0);
  await expect(page.locator(".audio-strands canvas")).toBeVisible();
  await page.reload();
  await expect(page.locator(".audio-background")).toHaveCount(0);
});

test("Aero Shards renders real pixels and shares one serialized audio loop with Strands", async ({ page, context }) => {
  await prepare(context, true); await page.goto("/");
  const background = page.locator(".audio-background .aero-shards");
  await expect(background).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  await expect(page.locator(".audio-strands canvas")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Song", exact: true })).toBeVisible();
  const frames: string[] = [];
  for (let i = 0; i < 4; i++) {
    frames.push(await canvasPixels(page));
    await page.waitForTimeout(150);
  }
  expect(new Set(frames).size).toBeGreaterThan(1);
  expect(await page.evaluate(() => (window as any).audioMaxPending)).toBe(1);
  expect(await page.evaluate(() => (window as any).audioCalls)).toBeGreaterThan(5);
  const hit = await page.getByRole("button", { name: "设置", exact: true }).evaluate(el => {
    const bounds = el.getBoundingClientRect();
    return el.contains(document.elementFromPoint(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2));
  });
  expect(hit).toBe(true);
  await page.screenshot({ path: "artifacts/aero-shards-playing.png" });
});

test("silence freezes the background and fresh audio wakes it again", async ({ page, context }) => {
  await prepare(context, true); await page.goto("/");
  await expect(page.locator(".aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  await page.evaluate(() => { (window as any).bands = [0, 0, 0]; });
  await page.waitForTimeout(1500);
  const quiet = await canvasPixels(page);
  const before = await writes(page);
  await page.waitForTimeout(300);
  expect(await writes(page)).toBe(before);
  expect((await canvasPixels(page)) === quiet).toBe(true);
  await page.evaluate(() => { (window as any).bands = [0.9, 0.7, 0.5]; });
  await expect.poll(async () => (await canvasPixels(page)) === quiet).toBe(false);
});

test("each band reaches its own real GPU parameters and disabling destroys owned devices", async ({ page, context }) => {
  await prepare(context, true); await page.goto("/");
  await expect(page.locator(".aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  await page.evaluate(() => { (window as any).bands = [1, 0, 0]; });
  await expect.poll(async () => Number((await frame(page)).shape[1].toFixed(3))).toBe(1.05);
  expect((await frame(page)).viewport[2]).toBeCloseTo(1.15, 4);
  expect((await frame(page)).shape[0]).toBeCloseTo(1, 4);
  await page.evaluate(() => { (window as any).bands = [0, 1, 0]; });
  await expect.poll(async () => Number((await frame(page)).shape[2].toFixed(4))).toBe(0.3744);
  await page.evaluate(() => { (window as any).bands = [0, 0, 1]; });
  await expect.poll(async () => Number((await frame(page)).material[3].toFixed(3))).toBe(1.458);
  await expect.poll(() => page.evaluate(() => Number((window as any).backgroundFinishing?.[0].toFixed(3)))).toBe(1.04);
  expect(await page.evaluate(() => (window as any).shardInstances)).toBeGreaterThan(1000);
  const panel = await settings(page);
  await panel.getByRole("switch", { name: "音频响应背景" }).uncheck();
  await expect.poll(() => page.evaluate(() => (window as any).gpuDestroyed === (window as any).gpuCreated)).toBe(true);
  expect(await page.evaluate(() => (window as any).gpuCreated)).toBeGreaterThan(0);
  await expect(page.locator(".audio-strands canvas")).toBeVisible();
});

test("reduced motion and unavailable WebGPU preserve a usable static player", async ({ page, context }) => {
  await prepare(context, true);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("/");
  await expect(page.locator(".audio-background")).toBeAttached();
  await expect(page.locator(".audio-background canvas")).toHaveCount(0);
  await expect(page.getByRole("heading", { name: "Song", exact: true })).toBeVisible();
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await page.addInitScript(() => Object.defineProperty(navigator, "gpu", { configurable: true, value: undefined }));
  await page.reload();
  await expect(page.locator(".audio-background")).toHaveAttribute("data-state", "unavailable", { timeout: 10000 });
  await expect(page.getByRole("progressbar", { name: "播放进度" })).toBeVisible();
});

test("pause, hidden windows, stale samples and another source cannot keep the background moving", async ({ page, context }) => {
  await prepare(context, true); await page.goto("/");
  await expect(page.locator(".aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  for (const override of [{ updatedAtMs: 1 }, { sourceId: "other.exe" }, { trackKey: "old-song" }, { status: "unavailable" }]) {
    await page.evaluate(value => { (window as any).audioOverride = value; }, override);
    await page.waitForTimeout(550);
    const before = await writes(page);
    await page.waitForTimeout(150);
    expect(await writes(page)).toBe(before);
    await page.evaluate(() => { (window as any).audioOverride = {}; });
    await expect.poll(() => writes(page)).toBeGreaterThan(before);
  }
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  const hidden = await writes(page);
  await page.waitForTimeout(200);
  expect(await writes(page)).toBe(hidden);
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: false });
    document.dispatchEvent(new Event("visibilitychange"));
    const state = window as any;
    state.media.track.playbackStatus = "paused";
    window.dispatchEvent(new CustomEvent("test:media", { detail: { ...state.media } }));
  });
  await expect(page.getByText("已暂停", { exact: true })).toBeVisible();
  await page.waitForTimeout(550);
  const paused = await writes(page);
  await page.waitForTimeout(200);
  expect(await writes(page)).toBe(paused);
});

test("all three live palette colors interpolate on the GPU even when the background is asleep", async ({ page, context }) => {
  await prepare(context, true); await page.goto("/");
  await expect(page.locator(".aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  await page.evaluate(() => { (window as any).bands = [0, 0, 0]; });
  await page.waitForTimeout(550);
  const quietPosition = (await frame(page)).viewport[3];
  await page.locator(".music-window").evaluate(el => {
    (window as any).backgroundFrames = [];
    ["#ff0000", "#00ff00", "#0000ff"].forEach((color, index) => (el as HTMLElement).style.setProperty(`--strand-${index}`, color));
  });
  await expect.poll(async () => (await frame(page)).palette.map(v => Number(v.toFixed(2))))
    .toEqual([1, 0, 0, 1, 0.3, 0.3, 1, 1, 0, 1, 0, 1]);
  expect(await page.evaluate(() => new Set((window as any).backgroundFrames.map((f: any) => JSON.stringify(f.palette))).size)).toBeGreaterThan(3);
  expect((await frame(page)).viewport[3]).toBe(quietPosition);
  await page.waitForTimeout(650);
  const before = await writes(page);
  await page.waitForTimeout(150);
  expect(await writes(page)).toBe(before);
});

test("a failed preference write does not falsely enable the background", async ({ page, context }) => {
  await prepare(context); await page.goto("/");
  const panel = await settings(page);
  await panel.evaluate(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function(key, value) {
      if (key === "helper-background-v1") throw new Error("storage denied");
      original.call(this, key, value);
    };
  });
  const toggle = panel.getByRole("switch", { name: "音频响应背景" });
  await toggle.click();
  await expect(toggle).not.toBeChecked();
  await expect(panel.getByRole("alert")).toHaveText("未能保存背景设置，请重试");
  await expect(page.locator(".audio-background")).toHaveCount(0);
});

test("showing a window without a fresh matching source cannot replay its old bass pulse", async ({ page, context }) => {
  await prepare(context, true); await page.goto("/");
  await expect(page.locator(".aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  await expect.poll(async () => (await frame(page)).shape[1]).toBeGreaterThan(1.02);
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: true });
    document.dispatchEvent(new Event("visibilitychange"));
    (window as any).audioOverride = { sourceId: "other.exe" };
    (window as any).backgroundFrames = [];
  });
  await page.waitForTimeout(200);
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: false });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expect.poll(() => page.evaluate(() => (window as any).backgroundFrames.length)).toBeGreaterThan(0);
  const first = await page.evaluate(() => (window as any).backgroundFrames[0]);
  expect(first.viewport[2]).toBeCloseTo(1.15, 4);
  expect(first.shape[0]).toBeCloseTo(1, 4);
  expect(first.shape[1]).toBeCloseTo(1, 4);
});

test("sustained bass does not prevent quality recovery after temporary GPU pressure", async ({ page, context }) => {
  test.setTimeout(45000);
  await prepare(context, true);
  await page.addInitScript(() => {
    const state = window as any;
    state.gpuPressureMs = 0;
    state.shardInstances = 0;
    const originalDraw = GPURenderPassEncoder.prototype.draw;
    GPURenderPassEncoder.prototype.draw = function(vertices, instances, ...rest) {
      if (instances && instances > 1) {
        state.shardInstances = instances;
        const until = performance.now() + state.gpuPressureMs;
        while (performance.now() < until) { /* controlled temporary encoding load */ }
      }
      return originalDraw.call(this, vertices, instances, ...rest);
    };
  });
  await page.goto("/");
  await expect(page.locator(".aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  await expect.poll(async () => (await frame(page)).shape[1]).toBeGreaterThan(1.02);
  const instances = () => page.evaluate(() => (window as any).shardInstances as number);
  const fullQuality = await instances();
  expect(fullQuality).toBeGreaterThan(1);
  await page.evaluate(() => { (window as any).gpuPressureMs = 7; });
  await expect.poll(instances, { timeout: 8000, intervals: [200] }).toBeLessThan(fullQuality);
  await page.evaluate(() => { (window as any).gpuPressureMs = 0; });
  await expect.poll(instances, { timeout: 22000, intervals: [250] }).toBe(fullQuality);
  expect((await frame(page)).shape[1]).toBeGreaterThan(1.02);
});

test("bass changes depth without zooming or spreading and audio only gently changes transport speed", async ({ page, context }) => {
  await prepare(context, true); await page.goto("/");
  await expect(page.locator(".aero-shards")).toHaveAttribute("data-ready", "true", { timeout: 20000 });
  const velocities: number[] = [];
  for (const bands of [[1, 0, 0], [0, 1, 0]]) {
    await page.evaluate(value => { (window as any).bands = value; }, bands);
    await page.waitForTimeout(400);
    const first = await frame(page);
    await page.waitForTimeout(450);
    const last = await frame(page);
    // Read actual GPU travel, not a test-only speed property.
    velocities.push((last.viewport[3] - first.viewport[3]) / ((last.at - first.at) / 1000 * 0.34));
    expect(last.viewport[2]).toBeCloseTo(1.15, 4);
    expect(last.shape[0]).toBeCloseTo(1, 4);
    expect(last.shape[1]).toBeCloseTo(bands[0] ? 1.05 : 1, 4);
  }
  expect(velocities[0]).toBeCloseTo(0.405, 2);
  expect(velocities[1]).toBeCloseTo(0.495, 2);
  expect(velocities[1]).toBeGreaterThan(velocities[0]);
});
