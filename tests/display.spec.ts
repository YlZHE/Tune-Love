import { test, expect } from "@playwright/test";

test("compact layout stays dark regardless of system or previously saved theme", async ({ page }) => {
  await page.emulateMedia({ colorScheme: "light" });
  await page.addInitScript(() => localStorage.setItem("helper-theme", "light"));
  await page.goto("/");
  const widget = await page.locator(".music-window").boundingBox();
  expect(widget!.width / widget!.height).toBeGreaterThan(1.8);
  const cover = await page.locator(".album-cover").boundingBox();
  const title = await page.getByRole("heading", { level: 1 }).boundingBox();
  expect(cover!.x + cover!.width).toBeLessThan(title!.x);
  await expect(page.getByText("等待播放", { exact: true })).toBeVisible();
  await expect(page.locator(".music-window")).toHaveAttribute("data-theme", "dark");
  await expect(page.getByRole("button", { name: /[深浅]色外观/ })).toHaveCount(0);
  const pin = await page.getByRole("button", { name: "取消置顶", exact: true }).boundingBox();
  const settings = await page.getByRole("button", { name: "设置", exact: true }).boundingBox();
  const close = await page.getByRole("button", { name: "关闭", exact: true }).boundingBox();
  expect(settings!.x).toBeGreaterThan(pin!.x);
  expect(settings!.x).toBeLessThan(close!.x);
  await page.screenshot({ path: "artifacts/empty-dark.png" });
});

test("settings opens as a separate spacious page and reuses it until closed", async ({ page, context }) => {
  await page.goto("/");
  const trigger = page.getByRole("button", { name: "设置", exact: true });
  const opening = page.waitForEvent("popup");
  await trigger.click();
  const settings = await opening;
  await expect(settings).toHaveURL(/view=settings/);
  await settings.setViewportSize({ width: 760, height: 540 });
  await expect(settings.getByRole("heading", { name: "设置", exact: true })).toBeVisible();
  await expect(page.locator(".music-window")).toBeVisible();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  const bounds = (await settings.locator(".settings-page").boundingBox())!;
  expect(bounds.width).toBeGreaterThanOrEqual(700);
  expect(bounds.height).toBeGreaterThanOrEqual(500);
  await settings.screenshot({ path: "artifacts/settings-page.png" });
  await expect(trigger).toBeEnabled();
  await trigger.click();
  expect(context.pages().filter(p => p.url().includes("view=settings"))).toHaveLength(1);
  const closing = settings.waitForEvent("close");
  await settings.getByRole("button", { name: "关闭设置" }).click();
  await closing;
  await expect(page.locator(".music-window")).toBeVisible();
  const reopening = page.waitForEvent("popup");
  await trigger.click();
  const reopened = await reopening;
  await expect(reopened.getByRole("heading", { name: "设置", exact: true })).toBeVisible();
  await reopened.close();
});

test("long metadata, missing cover and paused progress stay inside the window", async ({ page }) => {
  // Replace only the OS boundary, keeping the real rendering and progress code.
  await page.route("**/src/useNowPlaying.ts", route => route.fulfill({
    contentType: "application/javascript", body: `export function useNowPlaying() { return {
      status: 'ready', capturedAtMs: Date.now(), track: {
        title: '这是一首用来测试长标题是否正确显示的歌曲（现场特别版本）',
        artist: '歌手甲、歌手乙、歌手丙、歌手丁', album: '测试专辑', source: '测试播放器桌面客户端 com.example.desktop.music.streaming.application',
        artworkDataUrl: null, positionSeconds: 74, durationSeconds: 203,
        playbackStatus: 'paused', playbackRate: 1, updatedAtMs: Date.now()
      }
    }; }`,
  }));
  await page.goto("/");
  await expect(page.getByText("已暂停", { exact: true })).toBeVisible();
  await expect(page.getByRole("progressbar")).toHaveAttribute("aria-valuetext", "1:14 / 3:23");
  await expect(page.getByText("暂无封面")).toBeVisible();
  const dimensions = await page.locator(".music-window").evaluate(el => ({
    x: el.scrollWidth > el.clientWidth, y: el.scrollHeight > el.clientHeight,
  }));
  expect(dimensions).toEqual({ x: false, y: false });
  await page.screenshot({ path: "artifacts/long-title-fixture.png" });
  for (const selector of [".artist", ".source", ".title-line h1"]) {
    const trigger = page.locator(selector);
    const text = (await trigger.innerText()).trim();
    if (selector === ".source") { await page.mouse.move(0, 0); await trigger.focus(); }
    else await trigger.hover();
    const tooltip = page.locator(".warm-tooltip");
    await expect(tooltip).toHaveText(text);
    await expect(tooltip.locator(".warm-tooltip__arrow")).toHaveCount(0);
    await expect(tooltip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
    expect(await trigger.getAttribute("title")).toBeNull();
    const bounds = (await tooltip.boundingBox())!;
    expect(bounds.x).toBeGreaterThanOrEqual(0);
    expect(bounds.y).toBeGreaterThanOrEqual(0);
    expect(bounds.x + bounds.width).toBeLessThanOrEqual(468);
    expect(bounds.y + bounds.height).toBeLessThanOrEqual(242);
    await page.mouse.move(40, 210);
    await expect(tooltip).toHaveCount(0);
  }
});
