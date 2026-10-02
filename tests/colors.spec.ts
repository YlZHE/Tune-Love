import { test, expect, type BrowserContext, type Page } from "@playwright/test";

async function mediaFixture(context: BrowserContext) {
  await context.addInitScript(() => {
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 64;
    const ctx = canvas.getContext("2d")!;
    ctx.fillStyle = "#cc3344"; ctx.fillRect(0, 0, 64, 64);
    (window as any).colorSnapshot = { status: "ready", capturedAtMs: Date.now(), track: {
      title: "配色测试", artist: "示例歌手", album: "示例专辑", source: "测试播放器",
      artworkDataUrl: canvas.toDataURL(), positionSeconds: 30, durationSeconds: 180,
      playbackStatus: "paused", playbackRate: 1, updatedAtMs: Date.now(),
    } };
  });
  await context.route("**/src/useNowPlaying.ts", async route => {
    const response = await route.fetch();
    const imports = (await response.text()).split("export function useNowPlaying()")[0];
    await route.fulfill({ contentType: "application/javascript", body: imports + `
      export function useNowPlaying() {
        const [snapshot, setSnapshot] = useState(window.colorSnapshot);
        useEffect(() => {
          const receive = e => setSnapshot(e.detail);
          window.addEventListener('test:color-media', receive);
          return () => window.removeEventListener('test:color-media', receive);
        }, []);
        return snapshot;
      }` });
  });
}

async function openSettings(page: Page) {
  const popup = page.waitForEvent("popup");
  await page.getByRole("button", { name: "设置", exact: true }).click();
  const settings = await popup;
  await settings.setViewportSize({ width: 760, height: 540 });
  return settings;
}

test("automatic cover mode prefers colorful pixels over dominant black and white without selection", async ({ page, context }) => {
  await mediaFixture(context);
  await context.addInitScript(() => {
    const canvas = document.createElement("canvas"); canvas.width = canvas.height = 100;
    const ctx = canvas.getContext("2d")!;
    let x = 0;
    for (const [color, width] of [["#050505", 50], ["#fafafa", 40], ["#cc3344", 5], ["#2255cc", 3], ["#44bb66", 2]] as const) {
      ctx.fillStyle = color; ctx.fillRect(x, 0, width, 100); x += width;
    }
    // Previous versions stored a user-selected slot. It must no longer select white.
    localStorage.setItem("helper-colors-v1", JSON.stringify({ mode: "cover", manualColor: "#55aaff", coverIndex: 1 }));
    (window as any).colorSnapshot.track.artworkDataUrl = canvas.toDataURL();
  });
  await page.goto("/");
  const settings = await openSettings(page);
  await settings.getByRole("switch", { name: "从封面提取主色" }).check();
  await expect(settings.getByRole("button", { name: /^封面颜色/ })).toHaveCount(0);
  const palette = () => page.locator(".music-window").evaluate(el =>
    [0, 1, 2].map(i => (el as HTMLElement).style.getPropertyValue(`--strand-${i}`)));
  await expect.poll(palette).toEqual(["#cc3344", "#2255cc", "#44bb66"]);
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#cc3344");
  expect(await palette()).toEqual(["#cc3344", "#2255cc", "#44bb66"]);
  await page.reload(); await settings.reload();
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#cc3344");
  await settings.screenshot({ path: "artifacts/auto-cover-colors.png" });
});

test("grayscale covers prefer white while sparse cover color yields tonal accents", async ({ page, context }) => {
  await mediaFixture(context); await page.goto("/");
  const settings = await openSettings(page);
  await settings.getByRole("textbox", { name: "主色值" }).fill("#55aaff");
  await settings.getByRole("switch", { name: "从封面提取主色" }).check();
  for (const colorful of [false, true]) {
    for (const target of [page, settings]) await target.evaluate(colorful => {
      const canvas = document.createElement("canvas"); canvas.width = canvas.height = 100;
      const ctx = canvas.getContext("2d")!;
      ctx.fillStyle = "#050505"; ctx.fillRect(0, 0, 100, 100);
      ctx.fillStyle = "#fafafa"; ctx.fillRect(0, 0, 40, 100);
      ctx.fillStyle = colorful ? "#cc3344" : "#888888"; ctx.fillRect(0, 0, 10, 100);
      const previous = (window as any).colorSnapshot;
      window.dispatchEvent(new CustomEvent("test:color-media", { detail: { ...previous,
        track: { ...previous.track, artworkDataUrl: canvas.toDataURL() } } }));
    }, colorful);
    await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", colorful ? "#cc3344" : "#fafafa");
    const palette = await page.locator(".music-window").evaluate(el => [0, 1, 2].map(i => (el as HTMLElement).style.getPropertyValue(`--strand-${i}`)));
    expect(new Set(palette).size).toBe(3);
    expect(palette).not.toContain("#050505"); expect(palette).not.toContain("#888888");
    if (colorful) expect(palette).not.toContain("#fafafa");
  }
});

test("a single light colorful tone always fills all three effect colors", async ({ page, context }) => {
  await mediaFixture(context);
  await context.addInitScript(() => {
    const canvas = document.createElement("canvas"); canvas.width = canvas.height = 64;
    const ctx = canvas.getContext("2d")!; ctx.fillStyle = "#ff8585"; ctx.fillRect(0, 0, 64, 64);
    (window as any).colorSnapshot.track.artworkDataUrl = canvas.toDataURL();
    localStorage.setItem("helper-colors-v1", JSON.stringify({ mode: "cover", manualColor: "#55aaff" }));
  });
  await page.goto("/");
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#ff8585");
  const palette = await page.locator(".music-window").evaluate(el => [0, 1, 2].map(i => (el as HTMLElement).style.getPropertyValue(`--strand-${i}`)));
  expect(palette.every(color => /^#[0-9a-f]{6}$/.test(color))).toBe(true);
  expect(new Set(palette).size).toBe(3);
  expect(palette.every(color => color.slice(1, 3) === "ff" && color.slice(3, 5) === color.slice(5, 7))).toBe(true);
});

test("manual color derives three tonal colors and ignores new covers", async ({ page, context }) => {
  await mediaFixture(context); await page.goto("/");
  const settings = await openSettings(page);
  await settings.getByRole("textbox", { name: "主色值" }).fill("#2255cc");
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#2255cc");
  const palette = () => page.locator(".music-window").evaluate(el =>
    [0, 1, 2].map(i => (el as HTMLElement).style.getPropertyValue(`--strand-${i}`)));
  const initial = await palette();
  expect(initial.every(color => /^#[0-9a-f]{6}$/.test(color))).toBe(true);
  expect(new Set(initial).size).toBe(3);
  await page.evaluate(() => {
    const previous = (window as any).colorSnapshot;
    window.dispatchEvent(new CustomEvent("test:color-media", { detail: { ...previous,
      track: { ...previous.track, artworkDataUrl: null, title: "Next song" } } }));
  });
  await expect(page.getByRole("heading", { name: "Next song" })).toBeVisible();
  expect(await palette()).toEqual(initial);
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#2255cc");
});

test("color switch keeps its rounded track without painting a square behind it", async ({ page }) => {
  await page.setViewportSize({ width: 760, height: 540 });
  await page.goto("/?view=settings");
  await page.getByRole("textbox", { name: "主色值" }).fill("#55aaff");
  const automatic = page.getByRole("switch", { name: "从封面提取主色" });
  await automatic.focus();
  await automatic.press("Space");
  await expect(automatic).toBeChecked();
  // The app previously painted the square button, but Radix draws the track in ::before.
  await expect(automatic).toHaveCSS("background-color", "rgba(0, 0, 0, 0)");
  const track = await automatic.evaluate(el => {
    const style = getComputedStyle(el, "::before");
    return { radius: parseFloat(style.borderTopLeftRadius), height: parseFloat(style.height),
      image: style.backgroundImage, outline: style.outlineStyle };
  });
  expect(track.radius).toBeGreaterThanOrEqual(track.height / 2);
  await expect.poll(() => automatic.evaluate(el => getComputedStyle(el, "::before").backgroundImage))
    .toContain("rgb(85, 170, 255)");
  expect(track.outline).not.toBe("none");
  await automatic.press("Space");
  await expect(automatic).not.toBeChecked();
  await expect(automatic).toHaveCSS("background-color", "rgba(0, 0, 0, 0)");
  await expect(page.getByRole("textbox", { name: "主色值" })).toHaveValue("#55aaff");
});

test("cover background and accent colors transition through intermediate frames when the song changes", async ({ page, context }) => {
  await mediaFixture(context);
  await page.goto("/");
  const settings = await openSettings(page);
  await settings.getByRole("switch", { name: "从封面提取主色" }).check();
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#cc3344");
  const blueCover = await page.evaluate(() => {
    const canvas = document.createElement("canvas"); canvas.width = canvas.height = 64;
    const context = canvas.getContext("2d")!; context.fillStyle = "#2255cc"; context.fillRect(0, 0, 64, 64);
    return canvas.toDataURL();
  });
  const frames = await page.evaluate(async artworkDataUrl => {
    const previous = (window as any).colorSnapshot;
    window.dispatchEvent(new CustomEvent("test:color-media", {
      detail: { ...previous, capturedAtMs: Date.now(), track: { ...previous.track, artworkDataUrl } },
    }));
    await new Promise<void>(resolve => {
      const observer = new MutationObserver(() => {
        if (document.querySelector(".music-window")?.getAttribute("data-primary-color") === "#2255cc") {
          observer.disconnect(); resolve();
        }
      });
      observer.observe(document.querySelector(".music-window")!, { attributes: true, attributeFilter: ["data-primary-color"] });
    });
    const values: string[] = [];
    const accentValues: string[] = [];
    const progressValues: string[] = [];
    const pinValues: string[] = [];
    const started = performance.now();
    await new Promise<void>(resolve => {
      const sample = () => {
        values.push(getComputedStyle(document.querySelector(".color-atmosphere")!).backgroundColor);
        accentValues.push(getComputedStyle(document.querySelector(".brand > svg")!).color);
        progressValues.push(getComputedStyle(document.querySelector(".rt-ProgressIndicator")!).backgroundColor);
        pinValues.push(getComputedStyle(document.querySelector(".window-button.is-active")!).color);
        if (performance.now() - started < 500) requestAnimationFrame(sample); else resolve();
      };
      requestAnimationFrame(sample);
    });
    return { values, accentValues, progressValues, pinValues };
  }, blueCover);
  expect(frames.values.length).toBeGreaterThan(10);
  expect(new Set(frames.values).size).toBeGreaterThan(3);
  expect(new Set(frames.accentValues).size).toBeGreaterThan(3);
  expect(new Set(frames.progressValues).size).toBeGreaterThan(3);
  expect(new Set(frames.pinValues).size).toBeGreaterThan(3);
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#2255cc");
});

test("manual palette updates the widget across windows and survives reopening", async ({ page, context }) => {
  await mediaFixture(context);
  await page.goto("/");
  const settings = await openSettings(page);
  const input = settings.getByRole("textbox", { name: "主色值" });
  await input.fill("#55aaff");
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#55aaff");
  await expect(page.locator(".music-window")).toHaveAttribute("data-color-mode", "manual");
  await input.fill("");
  await input.pressSequentially("123abc");
  await expect(input).toHaveValue("#123abc");
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#123abc");
  await settings.getByRole("slider", { name: "Hue", exact: true }).press("ArrowRight");
  await expect(page.locator(".music-window")).not.toHaveAttribute("data-primary-color", "#123abc");
  await settings.getByRole("button", { name: "珊瑚", exact: true }).click();
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#f4a58c");
  await settings.screenshot({ path: "artifacts/settings-colors-manual.png" });
  await page.reload();
  await settings.reload();
  await expect(input).toHaveValue("#f4a58c");
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#f4a58c");
  await settings.setViewportSize({ width: 520, height: 380 });
  expect(await settings.locator(".settings-page-content").evaluate(el => el.scrollWidth > el.clientWidth)).toBe(false);
});

test("cover mode extracts real pixels, follows new covers and falls back without losing the manual color", async ({ page, context }) => {
  await mediaFixture(context);
  await page.goto("/");
  const settings = await openSettings(page);
  await settings.getByRole("textbox", { name: "主色值" }).fill("#55aaff");
  const automatic = settings.getByRole("switch", { name: "从封面提取主色" });
  await automatic.check();
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#cc3344");
  await expect(page.locator(".music-window")).toHaveAttribute("data-color-mode", "cover");
  await settings.screenshot({ path: "artifacts/settings-colors-cover.png" });
  for (const color of ["#2255cc", null]) {
    for (const target of [page, settings]) {
      await target.evaluate(color => {
        const canvas = document.createElement("canvas"); canvas.width = canvas.height = 64;
        const ctx = canvas.getContext("2d")!;
        if (color) { ctx.fillStyle = color; ctx.fillRect(0, 0, 64, 64); }
        const previous = (window as any).colorSnapshot;
        window.dispatchEvent(new CustomEvent("test:color-media", { detail: {
          ...previous, capturedAtMs: Date.now(), track: { ...previous.track, artworkDataUrl: color ? canvas.toDataURL() : null },
        } }));
      }, color);
    }
    await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", color ?? "#55aaff");
  }
  await expect(settings.getByText("暂无封面，暂用手动主色")).toBeVisible();
  await automatic.uncheck();
  await expect(settings.getByRole("textbox", { name: "主色值" })).toHaveValue("#55aaff");
  await expect(page.locator(".music-window")).toHaveAttribute("data-primary-color", "#55aaff");
});
