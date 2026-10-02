import { test, expect, type Page } from "@playwright/test";
import type { NowPlaying } from "../src/nowPlaying";

test.use({ video: { mode: "on", size: { width: 468, height: 242 } } });

// Replace only the Windows media boundary. The actual app and Motion renderer run normally.
async function prepare(page: Page) {
  await page.route("**/src/useNowPlaying.ts", async route => {
    const response = await route.fetch();
    const imports = (await response.text()).split("export function useNowPlaying()")[0];
    await route.fulfill({ contentType: "application/javascript", body: imports + `
      export function useNowPlaying() {
        const [snapshot, setSnapshot] = useState({ status: 'idle', track: null, capturedAtMs: Date.now() });
        useEffect(() => {
          const receive = event => setSnapshot({ status: 'ready', track: event.detail, capturedAtMs: Date.now() });
          window.addEventListener('test:media', receive);
          return () => window.removeEventListener('test:media', receive);
        }, []);
        return snapshot;
      }` });
  });
  await page.goto("/");
  await expect(page.getByText("等待播放", { exact: true })).toBeVisible();
  const covers = await page.evaluate(() => ["#598eae", "#af7352", "#7867a7"].map((color, index) => {
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 150;
    const context = canvas.getContext("2d")!;
    context.fillStyle = color; context.fillRect(0, 0, 150, 150);
    context.fillStyle = "#ffffff"; context.font = "64px sans-serif";
    context.fillText(String.fromCharCode(65 + index), 50, 99);
    return canvas.toDataURL("image/png");
  }));
  return covers.map((cover, i): NowPlaying => ({
    title: ["晨光", "夜航", "回声"][i], artist: "动画测试 · 示例歌手", album: "示例专辑",
    source: "测试播放器", artworkDataUrl: cover, positionSeconds: 20, durationSeconds: 180,
    sourceId: "test.exe", sourceIconDataUrl: null,
    playbackStatus: "paused", playbackRate: 1, updatedAtMs: Date.now(),
  }));
}

async function publish(page: Page, track: NowPlaying) {
  await page.evaluate(track => window.dispatchEvent(new CustomEvent("test:media", {
    detail: { ...track, updatedAtMs: Date.now() },
  })), track);
}

async function sampleChange(page: Page, track: NowPlaying) {
  return page.evaluate(async track => {
    window.dispatchEvent(new CustomEvent("test:media", { detail: { ...track, updatedAtMs: Date.now() } }));
    const samples: { coverX: number; coverWidth: number; titleY: number }[] = [];
    const start = performance.now();
    await new Promise<void>(resolve => {
      const frame = () => {
        const title = [...document.querySelectorAll("h1")].find(el => el.textContent === track.title);
        const cover = [...document.querySelectorAll(".album-cover")].find(el =>
          el.querySelector("img")?.alt === `${track.title}的封面`);
        if (title && cover) {
          const box = cover.getBoundingClientRect();
          samples.push({ coverX: box.x, coverWidth: box.width, titleY: title.getBoundingClientRect().y });
        }
        if (performance.now() - start < 800) requestAnimationFrame(frame);
        else resolve();
      };
      requestAnimationFrame(frame);
    });
    return samples;
  }, track);
}

function span(values: number[]) { return Math.max(...values) - Math.min(...values); }

test("song changes visibly move both artwork and title; timeline updates do not replay the transition", async ({ page }) => {
  const [first, second] = await prepare(page);
  await sampleChange(page, first);
  const change = await sampleChange(page, second);
  expect(change.length).toBeGreaterThan(5);
  expect(span(change.map(s => s.coverX))).toBeGreaterThan(8);
  expect(span(change.map(s => s.titleY))).toBeGreaterThan(5);
  await expect(page.getByRole("heading", { name: "夜航", exact: true })).toBeVisible();
  await expect(page.getByRole("img", { name: "晨光的封面", includeHidden: true })).toHaveCount(0);
  const steady = await sampleChange(page, { ...second, positionSeconds: 42, playbackStatus: "playing" });
  expect(span(steady.map(s => s.coverX))).toBeLessThan(1);
  expect(span(steady.map(s => s.titleY))).toBeLessThan(1);
  await expect(page.getByRole("heading", { level: 1 })).toHaveCount(1);
  await page.close();
  await page.video()!.saveAs("artifacts/track-transition.webm");
});

test("rapid changes and late artwork converge on the latest song without replaying its title", async ({ page }) => {
  const [first, second, latest] = await prepare(page);
  await sampleChange(page, first);
  await publish(page, second);
  await expect(page.getByRole("heading", { name: "夜航", exact: true })).toBeVisible();
  await publish(page, { ...latest, artworkDataUrl: null });
  await expect(page.getByRole("heading", { name: "回声", exact: true })).toBeVisible();
  await expect(page.getByRole("img", { name: "晨光的封面", includeHidden: true })).toHaveCount(0);
  await expect(page.getByRole("img", { name: "夜航的封面", includeHidden: true })).toHaveCount(0);
  const arrival = await sampleChange(page, latest);
  expect(span(arrival.map(s => s.titleY))).toBeLessThan(1);
  await expect(page.getByRole("img", { name: "回声的封面" })).toBeVisible();
  await expect(page.getByRole("heading", { level: 1, includeHidden: true })).toHaveCount(1);
  const overflow = await page.locator(".music-window").evaluate(el => el.scrollWidth > el.clientWidth || el.scrollHeight > el.clientHeight);
  expect(overflow).toBe(false);
});

test("reduced motion changes songs without sliding or zooming", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  const [first, second] = await prepare(page);
  await sampleChange(page, first);
  const change = await sampleChange(page, second);
  expect(change.length).toBeGreaterThan(5);
  expect(span(change.map(s => s.coverX))).toBeLessThan(1);
  expect(span(change.map(s => s.coverWidth))).toBeLessThan(1);
  expect(span(change.map(s => s.titleY))).toBeLessThan(1);
  await expect(page.getByRole("heading", { name: "夜航", exact: true })).toBeVisible();
});

test("old text clears before new text reveals, with the artist following the title", async ({ page }) => {
  const [first, second] = await prepare(page);
  await sampleChange(page, first);
  const frames = await page.evaluate(async track => {
    const next = { ...track, artist: "新的示例歌手", updatedAtMs: Date.now() };
    window.dispatchEvent(new CustomEvent("test:media", { detail: next }));
    const frames: { oldTitle: boolean; newTitle: boolean; newArtist: boolean }[] = [];
    const visible = (element: Element | undefined) => {
      if (!element) return false;
      const box = element.getBoundingClientRect();
      let top = box.top, bottom = box.bottom, opacity = 1;
      for (let node: Element | null = element; node; node = node.parentElement) {
        const style = getComputedStyle(node);
        opacity *= Number(style.opacity);
        if (["hidden", "clip"].includes(style.overflowY)) {
          const clip = node.getBoundingClientRect();
          top = Math.max(top, clip.top); bottom = Math.min(bottom, clip.bottom);
        }
      }
      return opacity > 0.15 && bottom - top > box.height * 0.15;
    };
    const start = performance.now();
    await new Promise<void>(resolve => {
      const sample = () => {
        const titles = [...document.querySelectorAll("h1")];
        const artists = [...document.querySelectorAll(".artist")];
        frames.push({
          oldTitle: visible(titles.find(el => el.textContent === "晨光")),
          newTitle: visible(titles.find(el => el.textContent === next.title)),
          newArtist: visible(artists.find(el => el.textContent === next.artist)),
        });
        if (performance.now() - start < 800) requestAnimationFrame(sample);
        else resolve();
      };
      requestAnimationFrame(sample);
    });
    return frames;
  }, second);
  expect(frames.length).toBeGreaterThan(5);
  expect(frames.some(frame => frame.oldTitle && frame.newTitle)).toBe(false);
  const titleArrival = frames.findIndex(frame => frame.newTitle);
  const artistArrival = frames.findIndex(frame => frame.newArtist);
  expect(titleArrival).toBeGreaterThanOrEqual(0);
  expect(artistArrival).toBeGreaterThan(titleArrival);
  await expect(page.getByRole("heading", { name: "夜航", exact: true })).toBeVisible();
  await expect(page.getByText("新的示例歌手", { exact: true })).toBeVisible();
});
