import { test, expect, type Page } from "@playwright/test";

async function prepare(page: Page, reduced = false) {
  await page.emulateMedia({ reducedMotion: reduced ? "reduce" : "no-preference" });
  await page.route("**/src/useNowPlaying.ts", async route => {
    const response = await route.fetch();
    const imports = (await response.text()).split("export function useNowPlaying()")[0];
    await route.fulfill({ contentType: "application/javascript", body: imports + `
      export function useNowPlaying() {
        const [snapshot, setSnapshot] = useState({ status: 'ready', track: {
          sourceId: 'player.exe', source: 'Test Player', title: 'Song', artist: 'Artist', album: 'Album',
          artworkDataUrl: null, sourceIconDataUrl: null, positionSeconds: 90, durationSeconds: 180,
          playbackStatus: 'playing', playbackRate: 1
        } });
        useEffect(() => {
          const receive = event => setSnapshot(current => ({ ...current, ...event.detail,
            track: event.detail.track === null ? null : { ...current.track, ...event.detail.track } }));
          window.addEventListener('test:progress', receive);
          return () => window.removeEventListener('test:progress', receive);
        }, []);
        return { ...snapshot, capturedAtMs: Date.now(), track: snapshot.track && {
          ...snapshot.track, updatedAtMs: Date.now()
        } };
      }` });
  });
  await page.goto("/");
  await expect(page.getByRole("progressbar", { name: "播放进度" })).toHaveAttribute("aria-valuenow", "50");
}

async function update(page: Page, detail: Record<string, unknown>) {
  await page.evaluate(detail => window.dispatchEvent(new CustomEvent("test:progress", { detail })), detail);
}

async function pixels(page: Page) {
  const png = await page.locator(".playback-progress-track").screenshot();
  return page.evaluate(async data => {
    const image = new Image(); image.src = `data:image/png;base64,${data}`;
    await image.decode();
    const canvas = document.createElement("canvas");
    canvas.width = image.naturalWidth; canvas.height = image.naturalHeight;
    const context = canvas.getContext("2d")!; context.drawImage(image, 0, 0);
    const rgba = context.getImageData(0, 0, canvas.width, canvas.height).data;
    const rows = Array.from({ length: canvas.height }, (_, y) =>
      Array.from({ length: canvas.width }, (_, x) => {
        const i = (y * canvas.width + x) * 4;
        return rgba[i] + rgba[i + 1] + rgba[i + 2];
      }));
    return { width: canvas.width, height: canvas.height, rows };
  }, png.toString("base64"));
}

// Catches a missing/reversed edge animation, or a translucent inner mask that
// turns the border into a misleading sweep across the actual progress fill.
test("Star Border moves opposite outer edges while the real progress interior stays unchanged", async ({ page }) => {
  await prepare(page);
  await expect(page.locator(".playback-progress-track")).toHaveCSS("height", "6px");
  const top = page.locator(".playback-progress-track > .border-gradient-top");
  const bottom = page.locator(".playback-progress-track > .border-gradient-bottom");
  await expect(top).toBeAttached();
  await expect(bottom).toBeAttached();
  await page.locator(".music-window").evaluate(el => (el as HTMLElement).style.setProperty("--accent", "#ffffff"));
  await page.waitForTimeout(500);
  const before = await top.evaluate(el => getComputedStyle(el).transform);
  await expect.poll(() => top.evaluate(el => getComputedStyle(el).transform)).not.toBe(before);
  const frames: Awaited<ReturnType<typeof pixels>>[] = [];
  for (const time of [2400, 3000, 3600]) {
    await page.locator(".playback-progress-track").evaluate((el, time) => {
      for (const animation of el.getAnimations({ subtree: true })) {
        animation.pause(); animation.currentTime = time;
      }
    }, time);
    frames.push(await pixels(page));
  }
  expect(frames.every(frame => frame.height === 6)).toBe(true);
  const peaks = (row: number) => frames.map(frame => {
    const values = frame.rows[row].slice(5, -5);
    return values.indexOf(Math.max(...values));
  });
  const topPeaks = peaks(0), bottomPeaks = peaks(5);
  expect(topPeaks[1] - topPeaks[0]).toBeGreaterThan(25);
  expect(topPeaks[2] - topPeaks[1]).toBeGreaterThan(25);
  expect(bottomPeaks[0] - bottomPeaks[1]).toBeGreaterThan(25);
  expect(bottomPeaks[1] - bottomPeaks[2]).toBeGreaterThan(25);
  expect(new Set(frames.map(frame => JSON.stringify(frame.rows.slice(2, 4)))).size).toBe(1);
  expect(frames[0].rows[2][40]).toBeGreaterThan(frames[0].rows[2][220] + 100);
  await expect(page.getByRole("progressbar")).toHaveCount(1);
  await expect(page.getByRole("progressbar")).toHaveAttribute("aria-valuetext", "1:30 / 3:00");
  await page.screenshot({ path: "artifacts/progress-star-border-playing.png" });
});

test("pause stops the border and resume restores it without changing the actual progress", async ({ page }) => {
  await prepare(page);
  const lights = page.locator(".playback-progress-track > [class^=border-gradient-]");
  await expect(lights).toHaveCount(2);
  for (const playbackStatus of ["paused", "stopped", "unknown"]) {
    await update(page, { track: { playbackStatus } });
    await expect(lights).toHaveCount(0);
    await page.waitForTimeout(300);
    const first = await pixels(page);
    await page.waitForTimeout(200);
    expect(await pixels(page)).toEqual(first);
    await expect(page.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "50");
  }
  await update(page, { track: { playbackStatus: "playing" } });
  await expect(lights).toHaveCount(2);
});

test("hidden and reduced-motion windows remove both border animations", async ({ page }) => {
  await prepare(page, true);
  const lights = page.locator(".playback-progress-track > [class^=border-gradient-]");
  await expect(lights).toHaveCount(0);
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await expect(lights).toHaveCount(2);
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expect(lights).toHaveCount(0);
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: false });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expect(lights).toHaveCount(2);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await expect(lights).toHaveCount(0);
});

test("seeking changes only the fill while Star Border stays around the full track", async ({ page }) => {
  await prepare(page);
  const track = page.locator(".playback-progress-track");
  const glow = page.locator(".playback-progress-track > .border-gradient-top");
  await expect(glow).toBeAttached();
  const width = (await track.boundingBox())!.width;
  await update(page, { track: { positionSeconds: 1.8 } });
  await expect(page.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "1");
  await expect.poll(() => page.getByRole("progressbar").evaluate(el => {
    const bar = el.getBoundingClientRect();
    const fill = el.querySelector(".rt-ProgressIndicator")!.getBoundingClientRect();
    return Math.abs((fill.right - bar.left) / bar.width * 100 - 1);
  })).toBeLessThan(0.1);
  expect((await track.boundingBox())!.width).toBe(width);
  await page.evaluate(() => { (window as any).previousBorder = document.querySelector(".border-gradient-top"); });
  await update(page, { track: { title: "Next Song", positionSeconds: 18 } });
  await expect(page.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "10");
  expect(await glow.evaluate(el => el === (window as any).previousBorder)).toBe(false);
  expect((await track.boundingBox())!.width).toBe(width);
});

test("zero, completed, missing and unavailable progress have no animated border", async ({ page }) => {
  await prepare(page);
  for (const track of [{ positionSeconds: 0 }, { positionSeconds: 180 }, { durationSeconds: 0 }]) {
    await update(page, { track });
    await expect(page.locator(".playback-progress-track > [class^=border-gradient-]")).toHaveCount(0);
  }
  await update(page, { status: "unavailable", track: null });
  await expect(page.locator(".playback-progress-track > [class^=border-gradient-]")).toHaveCount(0);
  await expect(page.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "0");
  await expect(page.getByRole("progressbar")).toHaveAttribute("aria-valuetext", "播放器未提供进度");
});

test("Star Border follows interpolated accent colors without restarting its animation", async ({ page }) => {
  await prepare(page);
  const glow = page.locator(".playback-progress-track > .border-gradient-top");
  await expect(glow).toBeAttached();
  await page.locator(".music-window").evaluate(el => (el as HTMLElement).style.setProperty("--accent", "#ff5555"));
  await page.waitForTimeout(500);
  const result = await glow.evaluate(el => new Promise<{ colors: string[]; sameNode: boolean; elapsed: number }>(resolve => {
    const animation = el.getAnimations()[0];
    const initial = Number(animation.currentTime);
    const colors: string[] = [], started = performance.now();
    (document.querySelector(".music-window") as HTMLElement).style.setProperty("--accent", "#5555ff");
    const sample = () => {
      colors.push(getComputedStyle(el).backgroundImage);
      if (performance.now() - started < 550) requestAnimationFrame(sample);
      else resolve({ colors, sameNode: el === document.querySelector(".border-gradient-top"), elapsed: Number(animation.currentTime) - initial });
    };
    requestAnimationFrame(sample);
  }));
  expect(new Set(result.colors).size).toBeGreaterThan(3);
  expect(result.sameNode).toBe(true);
  expect(result.elapsed).toBeGreaterThan(400);
});
