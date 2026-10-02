import { test, expect } from "@playwright/test";

test("short song and artist hints anchor to the visible text instead of the full metadata row", async ({ page }) => {
  // Replace only the OS media boundary; keep the actual layout and tooltip code.
  await page.route("**/src/useNowPlaying.ts", route => route.fulfill({
    contentType: "application/javascript", body: `export function useNowPlaying() { return {
      status: 'ready', capturedAtMs: Date.now(), track: {
        title: '夜曲', artist: '周杰伦', album: '测试专辑', source: '网易云音乐',
        artworkDataUrl: null, positionSeconds: 74, durationSeconds: 203,
        playbackStatus: 'paused', playbackRate: 1, updatedAtMs: Date.now()
      }
    }; }`,
  }));
  await page.goto("/");
  for (const selector of [".title-line h1", ".artist"]) {
    const label = page.locator(selector);
    await label.hover();
    const tooltip = page.locator(".warm-tooltip");
    await expect(tooltip).toHaveText((await label.innerText()).trim());
    await expect(tooltip.locator(".warm-tooltip__box")).toHaveCSS("opacity", "1");
    const text = await label.evaluate(element => {
      const range = document.createRange();
      range.selectNodeContents(element);
      const text = range.getBoundingClientRect();
      const row = element.getBoundingClientRect();
      return { center: (text.left + Math.min(text.right, row.right)) / 2, bottom: row.bottom };
    });
    const hint = (await tooltip.boundingBox())!;
    expect(Math.abs(hint.x + hint.width / 2 - text.center), JSON.stringify({ selector, text, hint })).toBeLessThan(2);
    expect(hint.y - text.bottom).toBeGreaterThanOrEqual(5);
    expect(hint.y - text.bottom).toBeLessThanOrEqual(9);
    await page.mouse.move(40, 210);
    await expect(tooltip).toHaveCount(0);
  }
  // Empty space to the right of a short name is not part of its hover target.
  const row = (await page.locator(".title-line").boundingBox())!;
  await page.mouse.move(row.x + row.width - 4, row.y + row.height / 2);
  await page.waitForTimeout(600);
  await expect(page.locator(".warm-tooltip")).toHaveCount(0);
});
