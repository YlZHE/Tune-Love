import { test, expect, type Page } from "@playwright/test";

const labelSelector = ".playback-state > span:last-child";

async function prepare(page: Page, reduced = false) {
  await page.emulateMedia({ reducedMotion: reduced ? "reduce" : "no-preference" });
  await page.route("**/src/useNowPlaying.ts", async route => {
    const response = await route.fetch();
    const imports = (await response.text()).split("export function useNowPlaying()")[0];
    await route.fulfill({ contentType: "application/javascript", body: imports + `
      export function useNowPlaying() {
        const [playbackStatus, setPlaybackStatus] = useState('playing');
        useEffect(() => {
          const receive = event => setPlaybackStatus(event.detail);
          window.addEventListener('test:playback', receive);
          return () => window.removeEventListener('test:playback', receive);
        }, []);
        return { status: 'ready', capturedAtMs: Date.now(), track: {
          sourceId: 'player.exe', source: 'Test Player', title: 'Song', artist: 'Artist', album: 'Album',
          artworkDataUrl: null, sourceIconDataUrl: null, positionSeconds: 30, durationSeconds: 180,
          playbackStatus, playbackRate: 1, updatedAtMs: Date.now()
        } };
      }` });
  });
  await page.goto("/");
  await expect(page.locator(labelSelector)).toHaveText("正在播放");
}

test("playing label sweeps real text pixels without moving the text or resizing Strands", async ({ page }) => {
  await prepare(page);
  const label = page.locator(labelSelector);
  expect(await label.evaluate(el => getComputedStyle(el).backgroundImage)).not.toBe("none");
  const bounds = (await label.boundingBox())!;
  const frames: string[] = [];
  const positions: string[] = [];
  for (let index = 0; index < 8; index++) {
    frames.push((await label.screenshot()).toString("base64"));
    positions.push(await label.evaluate(el => getComputedStyle(el).backgroundPosition));
    expect(await label.boundingBox()).toEqual(bounds);
    await expect(label).toHaveText("正在播放");
    await page.waitForTimeout(180);
  }
  expect(new Set(frames).size).toBeGreaterThan(3);
  expect(new Set(positions).size).toBeGreaterThan(3);
  const strands = (await page.locator(".audio-strands").boundingBox())!;
  expect([strands.width, strands.height, bounds.x - strands.x - strands.width]).toEqual([46, 22, 1]);
  await page.screenshot({ path: "artifacts/playback-shine-playing.png" });
});

test("pause and stopped labels are static readable text and resuming restores the sweep", async ({ page }) => {
  await prepare(page);
  const label = page.locator(labelSelector);
  for (const [status, text] of [["paused", "已暂停"], ["stopped", "已停止"]]) {
    await page.evaluate(status => window.dispatchEvent(new CustomEvent("test:playback", { detail: status })), status);
    await expect(label).toHaveText(text);
    await expect(label).toHaveCSS("background-image", "none");
    expect(await label.evaluate(el => getComputedStyle(el).webkitTextFillColor)).not.toBe("rgba(0, 0, 0, 0)");
    const first = await label.screenshot();
    await page.waitForTimeout(250);
    expect(await label.screenshot()).toEqual(first);
  }
  await page.evaluate(() => window.dispatchEvent(new CustomEvent("test:playback", { detail: "playing" })));
  await expect(label).toHaveText("正在播放");
  const position = await label.evaluate(el => getComputedStyle(el).backgroundPosition);
  await expect.poll(() => label.evaluate(el => getComputedStyle(el).backgroundPosition)).not.toBe(position);
});

test("reduced motion keeps the playing label static and reacts to preference changes", async ({ page }) => {
  await prepare(page, true);
  const label = page.locator(labelSelector);
  await expect(label).toHaveCSS("background-image", "none");
  const first = await label.screenshot();
  await page.waitForTimeout(250);
  expect(await label.screenshot()).toEqual(first);
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await expect(label).not.toHaveCSS("background-image", "none");
  await page.emulateMedia({ reducedMotion: "reduce" });
  await expect(label).toHaveCSS("background-image", "none");
});

test("hidden windows stop the text effect and visibility restores it", async ({ page }) => {
  await prepare(page);
  const label = page.locator(labelSelector);
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expect(label).toHaveCSS("background-image", "none");
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, value: false });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await expect(label).not.toHaveCSS("background-image", "none");
  const position = await label.evaluate(el => getComputedStyle(el).backgroundPosition);
  await expect.poll(() => label.evaluate(el => getComputedStyle(el).backgroundPosition)).not.toBe(position);
});

test("the shine palette follows intermediate accent colors without remounting", async ({ page }) => {
  await prepare(page);
  const label = page.locator(labelSelector);
  await page.locator(".music-window").evaluate(el => (el as HTMLElement).style.setProperty("--accent", "#ff5555"));
  await page.waitForTimeout(500);
  const frames = await label.evaluate(el => new Promise<{ colors: string[]; sameNode: boolean }>(resolve => {
    const colors: string[] = [];
    const started = performance.now();
    (document.querySelector(".music-window") as HTMLElement).style.setProperty("--accent", "#5555ff");
    const read = () => {
      colors.push(getComputedStyle(el).backgroundImage);
      if (performance.now() - started < 550) requestAnimationFrame(read);
      else resolve({ colors, sameNode: el === document.querySelector(".playback-state > span:last-child") });
    };
    requestAnimationFrame(read);
  }));
  expect(new Set(frames.colors).size).toBeGreaterThan(3);
  expect(frames.sameNode).toBe(true);
});
