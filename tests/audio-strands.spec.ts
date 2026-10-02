import { test, expect, type Page } from "@playwright/test";

async function prepare(page: Page, options: { reduced?: boolean; noWebGL?: boolean; browserOnly?: boolean } = {}) {
  if (options.reduced) await page.emulateMedia({ reducedMotion: "reduce" });
  await page.addInitScript(({ noWebGL }) => {
    const state = window as any;
    state.audioFrames = [];
    state.audioDrawCount = 0;
    state.audioCalls = [];
    state.audioPending = 0;
    state.audioMaxPending = 0;
    state.audioDelay = 0;
    state.audioError = false;
    state.audioFixture = { sourceId: "player.exe", trackKey: '["player.exe","Song","Artist","Album"]', status: "capturing", rms: 0.3, peak: 0.9, level: 0.8, bands: [0.8, 0.4, 0.2] };
    state.mediaFixture = { status: "ready", capturedAtMs: Date.now(), track: {
      sourceId: "player.exe", source: "Test Player", title: "Song", artist: "Artist", album: "Album",
      artworkDataUrl: null, sourceIconDataUrl: null, positionSeconds: 30, durationSeconds: 180,
      playbackStatus: "playing", playbackRate: 1, updatedAtMs: Date.now(),
    } };
    if (noWebGL) {
      const original = HTMLCanvasElement.prototype.getContext;
      HTMLCanvasElement.prototype.getContext = function(kind: string, ...args: any[]) {
        if (kind.startsWith("webgl") || kind === "experimental-webgl") return null;
        return original.call(this, kind as any, ...args);
      } as any;
    }
    const originalDraw = WebGL2RenderingContext.prototype.drawArrays;
    WebGL2RenderingContext.prototype.drawArrays = function(...args) {
      originalDraw.apply(this, args);
      if (!(this.canvas as HTMLCanvasElement).closest(".audio-strands") || this.getParameter(this.FRAMEBUFFER_BINDING)) return;
      const pixels = new Uint8Array(this.drawingBufferWidth * this.drawingBufferHeight * 4);
      this.readPixels(0, 0, this.drawingBufferWidth, this.drawingBufferHeight, this.RGBA, this.UNSIGNED_BYTE, pixels);
      let hash = 2166136261, visible = 0, alpha = 0, red = 0, green = 0, blue = 0;
      const centerWeight = [0, 0, 0], centerY = [0, 0, 0];
      let minX = this.drawingBufferWidth, maxX = -1, minY = this.drawingBufferHeight, maxY = -1;
      for (let i = 0; i < pixels.length; i += 4) {
        for (let j = 0; j < 4; j++) hash = Math.imul(hash ^ pixels[i + j], 16777619);
        if (pixels[i + 3] > 20) {
          visible++;
          const x = (i / 4) % this.drawingBufferWidth;
          const y = Math.floor((i / 4) / this.drawingBufferWidth);
          minX = Math.min(minX, x); maxX = Math.max(maxX, x);
          minY = Math.min(minY, y); maxY = Math.max(maxY, y);
        }
        alpha += pixels[i + 3]; red += pixels[i]; green += pixels[i + 1]; blue += pixels[i + 2];
        const x = (i / 4) % this.drawingBufferWidth;
        if (Math.abs(x - (this.drawingBufferWidth - 1) / 2) <= 3) {
          const y = Math.floor((i / 4) / this.drawingBufferWidth);
          for (let channel = 0; channel < 3; channel++) {
            const weight = pixels[i + channel] ** 2;
            centerWeight[channel] += weight;
            centerY[channel] += weight * y;
          }
        }
      }
      state.audioDrawCount++;
      state.audioFrames.push({ hash: hash >>> 0, visible, visibleWidth: Math.max(0, maxX - minX + 1), visibleRight: maxX + 1, visibleHeight: Math.max(0, maxY - minY + 1), alpha, red, green, blue, centerY: centerY.map((sum, i) => centerWeight[i] ? sum / centerWeight[i] : null), at: performance.now() });
      if (state.audioFrames.length > 180) state.audioFrames.shift();
    };
  }, { noWebGL: !!options.noWebGL });
  await page.route("**/src/useNowPlaying.ts", async route => {
    const response = await route.fetch();
    const imports = (await response.text()).split("export function useNowPlaying()")[0];
    await route.fulfill({ contentType: "application/javascript", body: imports + `
      export function useNowPlaying() {
        const [snapshot, setSnapshot] = useState(window.mediaFixture);
        useEffect(() => {
          const receive = event => setSnapshot({ ...event.detail, capturedAtMs: Date.now() });
          window.addEventListener('test:media', receive);
          return () => window.removeEventListener('test:media', receive);
        }, []);
        return { ...snapshot, capturedAtMs: Date.now() };
      }` });
  });
  if (!options.browserOnly) await page.route("**/src/audioLevel.ts", async route => {
    const response = await route.fetch();
    const source = (await response.text()).split("export async function readAudioLevel")[0];
    await route.fulfill({ contentType: "application/javascript", body: source + `
      export async function readAudioLevel() {
        window.audioCalls.push(performance.now());
        window.audioPending++;
        window.audioMaxPending = Math.max(window.audioMaxPending, window.audioPending);
        try {
          if (window.audioDelay) await new Promise(resolve => setTimeout(resolve, window.audioDelay));
          if (window.audioError) throw new Error('capture unavailable');
          return { updatedAtMs: Date.now(), ...window.audioFixture };
        } finally { window.audioPending--; }
      }` });
  });
  await page.goto("/");
  await expect(page.getByText("正在播放", { exact: true })).toBeVisible();
}

const drawCount = (page: Page) => page.evaluate(() => (window as any).audioDrawCount as number);
const lastFrame = (page: Page) => page.evaluate(() => (window as any).audioFrames.at(-1) as { hash: number; visible: number; visibleWidth: number; visibleRight: number; visibleHeight: number; alpha: number; red: number; green: number; blue: number });
const renderedBands = (page: Page) => page.locator(".audio-strands canvas").evaluate((canvas: HTMLCanvasElement) => {
  const gl = canvas.getContext("webgl2")!;
  const program = gl.getParameter(gl.CURRENT_PROGRAM);
  const location = gl.getUniformLocation(program, "uBands");
  return location ? Array.from(gl.getUniform(program, location) as Float32Array) : null;
});

test("three frequency bands reach separate strands and constant energy does not rotate", async ({ page }) => {
  await prepare(page);
  await expect.poll(() => renderedBands(page)).not.toBeNull();
  const frames: number[] = [];
  for (const bands of [[0.85, 0, 0], [0, 0.85, 0], [0, 0, 0.85]]) {
    await page.evaluate(bands => { (window as any).audioFixture.bands = bands; }, bands);
    await expect.poll(async () => (await renderedBands(page))?.map(v => Math.round(v * 100))).toEqual(bands.map(v => v * 100));
    await page.waitForTimeout(400);
    const first = (await lastFrame(page)).hash;
    await page.waitForTimeout(300);
    expect((await lastFrame(page)).hash).toBe(first);
    frames.push(first);
  }
  expect(new Set(frames).size).toBe(3);
});

test("missing or malformed band values never fall back to three copies of total volume", async ({ page }) => {
  await prepare(page);
  for (const bands of [undefined, [0.5], [0.3, -1, 0.1], [0.3, null, 0.1]]) {
    await page.evaluate(bands => { (window as any).audioFixture.bands = bands; }, bands);
    await expect.poll(() => renderedBands(page)).toEqual([0, 0, 0]);
  }
});

test("low stays below fixed mid and high stays above while the outer strands move", async ({ page }) => {
  await prepare(page);
  await page.evaluate(() => {
    const owner = document.querySelector(".music-window") as HTMLElement;
    ["#ff0000", "#00ff00", "#0000ff"].forEach((color, i) => owner.style.setProperty(`--strand-${i}`, color));
    (window as any).audioFixture.bands = [0.15, 0.15, 0.15];
  });
  await page.waitForTimeout(1_000);
  const movements: number[] = [];
  for (let band = 0; band < 3; band++) {
    await page.evaluate(() => { (window as any).audioFixture.bands = [0.15, 0.15, 0.15]; });
    await page.waitForTimeout(800);
    const before = await page.evaluate(() => (window as any).audioFrames.at(-1).centerY as number[]);
    await page.evaluate(index => { (window as any).audioFixture.bands[index] = 0.95; }, band);
    await page.waitForTimeout(200);
    const after = await page.evaluate(() => (window as any).audioFrames.at(-1).centerY as number[]);
    movements.push(Math.abs(after[band] - before[band]));
    // Framebuffer rows count upward: low red < middle green < high blue.
    expect(before[0]).toBeLessThan(before[1] - 0.5);
    expect(before[2]).toBeGreaterThan(before[1] + 0.5);
    expect(after[0]).toBeLessThan(after[1] - 0.5);
    expect(after[2]).toBeGreaterThan(after[1] + 0.5);
    expect(Math.abs(after[1] - 10.5)).toBeLessThan(0.15);
    if (band === 1) expect(movements[band], "middle line must stay centered").toBeLessThan(0.15);
    else expect(movements[band], `band ${band} needs visible vertical movement at 46 x 22`).toBeGreaterThan(2);
    for (let other = 0; other < 3; other++) {
      if (other !== band) expect(Math.abs(after[other] - before[other])).toBeLessThan(0.15);
    }
  }
  await test.info().attach("vertical-strand-movement-pixels", { body: JSON.stringify(movements), contentType: "application/json" });
});
async function expectStill(page: Page) {
  await page.waitForTimeout(2_200);
  const count = await drawCount(page);
  await page.waitForTimeout(220);
  expect(await drawCount(page)).toBe(count);
}

test("adapted Strands renders real pixels, follows changing bands, and sleeps after silence", async ({ page }) => {
  await prepare(page);
  await expect(page.locator(".audio-strands canvas")).toBeVisible();
  await expect.poll(() => drawCount(page)).toBeGreaterThan(8);
  const box = (await page.locator(".audio-strands").boundingBox())!;
  expect(box.width).toBe(46); expect(box.height).toBe(22);
  expect(await page.locator(".audio-strands canvas").evaluate((canvas: HTMLCanvasElement) => [canvas.width, canvas.height])).toEqual([46, 22]);
  const label = (await page.locator(".playback-state > span:last-child").boundingBox())!;
  const loud = await lastFrame(page);
  expect(loud.visible).toBeGreaterThan(10);
  // Measure the lit strands, including transparent canvas padding in the visible gap.
  expect(loud.visibleWidth).toBeGreaterThanOrEqual(36);
  expect(loud.visibleHeight).toBeLessThanOrEqual(18);
  const visibleGap = label.x - (box.x + loud.visibleRight);
  expect(visibleGap).toBeGreaterThanOrEqual(2);
  expect(visibleGap).toBeLessThanOrEqual(5);
  expect(loud.alpha).toBeGreaterThan(1_000);
  await page.evaluate(() => { (window as any).audioFixture.bands = [0.2, 0.9, 0.6]; });
  await page.waitForTimeout(160);
  expect((await lastFrame(page)).hash).not.toBe(loud.hash);
  await page.screenshot({ path: "artifacts/audio-strands-playing.png" });
  await page.evaluate(() => { (window as any).audioFixture.bands = [0, 0, 0]; });
  await expectStill(page);
  const quiet = await lastFrame(page);
  expect(quiet.visible).toBeGreaterThan(0);
  expect(quiet.alpha).toBeLessThan(loud.alpha);
  await test.info().attach("actual-strands-pixels", { body: JSON.stringify({ loud, quiet, visibleGap }), contentType: "application/json" });
  await page.screenshot({ path: "artifacts/audio-strands-silent.png" });
  const sleepCount = await drawCount(page);
  await page.evaluate(() => { (window as any).audioFixture.bands = [1, 0.8, 0.6]; });
  await expect.poll(() => drawCount(page)).toBeGreaterThan(sleepCount + 4);
  expect((await lastFrame(page)).alpha).toBeGreaterThan(quiet.alpha);
  const source = (await page.locator(".source").boundingBox())!;
  expect(source.x + source.width + 7).toBeLessThanOrEqual(box.x);
  expect(box.x + box.width).toBeLessThan(label.x);
  expect(await page.locator(".music-window").evaluate(el => [el.clientWidth, el.clientHeight, el.scrollWidth > el.clientWidth, el.scrollHeight > el.clientHeight])).toEqual([446, 220, false, false]);
});

test("stale, mismatched and failed capture stops motion without replacing metadata", async ({ page }) => {
  await prepare(page);
  await expect.poll(() => drawCount(page)).toBeGreaterThan(4);
  for (const change of [{ updatedAtMs: Date.now() - 1_000 }, { sourceId: "other.exe" }, { trackKey: "old-song" }, { status: "unavailable" }, { level: null }]) {
    await page.evaluate(change => { (window as any).audioFixture = { sourceId: "player.exe", trackKey: '["player.exe","Song","Artist","Album"]', status: "capturing", rms: 0.3, peak: 0.9, level: 0.8, bands: [0.8, 0.4, 0.2], ...change }; }, change);
    await expectStill(page);
    expect((await lastFrame(page)).visible).toBeGreaterThan(0);
  }
  await page.evaluate(() => { (window as any).audioError = true; });
  await expectStill(page);
  await expect(page.getByRole("heading", { name: "Song", exact: true })).toBeVisible();
});

test("pause preserves its icon; hiding clears audio and resumes only with a fresh frame", async ({ page }) => {
  await prepare(page);
  await expect.poll(() => drawCount(page)).toBeGreaterThan(4);
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  const hiddenCount = await drawCount(page);
  await page.waitForTimeout(200);
  expect(await drawCount(page)).toBe(hiddenCount);
  await page.evaluate(() => {
    (window as any).audioFixture.updatedAtMs = Date.now() - 1_000;
    Object.defineProperty(document, "hidden", { configurable: true, value: false });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expectStill(page);
  await page.evaluate(() => {
    const snapshot = (window as any).mediaFixture;
    window.dispatchEvent(new CustomEvent("test:media", { detail: { ...snapshot, track: { ...snapshot.track, playbackStatus: "paused" } } }));
  });
  await expect(page.getByText("已暂停", { exact: true })).toBeVisible();
  await expect(page.locator(".audio-strands")).toHaveCount(0);
  await expect(page.locator(".playback-state svg")).toBeVisible();
});

test("all three computed palette colors reach actual shader pixels throughout the CSS transition", async ({ page }) => {
  await prepare(page);
  await expect.poll(() => drawCount(page)).toBeGreaterThan(5);
  await page.evaluate(() => {
    (window as any).audioFixture.bands = [0, 0, 0];
    for (let i = 0; i < 3; i++) (document.querySelector(".music-window") as HTMLElement).style.setProperty(`--strand-${i}`, "#ff5555");
  });
  await expectStill(page);
  const red = await lastFrame(page);
  expect(red.red).toBeGreaterThan(red.blue);
  const initialCount = await drawCount(page);
  await page.evaluate(() => {
    (window as any).audioFrames = [];
    for (let i = 0; i < 3; i++) (document.querySelector(".music-window") as HTMLElement).style.setProperty(`--strand-${i}`, "#5555ff");
  });
  await page.waitForTimeout(650);
  const frames = await page.evaluate(() => (window as any).audioFrames as { hash: number; red: number; blue: number }[]);
  expect(await drawCount(page)).toBeGreaterThan(initialCount + 3);
  expect(new Set(frames.map(frame => frame.hash)).size).toBeGreaterThan(3);
  expect(frames.some(frame => frame.red > frame.blue)).toBe(true);
  expect(frames.at(-1)!.blue).toBeGreaterThan(frames.at(-1)!.red);
  await page.screenshot({ path: "artifacts/audio-strands-accent.png" });
});

test("each fixed strand uses its own palette color and energy in actual pixels", async ({ page }) => {
  await prepare(page);
  await page.evaluate(() => {
    const owner = document.querySelector(".music-window") as HTMLElement;
    ["#ff0000", "#00ff00", "#0000ff"].forEach((color, i) => owner.style.setProperty(`--strand-${i}`, color));
    (window as any).audioFixture.bands = [0, 0, 0];
  });
  await expectStill(page);
  const quiet = await lastFrame(page);
  for (const [i, channel] of ["red", "green", "blue"].entries()) {
    await page.evaluate(index => { (window as any).audioFixture.bands = [0, 0, 0].map((_, i) => i === index ? 0.9 : 0); }, i);
    await page.waitForTimeout(2_000);
    const frame = await lastFrame(page);
    for (const name of ["red", "green", "blue"] as const) {
      if (name === channel) expect(frame[name]).toBeGreaterThan(quiet[name] * 2);
      else expect(Math.abs(frame[name] - quiet[name])).toBeLessThan(40);
    }
    await page.locator(".audio-strands").screenshot({ path: `artifacts/strand-band-${i}.png` });
  }
});

test("near-black cover colors still produce a visible responsive strand", async ({ page }) => {
  await prepare(page);
  await page.evaluate(() => {
    const owner = document.querySelector(".music-window") as HTMLElement;
    ["#000000", "#010101", "#020202"].forEach((color, i) => owner.style.setProperty(`--strand-${i}`, color));
    (window as any).audioFixture.bands = [0.9, 0.6, 0.3];
  });
  await page.waitForTimeout(700);
  const loud = await lastFrame(page);
  expect(loud.visible).toBeGreaterThan(10);
  await page.evaluate(() => { (window as any).audioFixture.bands = [0, 0, 0]; });
  await expectStill(page);
  expect((await lastFrame(page)).alpha).toBeLessThan(loud.alpha);
  expect((await lastFrame(page)).visible).toBeGreaterThan(0);
});

test("reduced motion renders a visible static fallback with no audio animation", async ({ page }) => {
  await prepare(page, { reduced: true });
  await expect(page.locator(".audio-strands .strands-fallback")).toBeVisible();
  await expect(page.locator(".audio-strands canvas")).toHaveCount(0);
  const centers = await page.locator(".strands-fallback path").evaluateAll(paths => paths.map(path => {
    const box = (path as SVGGraphicsElement).getBBox();
    return box.y + box.height / 2;
  }));
  expect(centers[0]).toBeGreaterThan(centers[1]);
  expect(centers[2]).toBeLessThan(centers[1]);
  const first = await page.locator(".audio-strands").screenshot();
  await page.waitForTimeout(180);
  expect(await page.locator(".audio-strands").screenshot()).toEqual(first);
  expect(await drawCount(page)).toBe(0);
  await page.screenshot({ path: "artifacts/audio-strands-reduced.png" });
});

test("missing WebGL2 and context loss render the static fallback without page errors", async ({ page }) => {
  const errors: string[] = []; page.on("pageerror", error => errors.push(error.message));
  await prepare(page, { noWebGL: true });
  await expect(page.locator(".audio-strands .strands-fallback")).toBeVisible();
  await page.screenshot({ path: "artifacts/audio-strands-fallback.png" });
  expect(errors).toEqual([]);
});

test("a lost context releases animation and shows a visible static glyph", async ({ page }) => {
  const errors: string[] = []; page.on("pageerror", error => errors.push(error.message));
  await prepare(page);
  await expect.poll(() => drawCount(page)).toBeGreaterThan(4);
  await page.locator(".audio-strands canvas").evaluate((canvas: HTMLCanvasElement) => canvas.getContext("webgl2")!.getExtension("WEBGL_lose_context")!.loseContext());
  await expect(page.locator(".audio-strands .strands-fallback")).toBeVisible();
  const count = await drawCount(page);
  await page.waitForTimeout(180);
  expect(await drawCount(page)).toBe(count);
  expect(errors).toEqual([]);
});

test("audio requests never overlap and errors keep the normal polling limit", async ({ page }) => {
  await prepare(page);
  await page.evaluate(() => { (window as any).audioDelay = 85; });
  await page.waitForTimeout(450);
  expect(await page.evaluate(() => (window as any).audioMaxPending)).toBe(1);
  await page.evaluate(() => { (window as any).audioDelay = 0; (window as any).audioError = true; (window as any).audioCalls = []; });
  await page.waitForTimeout(450);
  const calls = await page.evaluate(() => (window as any).audioCalls as number[]);
  expect(calls.length).toBeGreaterThan(3);
  expect(calls.length).toBeLessThanOrEqual(30);
  expect(calls.slice(1).every((time, index) => time - calls[index] >= 15)).toBe(true);
});

test("browser preview has no fabricated audio activity", async ({ page }) => {
  await prepare(page, { browserOnly: true });
  await expect(page.locator(".audio-strands canvas")).toBeVisible();
  await expectStill(page);
  expect((await lastFrame(page)).visible).toBeGreaterThan(0);
});

test("a delayed native reply cannot keep an expired audio frame moving", async ({ page }) => {
  await prepare(page);
  await expect.poll(() => drawCount(page)).toBeGreaterThan(5);
  await page.evaluate(() => { (window as any).audioDelay = 5_000; });
  await expectStill(page);
  const quiet = await lastFrame(page);
  expect(quiet.visible).toBeGreaterThan(0);
  await page.evaluate(() => { (window as any).audioFixture.level = 0; });
});
