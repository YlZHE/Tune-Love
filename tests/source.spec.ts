import { test, expect, type Page } from "@playwright/test";

async function prepare(page: Page, icon: "valid" | "missing" | "invalid", name = "Folia") {
  await page.addInitScript(({ icon, name }) => {
    const canvas = document.createElement("canvas"); canvas.width = canvas.height = 32;
    const brush = canvas.getContext("2d")!;
    brush.fillStyle = "#76c3ae"; brush.fillRect(0, 0, 32, 32);
    (window as any).sourceFixture = {
      status: "ready", capturedAtMs: Date.now(), track: {
        title: "来源图标测试", artist: "示例歌手", album: "示例专辑",
        source: name, sourceId: "top.example.player",
        sourceIconDataUrl: icon === "valid" ? canvas.toDataURL() : icon === "invalid" ? "data:image/png;base64,bm90LXBuZw==" : null,
        artworkDataUrl: null, positionSeconds: 30, durationSeconds: 180,
        playbackStatus: "paused", playbackRate: 1, updatedAtMs: Date.now(),
      },
    };
  }, { icon, name });
  await page.route("**/src/useNowPlaying.ts", async route => {
    const response = await route.fetch();
    const imports = (await response.text()).split("export function useNowPlaying()")[0];
    await route.fulfill({ contentType: "application/javascript", body: imports + `
      export function useNowPlaying() {
        const [snapshot, setSnapshot] = useState(window.sourceFixture);
        useEffect(() => {
          const receive = event => setSnapshot({ ...event.detail, capturedAtMs: Date.now() });
          window.addEventListener('test:source', receive);
          return () => window.removeEventListener('test:source', receive);
        }, []);
        return snapshot;
      }` });
  });
  await page.goto("/");
}

test("source shows the real app icon beside its name and keeps Warm Tooltip", async ({ page }) => {
  await prepare(page, "valid");
  const source = page.locator(".source");
  await expect(source).toHaveText("Folia");
  const icon = source.locator("img");
  await expect(icon).toBeVisible();
  expect(await icon.evaluate((el: HTMLImageElement) => el.complete && el.naturalWidth > 0)).toBe(true);
  await expect(source.locator("svg")).toHaveCount(0);
  const iconBox = (await icon.boundingBox())!;
  const textBox = (await source.locator(".rt-Text").boundingBox())!;
  expect(iconBox.x + iconBox.width).toBeLessThan(textBox.x);
  expect(iconBox.width).toBeGreaterThanOrEqual(13);
  expect(iconBox.width).toBeLessThanOrEqual(18);
  // Hover now reveals the controls; the original source hint remains reachable by keyboard.
  await source.focus();
  await expect(page.locator(".warm-tooltip")).toHaveText("Folia");
  await expect(page.locator(".warm-tooltip__arrow")).toHaveCount(0);
  await page.screenshot({ path: "artifacts/source-app-icon.png" });
});

test("broken source icon falls back and a subsequent source recovers its own icon", async ({ page }) => {
  await prepare(page, "invalid");
  await expect(page.locator(".source img")).toHaveCount(0);
  await expect(page.locator(".source svg")).toBeVisible();
  await expect(page.getByRole("heading", { name: "来源图标测试" })).toBeVisible();
  await page.evaluate(() => {
    const canvas = document.createElement("canvas"); canvas.width = canvas.height = 32;
    const brush = canvas.getContext("2d")!; brush.fillStyle = "#88aaff"; brush.fillRect(0, 0, 32, 32);
    const snapshot = (window as any).sourceFixture;
    window.dispatchEvent(new CustomEvent("test:source", { detail: {
      ...snapshot, track: { ...snapshot.track, source: "Next Player", sourceId: "next.exe", sourceIconDataUrl: canvas.toDataURL() },
    } }));
  });
  await expect(page.locator(".source")).toHaveText("Next Player");
  await expect(page.locator(".source img")).toBeVisible();
  await expect(page.locator(".source svg")).toHaveCount(0);
});

test("missing source icon and long source names keep the compact layout", async ({ page }) => {
  const name = "这是一个很长的播放器应用名称用于检查图标与名称布局";
  await prepare(page, "missing", name);
  await expect(page.locator(".source svg")).toBeVisible();
  await expect(page.locator(".source img")).toHaveCount(0);
  expect(await page.locator(".music-window").evaluate(el => el.scrollWidth > el.clientWidth)).toBe(false);
  await page.locator(".source").focus();
  await expect(page.locator(".warm-tooltip")).toHaveText(name);
});

test("resolving the source display name does not replay the song transition", async ({ page }) => {
  await prepare(page, "missing", "媒体播放器");
  await expect(page.locator('.track-copy[aria-hidden="false"]')).toHaveCount(1);
  const remained = await page.evaluate(async () => {
    const before = document.querySelector('.track-copy[aria-hidden="false"]');
    const snapshot = (window as any).sourceFixture;
    window.dispatchEvent(new CustomEvent("test:source", { detail: {
      ...snapshot, track: { ...snapshot.track, source: "Folia" },
    } }));
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    return before === document.querySelector('.track-copy[aria-hidden="false"]');
  });
  expect(remained).toBe(true);
  await expect(page.locator(".source")).toHaveText("Folia");
});
