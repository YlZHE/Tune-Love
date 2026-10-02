import { test, expect } from "@playwright/test";

test("toolbar hints glide continuously between controls and stay inside the dark widget", async ({ page }) => {
  await page.goto("/");
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await page.getByRole("button", { name: "取消置顶", exact: true }).hover();
  const tip = page.locator(".warm-tooltip");
  await expect(tip).toBeVisible();
  await expect(tip).toHaveText("取消置顶");
  await expect(tip.locator(".warm-tooltip__arrow")).toHaveCount(0);
  await expect(tip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
  await expect(tip.locator(".warm-tooltip__box")).toHaveCSS("opacity", "1");
  const before = (await tip.boundingBox())!;
  const id = await tip.getAttribute("id");
  // Capture actual rendered frames: a separate tooltip per button or an instant
  // position jump loses the uninterrupted intermediate positions required here.
  await page.evaluate(() => {
    const frames: { x: number; width: number; visible: boolean; tilt: number }[] = [];
    (window as any).tooltipFrames = frames;
    let count = 0;
    const capture = () => {
      const tip = document.querySelector(".warm-tooltip");
      const bounds = tip?.getBoundingClientRect();
      const box = tip?.querySelector(".warm-tooltip__box");
      const matrix = new DOMMatrix(box ? getComputedStyle(box).transform : undefined);
      frames.push({ x: bounds?.x ?? 0, width: bounds?.width ?? 0, visible: !!bounds, tilt: Math.atan2(matrix.b, matrix.a) * 180 / Math.PI });
      if (++count < 40) requestAnimationFrame(capture);
    };
    // Arm on the actual label change, after actionability checks and before the
    // next animation frame. This also works with native WebView2 pointer events.
    const tooltip = document.querySelector(".warm-tooltip")!;
    const observer = new MutationObserver(() => {
      if (!tooltip.textContent?.includes("设置")) return;
      observer.disconnect();
      capture();
    });
    observer.observe(tooltip, { childList: true, characterData: true, subtree: true });
  });
  await page.getByRole("button", { name: "设置", exact: true }).hover();
  await expect(tip).toHaveText("设置");
  await expect(tip).toHaveAttribute("id", id!);
  await expect.poll(() => page.evaluate(() => (window as any).tooltipFrames.length)).toBe(40);
  const after = (await tip.boundingBox())!;
  const frames: { x: number; width: number; visible: boolean; tilt: number }[] = await page.evaluate(() => (window as any).tooltipFrames);
  expect(after.x).toBeGreaterThan(before.x + 10);
  expect(after.width).toBeLessThan(before.width);
  expect(frames.every(frame => frame.visible)).toBe(true);
  expect(frames.some(frame => frame.x > before.x + 2 && frame.x < after.x - 2), JSON.stringify({ before, after, frames })).toBe(true);
  expect(frames.some(frame => frame.width < before.width - 2 && frame.width > after.width + 2)).toBe(true);
  expect(frames.some(frame => Math.abs(frame.tilt) > 0.1)).toBe(true);
  await page.getByRole("button", { name: "关闭", exact: true }).hover();
  await expect(tip).toHaveText("关闭");
  const bounds = (await tip.boundingBox())!;
  expect(bounds.y).toBeGreaterThan(35);
  expect(bounds.x).toBeGreaterThanOrEqual(0);
  expect(bounds.x + bounds.width).toBeLessThanOrEqual(468);
  expect(bounds.y + bounds.height).toBeLessThanOrEqual(242);
  await page.mouse.move(40, 210);
  await expect(tip).toHaveCount(0);
});

test("keyboard hints stay accessible and reduced motion disables tooltip movement", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("/");
  await page.keyboard.press("Tab");
  const pin = page.getByRole("button", { name: "取消置顶", exact: true });
  await expect(pin).toBeFocused();
  const tip = page.locator(".warm-tooltip");
  await expect(tip).toHaveText("取消置顶");
  const id = await tip.getAttribute("id");
  await expect(pin).toHaveAttribute("aria-describedby", id!);
  await expect(tip.locator(".warm-tooltip__box")).toHaveCSS("transform", "none");
  await page.keyboard.press("Tab");
  await expect(page.getByRole("button", { name: "设置", exact: true })).toBeFocused();
  await expect(tip).toHaveText("设置");
  await page.keyboard.press("Escape");
  await expect(tip).toHaveCount(0);
});
