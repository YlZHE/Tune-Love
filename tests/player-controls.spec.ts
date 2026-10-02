import { test, expect, type Page } from "@playwright/test";

async function prepare(page: Page, hasTrack = true) {
  await page.addInitScript(({ hasTrack }) => {
    const state = window as any;
    state.commands = [];
    state.effectRequests = [];
    state.devocalRequests = [];
    state.devocal = { phase: "off", held: false, latencyMs: null, loadRatio: null, fallbackReason: null,
      sessionOverridden: false, inputSilent: false, error: null };
    state.isTauri = true;
    state.controlMedia = { status: hasTrack ? "ready" : "empty", targetGeneration: 1, capturedAtMs: Date.now(), track: hasTrack ? {
      sourceId: "player.exe", source: "Folia", title: "控制区预览", artist: "示例歌手", album: "示例专辑",
      artworkDataUrl: null, sourceIconDataUrl: null, positionSeconds: 42, durationSeconds: 180,
      playbackStatus: "playing", playbackRate: 1, updatedAtMs: Date.now(),
      transport: { sessionId: "player.exe", canPlay: true, canPause: true, canPrevious: true, canNext: true },
    } : null };
    // Effect previews remain read-only. Transport crosses the native boundary.
    state.__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "main" } },
      invoke: async (command: string, args: any) => {
        state.commands.push(command);
        if (command === "plugin:window|is_always_on_top") return true;
        if (command === "get_devocal_status") return structuredClone(state.devocal);
        if (command === "devocal_command") {
          state.devocalRequests.push(args.request);
          state.devocal = { ...state.devocal, held: true, ...(args.request.action === "enable"
            ? { phase: "devocal", latencyMs: 45.4 } : { phase: "passthrough", latencyMs: null }) };
          return structuredClone(state.devocal);
        }
        if (command === "control_media") {
          const action = args.request.action;
          if (action === "play" || action === "pause") {
            state.controlMedia = { ...state.controlMedia, track: { ...state.controlMedia.track,
              playbackStatus: action === "play" ? "playing" : "paused" } };
            window.dispatchEvent(new CustomEvent("test:media", { detail: state.controlMedia }));
          }
          return;
        }
        if (command === "get_audio_level") return { sourceId: "player.exe", status: "capturing",
          trackKey: '["player.exe","控制区预览","示例歌手","示例专辑"]',
          rms: 0.2, peak: 0.4, level: 0.4, bands: [0.5, 0.4, 0.3], updatedAtMs: Date.now() };
        if (command === "get_key_detection") return { sourceId: null, trackKey: null,
          targetGeneration: 0, status: "unavailable", key: null, updatedAtMs: Date.now() };
        if (command === "autotune_command") {
          state.effectRequests.push(args.request);
          return { ok: true, state: { phase: "disconnected", connectionId: null,
            target: null, capabilities: [], instanceCount: 0, delivery: null, error: null, audioVerified: false } };
        }
        throw new Error(`Unexpected control IPC: ${command}`);
      },
    };
  }, { hasTrack });
  await page.route("**/src/useNowPlaying.ts", async route => {
    const imports = (await (await route.fetch()).text()).split("export function useNowPlaying()")[0];
    await route.fulfill({ contentType: "application/javascript", body: imports + `
      export function useNowPlaying() {
        const [snapshot, setSnapshot] = useState(window.controlMedia);
        useEffect(() => {
          const update = event => setSnapshot(event.detail);
          window.addEventListener('test:media', update);
          return () => window.removeEventListener('test:media', update);
        }, []);
        return { ...snapshot, capturedAtMs: Date.now() };
      }` });
  });
  await page.goto("/");
}

const reveal = (page: Page) => page.locator(".music-window").hover({ position: { x: 18, y: 45 } });

async function openDials(page: Page) {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  return page.getByRole("dialog", { name: "Auto-Tune 参数", exact: true });
}

function dialPoint(box: { x: number; y: number; width: number; height: number }, fraction: number) {
  // The approved 260 degree arc starts at 140 degrees in SVG coordinates.
  const angle = (140 + 260 * fraction) * Math.PI / 180;
  return { x: box.x + box.width * (0.5 + 0.4 * Math.cos(angle)),
    y: box.y + box.height * (0.5 + 0.4 * Math.sin(angle)) };
}

test("Comet parameters fit in one compact nonmodal row with editable centered values", async ({ page }) => {
  const panel = await openDials(page);
  await expect(panel.getByRole("heading")).toHaveCount(0);
  await expect(panel.getByRole("button")).toHaveCount(0);
  await expect(page.locator(".rt-DialogOverlay")).toHaveCount(0);
  await expect(panel.getByRole("slider")).toHaveCount(3);
  await expect(panel.getByRole("spinbutton")).toHaveCount(3);
  await expect.poll(async () => (await panel.boundingBox())!.width).toBeCloseTo(340, 0);
  const box = (await panel.boundingBox())!;
  expect(box.height).toBeLessThanOrEqual(150);
  const centers = [];
  for (const name of ["Flex-Tune", "Natural Vibrato", "Humanize"]) {
    const dial = (await panel.getByRole("slider", { name, exact: true }).boundingBox())!;
    expect(dial.width).toBeGreaterThanOrEqual(80);
    expect(dial.width).toBeCloseTo(dial.height, 1);
    const number = (await panel.getByRole("spinbutton", { name: `${name}数值` }).boundingBox())!;
    expect(Math.abs(number.x + number.width / 2 - dial.x - dial.width / 2)).toBeLessThan(1);
    expect(Math.abs(number.y + number.height / 2 - dial.y - dial.height / 2)).toBeLessThan(1);
    const label = (await panel.getByText(name, { exact: true }).boundingBox())!;
    expect(label.y).toBeGreaterThanOrEqual(dial.y + dial.height);
    centers.push(dial.y + dial.height / 2);
  }
  expect(Math.max(...centers) - Math.min(...centers)).toBeLessThan(1);
  expect(box.x).toBeGreaterThanOrEqual(0); expect(box.y).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width).toBeLessThanOrEqual(468);
  expect(box.y + box.height).toBeLessThanOrEqual(242);
  await page.screenshot({ path: "artifacts/comet-parameters.png" });
  await page.mouse.click(0, 0);
  await expect(panel).toHaveCount(0);
});

test("Comet number text is centered without inherited input indentation", async ({ page }) => {
  const panel = await openDials(page);
  for (const [name, value] of [["Flex-Tune", "74"], ["Natural Vibrato", "-0.3"], ["Humanize", "100"]]) {
    const input = panel.getByRole("spinbutton", { name: `${name}数值` });
    await input.fill(value); await input.press("Enter");
    await expect(input).toHaveCSS("text-align", "center");
    await expect(input).toHaveCSS("text-indent", "0px");
    const padding = await input.evaluate(el => {
      const style = getComputedStyle(el);
      return [style.paddingLeft, style.paddingRight];
    });
    expect(padding[0]).toBe(padding[1]);
  }
});

test("Natural Vibrato draws only the signed distance from zero, including after crossing zero", async ({ page }) => {
  const panel = await openDials(page);
  const dial = panel.getByRole("slider", { name: "Natural Vibrato", exact: true });
  const input = panel.getByRole("spinbutton", { name: "Natural Vibrato数值" });
  const lit = dial.locator(".comet-dial__lit");
  // Hand-checked lengths in the 200 x 200 viewBox: radius 80, half arc 130 degrees.
  for (const [value, length, side] of [["0", 0, 0], ["-0.3", 4.54, -1], ["0.3", 4.54, 1],
    ["-12", 181.51, -1], ["12", 181.51, 1], ["0", 0, 0], ["6", 90.76, 1], ["-6", 90.76, -1], ["0", 0, 0]] as const) {
    await input.fill(value); await input.press("Enter");
    await expect(dial).toHaveAttribute("aria-valuenow", value);
    if (side === 0) {
      await expect(lit).toHaveAttribute("d", "");
      await expect(dial.locator(".comet-dial__head")).toHaveAttribute("cx", "100.000");
      await expect(dial.locator(".comet-dial__head")).toHaveAttribute("cy", "20.000");
    } else {
      await expect.poll(() => lit.evaluate(el => (el as SVGPathElement).getTotalLength())).toBeCloseTo(length, 1);
      const arc = await lit.evaluate(el => {
        const path = el as SVGPathElement; const length = path.getTotalLength();
        return [0, 0.25, 0.5, 0.75, 1].map(f => { const p = path.getPointAtLength(length * f); return { x: p.x, y: p.y }; });
      });
      expect(arc.some(point => Math.abs(point.x - 100) < 0.01 && Math.abs(point.y - 20) < 0.01)).toBe(true);
      expect(arc.every(point => side < 0 ? point.x <= 100.01 : point.x >= 99.99)).toBe(true);
    }
  }
  for (const name of ["Flex-Tune", "Humanize"]) {
    const unipolar = panel.getByRole("slider", { name, exact: true });
    await unipolar.press("End");
    await expect.poll(() => unipolar.locator(".comet-dial__lit").evaluate(el => (el as SVGPathElement).getTotalLength())).toBeCloseTo(363.03, 1);
    await unipolar.press("Home");
    await expect(unipolar.locator(".comet-dial__lit")).toHaveAttribute("d", "");
  }
});

test("Comet help explains each parameter on name or ring hover and fits the compact window", async ({ page }) => {
  const panel = await openDials(page);
  for (const [name, meaning] of [["Flex-Tune", /滑音|音高变化/], ["Natural Vibrato", /原有.*颤音/], ["Humanize", /长音.*自然/]] as const) {
    const label = panel.getByText(name, { exact: true });
    const dial = panel.getByRole("slider", { name, exact: true });
    for (const target of [label, dial]) {
      await target.hover({ position: target === dial ? { x: 43, y: 9 } : undefined });
      const tip = page.getByRole("tooltip");
      await expect(tip).toContainText(meaning);
      await expect(tip).toBeInViewport({ ratio: 1 });
      await expect(tip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
      await expect(tip.locator(".warm-tooltip__arrow")).toHaveCount(0);
      await page.mouse.move(0, 0);
      await expect(tip).toHaveCount(0);
    }
  }
  await page.keyboard.press("Escape");
  await expect(page.getByRole("tooltip")).toHaveCount(0);
});

test("Comet help glides between parameter names without an anchor snap", async ({ page }) => {
  const panel = await openDials(page);
  await panel.getByText("Flex-Tune", { exact: true }).hover();
  const tip = page.getByRole("tooltip");
  await expect(tip).toContainText("滑音");
  await expect(tip.locator(".warm-tooltip__box")).toHaveCSS("opacity", "1");
  const before = (await tip.boundingBox())!;
  const id = await tip.getAttribute("id");
  await page.evaluate(() => {
    const frames: { center: number; tilt: number; visible: boolean }[] = [];
    (window as any).dialHelpFrames = frames;
    const capture = () => {
      const tip = document.querySelector('[role="tooltip"]');
      const box = tip?.getBoundingClientRect();
      const surface = tip?.querySelector(".warm-tooltip__box");
      const matrix = new DOMMatrix(surface ? getComputedStyle(surface).transform : undefined);
      frames.push({ center: box ? box.x + box.width / 2 : 0,
        tilt: Math.atan2(matrix.b, matrix.a) * 180 / Math.PI, visible: !!box });
      if (frames.length < 40) requestAnimationFrame(capture);
    };
    const observer = new MutationObserver(() => {
      if (!document.querySelector('[role="tooltip"]')?.textContent?.includes("原有")) return;
      observer.disconnect(); capture();
    });
    observer.observe(document.querySelector('[role="tooltip"]')!, { childList: true, subtree: true });
  });
  await panel.getByText("Natural Vibrato", { exact: true }).hover();
  await expect(tip).toContainText("原有");
  await expect(tip).toHaveAttribute("id", id!);
  await expect.poll(() => page.evaluate(() => (window as any).dialHelpFrames.length)).toBe(40);
  const after = (await tip.boundingBox())!;
  const frames: { center: number; tilt: number; visible: boolean }[] = await page.evaluate(() => (window as any).dialHelpFrames);
  const low = before.x + before.width / 2; const high = after.x + after.width / 2;
  expect(high - low).toBeGreaterThan(60);
  expect(frames.every(frame => frame.visible)).toBe(true);
  expect(new Set(frames.filter(frame => frame.center > low + 2 && frame.center < high - 2)
    .map(frame => Math.round(frame.center))).size).toBeGreaterThanOrEqual(4);
  expect(frames.some(frame => Math.abs(frame.tilt) > 0.1)).toBe(true);
});

test("Comet keyboard edits retain signed decimal precision with explanatory Warm Tooltip", async ({ page }) => {
  const panel = await openDials(page);
  const dial = panel.getByRole("slider", { name: "Natural Vibrato", exact: true });
  await expect(dial).toHaveJSProperty("tagName", "svg");
  await page.keyboard.press("Tab");
  await dial.focus(); await dial.press("Home"); await dial.press("ArrowRight");
  await expect(dial).toHaveAttribute("aria-valuenow", "-11.9");
  await expect(panel.getByRole("spinbutton", { name: "Natural Vibrato数值" })).toHaveValue("-11.9");
  const tip = page.getByRole("tooltip");
  await expect(tip).toContainText(/原有.*颤音/);
  // Chromium's intersection ratio can round a fully visible fractional box
  // to 0.99999988. Check its actual edges instead of requiring exact float 1.
  await expect.poll(() => tip.evaluate(el => {
    const box = el.getBoundingClientRect();
    return box.width > 0 && box.height > 0 && box.left >= 0 && box.top >= 0
      && box.right <= innerWidth && box.bottom <= innerHeight;
  })).toBe(true);
  await expect(tip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
  await expect(tip.locator(".warm-tooltip__arrow")).toHaveCount(0);
  await dial.press("End"); await dial.press("ArrowUp");
  await expect(dial).toHaveAttribute("aria-valuenow", "12");
  await dial.press("PageDown");
  await expect(dial).toHaveAttribute("aria-valuenow", "11");
  for (const name of ["Flex-Tune", "Humanize"])
    await expect(panel.getByRole("slider", { name, exact: true })).toHaveAttribute("aria-valuenow", "0");
  await page.keyboard.press("Escape");
  await expect(page.getByRole("tooltip")).toHaveCount(0);
});

test("Comet circular dragging lights the tail without flinging the committed parameter", async ({ page }) => {
  const panel = await openDials(page);
  const dial = panel.getByRole("slider", { name: "Flex-Tune", exact: true });
  await expect.poll(async () => (await dial.boundingBox())!.width).toBeCloseTo(86, 1);
  const box = (await dial.boundingBox())!;
  const first = dialPoint(box, 0.25);
  await page.mouse.move(first.x, first.y); await page.mouse.down();
  await expect(dial).toHaveAttribute("aria-valuenow", "25");
  const tip = page.getByRole("tooltip");
  await expect(tip).toHaveCount(0);
  for (const fraction of [0.5, 0.75]) {
    const point = dialPoint(box, fraction);
    await page.mouse.move(point.x, point.y, { steps: 4 });
    await expect(dial).toHaveAttribute("aria-valuenow", String(fraction * 100));
    await expect(panel.getByRole("spinbutton", { name: "Flex-Tune数值" })).toHaveValue(String(fraction * 100));
    await expect(tip).toHaveCount(0);
  }
  await expect.poll(() => dial.locator(".comet-dial__comet path").evaluateAll(nodes =>
    nodes.some(node => Number(getComputedStyle(node).opacity) > 0.01))).toBe(true);
  await page.screenshot({ path: "artifacts/comet-parameters-dragging.png" });
  await page.mouse.up();
  await expect(tip).toHaveCount(0);
  await expect.poll(() => dial.locator(".comet-dial__comet path").evaluateAll(nodes =>
    nodes.every(node => Number(getComputedStyle(node).opacity) === 0))).toBe(true);
  await expect(dial).toHaveAttribute("aria-valuenow", "75");
  await expect(panel.getByRole("spinbutton", { name: "Flex-Tune数值" })).toHaveValue("75");
  const commands = await page.evaluate(() => (window as any).commands as string[]);
  expect(commands.every(command => ["get_audio_level", "get_key_detection", "autotune_command", "plugin:window|is_always_on_top", "get_devocal_status", "devocal_command"].includes(command))).toBe(true);
  expect(await page.evaluate(() => (window as any).effectRequests.every((r: any) => r.op === "status"))).toBe(true);
});

test("Comet pointer capture loss clears the drag and tooltip before the next gesture", async ({ page }) => {
  const panel = await openDials(page);
  const dial = panel.getByRole("slider", { name: "Humanize", exact: true });
  await expect.poll(async () => (await dial.boundingBox())!.width).toBeCloseTo(86, 1);
  const box = (await dial.boundingBox())!;
  await dial.evaluate(el => el.addEventListener("pointerdown", event => {
    (window as any).dialPointerId = (event as PointerEvent).pointerId;
  }, { once: true }));
  const start = dialPoint(box, 0.25); const end = dialPoint(box, 0.75);
  await page.mouse.move(start.x, start.y); await page.mouse.down();
  await page.mouse.move(end.x, end.y, { steps: 4 });
  await expect(dial).toHaveAttribute("aria-valuenow", "75");
  await expect(page.getByRole("tooltip")).toHaveCount(0);
  await dial.evaluate(el => {
    const id = (window as any).dialPointerId;
    if (!el.hasPointerCapture(id)) throw new Error("Expected real pointer capture");
    el.releasePointerCapture(id);
  });
  await page.mouse.move(end.x + 1, end.y);
  await expect(page.locator(".comet-dial[data-dragging]")).toHaveCount(0);
  await expect(page.getByRole("tooltip")).toHaveCount(0);
  await page.mouse.up();
  await page.mouse.click(start.x, start.y);
  await expect(dial).toHaveAttribute("aria-valuenow", "25");
  await page.keyboard.press("Escape");
  await expect(panel).toHaveCount(0);
  await expect(page.getByRole("tooltip").filter({ hasText: /^\d+$/ })).toHaveCount(0);
});

test("Comet ring selection wins over an out-of-range number draft without desynchronizing", async ({ page }) => {
  const panel = await openDials(page);
  const dial = panel.getByRole("slider", { name: "Flex-Tune", exact: true });
  const input = panel.getByRole("spinbutton", { name: "Flex-Tune数值" });
  await expect.poll(async () => (await dial.boundingBox())!.width).toBeCloseTo(86, 1);
  const box = (await dial.boundingBox())!;
  for (const [key, draft, fraction, expected] of [["Home", "120", 0.0001, "0"], ["End", "-20", 0.9999, "100"]] as const) {
    await dial.press(key);
    await input.fill(draft);
    const point = dialPoint(box, fraction);
    await page.mouse.move(point.x, point.y); await page.mouse.down();
    await expect(dial).toHaveAttribute("aria-valuenow", expected);
    await expect(input).toHaveValue(expected);
    await page.mouse.up();
    await expect(page.getByRole("tooltip")).toHaveCount(0);
    await expect(dial).toHaveAttribute("aria-valuenow", expected);
    await expect(input).toHaveValue(expected);
  }
});

test("Comet accent transitions with the palette while parameter values stay unchanged", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("helper-colors-v1", JSON.stringify({ mode: "manual", manualColor: "#ffffff" })));
  const panel = await openDials(page);
  const head = panel.locator(".comet-dial__head").first();
  await expect(head).toHaveCSS("fill", "rgb(255, 255, 255)");
  const frames = await head.evaluate(async el => {
    localStorage.setItem("helper-colors-v1", JSON.stringify({ mode: "manual", manualColor: "#55aaff" }));
    window.dispatchEvent(new StorageEvent("storage", { key: "helper-colors-v1" }));
    const frames: string[] = [];
    const started = performance.now();
    await new Promise<void>(resolve => {
      const sample = () => {
        frames.push(getComputedStyle(el).fill);
        if (performance.now() - started < 700) requestAnimationFrame(sample); else resolve();
      };
      requestAnimationFrame(sample);
    });
    return frames;
  });
  expect(new Set(frames).size).toBeGreaterThan(3);
  for (const name of ["Flex-Tune", "Natural Vibrato", "Humanize"])
    await expect(panel.getByRole("slider", { name, exact: true })).toHaveAttribute("aria-valuenow", "0");
  for (const element of await panel.locator(".comet-dial__head").all())
    await expect(element).toHaveCSS("fill", "rgb(85, 170, 255)");
});

test("Comet reduced motion preserves taps and precision without a moving tail", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  const panel = await openDials(page);
  const dial = panel.getByRole("slider", { name: "Flex-Tune", exact: true });
  await expect.poll(async () => (await dial.boundingBox())!.width).toBeCloseTo(86, 1);
  const point = dialPoint((await dial.boundingBox())!, 0.75);
  await page.mouse.click(point.x, point.y);
  await expect(dial).toHaveAttribute("aria-valuenow", "75");
  await expect(page.getByRole("tooltip")).toHaveCount(0);
  expect(await dial.locator(".comet-dial__comet path").evaluateAll(nodes =>
    nodes.every(node => Number(getComputedStyle(node).opacity) === 0))).toBe(true);
});

for (const name of ["电音强度", "详细参数"]) {
  test(`${name} selection follows the live accent after the pointer leaves`, async ({ page }) => {
    await page.addInitScript(() => {
      localStorage.setItem("helper-colors-v1", JSON.stringify({ mode: "manual", manualColor: "#ffffff" }));
    });
    await prepare(page); await reveal(page);
    const button = page.getByRole("button", { name, exact: true, includeHidden: true });
    await button.click();
    await page.mouse.move(0, 0);
    await expect(button).toHaveAttribute("data-state", "open");
    // Regression: Radix's jade open-state background used to survive here,
    // even though the rest of the app (and the hover state) used our palette.
    await expect(button).toHaveCSS("background-color", "color(srgb 1 1 1 / 0.12)");
    await expect(button).toHaveCSS("color", "rgb(255, 255, 255)");

    const frames = await button.evaluate(async el => {
      localStorage.setItem("helper-colors-v1", JSON.stringify({ mode: "manual", manualColor: "#55aaff" }));
      window.dispatchEvent(new StorageEvent("storage", { key: "helper-colors-v1" }));
      const colors: string[] = [];
      const started = performance.now();
      await new Promise<void>(resolve => {
        const sample = () => {
          colors.push(getComputedStyle(el).backgroundColor);
          if (performance.now() - started < 750) requestAnimationFrame(sample);
          else resolve();
        };
        requestAnimationFrame(sample);
      });
      return colors;
    });
    expect(new Set(frames).size).toBeGreaterThan(3);
    await expect(button).toHaveCSS("background-color", "color(srgb 0.333333 0.666667 1 / 0.12)");
    await expect(button).toHaveCSS("color", "rgb(85, 170, 255)");
    await expect(button).toHaveAttribute("data-state", "open");
    await page.keyboard.press("Escape");
    await expect(button).toHaveAttribute("data-state", "closed");
    await expect(button).toHaveCSS("background-color", "rgba(0, 0, 0, 0)");
  });
}

test("card hover replaces only the footer and leaving restores source and live status", async ({ page }) => {
  await prepare(page);
  await expect(page.locator(".footer-information")).toHaveCSS("opacity", "1");
  const before = await page.locator(".timeline").boundingBox();
  await reveal(page);
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
  await expect(page.locator(".footer-information")).toHaveCSS("opacity", "0");
  for (const name of ["电音强度", "详细参数", "开启去人声", "上一首", "暂停", "下一首"])
    await expect(page.getByRole("button", { name, exact: true })).toBeVisible();
  expect(await page.locator(".timeline").boundingBox()).toEqual(before);
  await page.screenshot({ path: "artifacts/player-controls-hover.png" });
  await page.mouse.move(0, 0);
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "0");
  await expect(page.locator(".footer-information")).toHaveCSS("opacity", "1");
  await expect(page.locator(".source")).toHaveText("Folia");
  await expect(page.getByText("正在播放", { exact: true })).toBeVisible();
});

test("single-row strength popover places endpoints beside the slider and preserves keyboard changes", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  const panel = page.getByRole("dialog", { name: "电音强度", exact: true });
  await expect(panel).toBeVisible();
  await expect(panel.getByText("自然", { exact: true })).toBeVisible();
  await expect(panel.getByText("明显", { exact: true })).toBeVisible();
  await expect(panel.getByRole("spinbutton")).toHaveCount(0);
  await expect(panel.getByRole("button")).toHaveCount(0);
  await expect(panel.getByText("电音强度", { exact: true })).toHaveCount(0);
  const slider = panel.getByRole("slider", { name: "电音强度", exact: true });
  await slider.focus(); await slider.press("End");
  await expect(slider).toHaveAttribute("aria-valuenow", "100");
  await slider.press("Home");
  for (let i = 0; i < 7; i++) await slider.press("PageUp");
  for (let i = 0; i < 3; i++) await slider.press("ArrowRight");
  await expect(slider).toHaveAttribute("aria-valuenow", "73");
  await page.mouse.move(0, 0);
  await expect(panel).toBeVisible();
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
  const bounds = (await panel.boundingBox())!;
  expect(bounds.width).toBeLessThanOrEqual(240);
  expect(bounds.height).toBeLessThanOrEqual(54);
  const left = (await panel.getByText("自然", { exact: true }).boundingBox())!;
  const right = (await panel.getByText("明显", { exact: true }).boundingBox())!;
  const track = (await panel.locator(".elastic-slider__root").boundingBox())!;
  expect(left.x + left.width + 4).toBeLessThanOrEqual(track.x);
  expect(track.x + track.width + 4).toBeLessThanOrEqual(right.x);
  for (const label of [left, right])
    expect(Math.abs(label.y + label.height / 2 - (track.y + track.height / 2))).toBeLessThan(1);
  expect(bounds.x).toBeGreaterThanOrEqual(0); expect(bounds.y).toBeGreaterThanOrEqual(0);
  expect(bounds.x + bounds.width).toBeLessThanOrEqual(468);
  expect(bounds.y + bounds.height).toBeLessThanOrEqual(242);
  await page.screenshot({ path: "artifacts/player-controls-strength.png" });
  await page.keyboard.press("Escape"); await expect(panel).toHaveCount(0);
  await expect(page.getByRole("button", { name: "电音强度", exact: true })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(slider).toHaveAttribute("aria-valuenow", "73");
});

test("details contains only the three requested parameters and safely bounds numeric input", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  const panel = page.getByRole("dialog", { name: "Auto-Tune 参数" });
  await expect(panel).toBeVisible();
  await expect(panel.getByRole("slider")).toHaveCount(3);
  await expect(panel.getByText(/Key|Scale|调性|调式/)).toHaveCount(0);
  for (const [name, value, wanted] of [["Flex-Tune", "120", "100"], ["Natural Vibrato", "-12.7", "-12"], ["Humanize", "41", "41"]]) {
    const input = panel.getByRole("spinbutton", { name: `${name}数值` });
    await input.fill(value); await input.press("Tab");
    await expect(input).toHaveValue(wanted);
    await expect(panel.getByRole("slider", { name, exact: true })).toHaveAttribute("aria-valuenow", wanted);
  }
  await panel.getByRole("spinbutton", { name: "Humanize数值" }).fill("");
  await panel.getByRole("spinbutton", { name: "Humanize数值" }).press("Tab");
  await expect(panel.getByRole("spinbutton", { name: "Humanize数值" })).toHaveValue("41");
  const bounds = (await panel.boundingBox())!;
  await page.screenshot({ path: "artifacts/player-controls-details.png" });
  expect(bounds.y).toBeGreaterThanOrEqual(0);
  expect(bounds.y + bounds.height).toBeLessThanOrEqual(242);
  const dimensions = await panel.evaluate(el => ({
    scrollHeight: el.scrollHeight, clientHeight: el.clientHeight,
    children: Array.from(el.children).map(child => ({ className: child.className, height: child.getBoundingClientRect().height,
      marginTop: getComputedStyle(child).marginTop, marginBottom: getComputedStyle(child).marginBottom })),
  }));
  expect(dimensions.scrollHeight, JSON.stringify(dimensions)).toBeLessThanOrEqual(dimensions.clientHeight);
  await page.keyboard.press("Escape");
  await expect(panel).toHaveCount(0);
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  await expect(panel.getByRole("spinbutton", { name: "Humanize数值" })).toHaveValue("41");
});

test("vocal removal toggle communicates both states without enabling Auto-Tune effects", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("helper-colors-v1", JSON.stringify({ mode: "manual", manualColor: "#55aaff" })));
  await prepare(page); await reveal(page);
  const button = page.getByRole("button", { name: /^(开启|关闭)去人声$/ });
  await expect(button).toBeVisible();
  await expect(button).toHaveAccessibleName("开启去人声");
  await expect(button).toHaveAttribute("aria-pressed", "false");
  await expect(button).toHaveCSS("color", "rgb(154, 159, 172)");
  const regularIcon = await button.locator("svg").innerHTML();
  await expect(button.locator("svg")).toHaveAttribute("width", "17");
  await button.hover();
  await expect(page.getByRole("tooltip")).toContainText("开启去人声");
  await button.click();
  await expect(button).toHaveAccessibleName("关闭去人声");
  await expect(button).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByText("去人声中 · 延迟约 45 ms", { exact: true })).toBeVisible();
  await reveal(page);
  await expect(button).toHaveCSS("color", "rgb(85, 170, 255)");
  expect(await button.locator("svg").innerHTML()).not.toBe(regularIcon);
  await button.hover();
  const tip = page.getByRole("tooltip");
  await expect(tip).toContainText("关闭去人声，恢复原唱");
  await expect(tip.locator(".warm-tooltip__arrow")).toHaveCount(0);
  await expect(tip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
  await page.screenshot({ path: "artifacts/vocal-removal-enabled.png" });
  await button.press("Space"); await reveal(page);
  await expect(button).toHaveAccessibleName("开启去人声");
  await expect(button).toHaveAttribute("aria-pressed", "false");
  await expect(button).toHaveCSS("color", "rgb(154, 159, 172)");
  expect(await button.locator("svg").innerHTML()).toBe(regularIcon);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  await expect(page.getByRole("slider", { name: "电音强度", exact: true })).toHaveAttribute("aria-valuenow", "50");
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  for (const name of ["Flex-Tune", "Natural Vibrato", "Humanize"])
    await expect(page.getByRole("slider", { name, exact: true })).toHaveAttribute("aria-valuenow", "0");
  expect(await page.evaluate(() => (window as any).controlMedia.track.playbackStatus)).toBe("playing");
  const commands = await page.evaluate(() => (window as any).commands as string[]);
  expect(commands.every(command => ["get_audio_level", "get_key_detection", "autotune_command", "plugin:window|is_always_on_top", "get_devocal_status", "devocal_command"].includes(command))).toBe(true);
  expect(await page.evaluate(() => (window as any).effectRequests.every((r: any) => r.op === "status"))).toBe(true);
});

test("vocal removal toggle remains separate from live transport controls", async ({ page }) => {
  await prepare(page); await reveal(page);
  const errors: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.getByRole("button", { name: "开启去人声", exact: true }).click();
  await expect(page.getByRole("button", { name: "关闭去人声", exact: true })).toHaveAttribute("aria-pressed", "true");
  await page.getByRole("button", { name: "关闭去人声", exact: true }).click();
  await expect(page.getByRole("button", { name: "开启去人声", exact: true })).toHaveAttribute("aria-pressed", "false");
  expect(await page.evaluate(() => (window as any).commands.includes("control_media"))).toBe(false);
  await page.getByRole("button", { name: "暂停", exact: true }).click();
  await expect(page.getByRole("button", { name: "播放", exact: true })).toBeVisible();
  for (const name of ["上一首", "下一首"]) {
    await page.getByRole("button", { name, exact: true }).click();
    await expect(page.getByRole("heading", { name: "控制区预览", exact: true })).toBeVisible();
  }
  await page.mouse.move(0, 0);
  await expect(page.getByText("已暂停", { exact: true })).toBeVisible();
  expect(await page.evaluate(() => (window as any).controlMedia.track.playbackStatus)).toBe("paused");
  const commands = await page.evaluate(() => (window as any).commands as string[]);
  expect(commands.filter(command => command === "control_media")).toHaveLength(3);
  expect(commands.every(command => ["get_audio_level", "get_key_detection", "autotune_command", "plugin:window|is_always_on_top", "get_devocal_status", "devocal_command", "control_media"].includes(command))).toBe(true);
  expect(await page.evaluate(() => (window as any).effectRequests.every((r: any) => r.op === "status"))).toBe(true);
  expect(errors).toEqual([]);
});

test("keyboard access reveals hidden controls and reduced motion retains usable panels", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await prepare(page);
  const strength = page.getByRole("button", { name: "电音强度", exact: true });
  for (let i = 0; i < 12; i++) {
    await page.keyboard.press("Tab");
    if (await strength.evaluate(el => el === document.activeElement)) break;
  }
  await expect(strength).toBeFocused();
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
  await page.keyboard.press("Enter");
  const panel = page.getByRole("dialog", { name: "电音强度", exact: true });
  await expect(panel).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(strength).toBeFocused();
});

test("without a media session transport is disabled but effect previews remain available", async ({ page }) => {
  await prepare(page, false); await reveal(page);
  for (const name of ["上一首", "播放", "下一首"])
    await expect(page.getByRole("button", { name, exact: true })).toBeDisabled();
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  await expect(page.getByRole("dialog", { name: "Auto-Tune 参数" })).toBeVisible();
});

test("clicking outside strength closes the popover without changing its value", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  const panel = page.getByRole("dialog", { name: "电音强度", exact: true });
  await panel.getByRole("slider").press("Home");
  await page.mouse.click(452, 45);
  await expect(panel).toHaveCount(0);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  await expect(panel.getByRole("slider")).toHaveAttribute("aria-valuenow", "0");
});

test("real metadata changes refresh the transport state while controls remain visible", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "暂停", exact: true }).click();
  await expect(page.getByRole("button", { name: "播放", exact: true })).toBeVisible();
  await page.evaluate(() => {
    const state = window as any;
    state.controlMedia = { ...state.controlMedia, track: { ...state.controlMedia.track, title: "真实会话新歌", playbackStatus: "playing" } };
    window.dispatchEvent(new CustomEvent("test:media", { detail: state.controlMedia }));
  });
  await expect(page.getByRole("button", { name: "暂停", exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: "真实会话新歌", exact: true })).toBeVisible();
});

test.describe("touch input", () => {
  test.use({ hasTouch: true, isMobile: true });
  test("parameter names and centered numbers retain native touch editing", async ({ page }) => {
    const panel = await openDials(page);
    const input = panel.getByRole("spinbutton", { name: "Natural Vibrato数值" });
    await panel.getByText("Natural Vibrato", { exact: true }).tap();
    await expect(input).toBeFocused();
    await input.press("Enter");
    await input.tap();
    await expect(input).toBeFocused();
    await input.pressSequentially("-0.3"); await input.press("Enter");
    await expect(input).toHaveValue("-0.3");
    await expect(panel.getByRole("slider", { name: "Natural Vibrato", exact: true })).toHaveAttribute("aria-valuenow", "-0.3");
  });
  test("controls remain available without hover", async ({ page }) => {
    await prepare(page);
    await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
    await page.getByRole("button", { name: "详细参数", exact: true }).tap();
    const panel = page.getByRole("dialog", { name: "Auto-Tune 参数" });
    await expect(panel.getByRole("slider")).toHaveCount(3);
    const dial = panel.getByRole("slider", { name: "Flex-Tune", exact: true });
    await expect.poll(async () => (await dial.boundingBox())!.width).toBeCloseTo(86, 1);
    const tap = dialPoint((await dial.boundingBox())!, 0.75);
    await page.touchscreen.tap(tap.x, tap.y);
    await expect(dial).toHaveAttribute("aria-valuenow", "75");
    await expect(page.getByRole("tooltip")).toHaveCount(0);
    await page.getByRole("button", { name: "详细参数", exact: true }).tap();
    await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
    await page.getByRole("button", { name: "电音强度", exact: true }).tap();
    const strength = page.getByRole("dialog", { name: "电音强度", exact: true });
    await expect(strength).toBeVisible();
    const touchTrack = strength.locator(".elastic-slider__root");
    const touchBounds = (await touchTrack.boundingBox())!;
    await touchTrack.tap({ position: { x: touchBounds.width * 0.75, y: touchBounds.height / 2 } });
    const touchedValue = Number(await strength.getByRole("slider").getAttribute("aria-valuenow"));
    expect(touchedValue).toBeGreaterThanOrEqual(70);
    expect(touchedValue).toBeLessThanOrEqual(80);
    await page.getByRole("button", { name: "电音强度", exact: true }).tap();
    await expect(strength).toHaveCount(0);
    await page.getByRole("button", { name: "开启去人声", exact: true }).tap();
    await expect(page.getByRole("button", { name: "关闭去人声", exact: true })).toHaveAttribute("aria-pressed", "true");
    await page.getByRole("button", { name: "暂停", exact: true }).tap();
    await expect(page.getByRole("button", { name: "播放", exact: true })).toBeVisible();
  });

  test("a touch tap reveals controls on hybrid devices that also support hover", async ({ page }) => {
    await page.addInitScript(() => {
      const original = window.matchMedia.bind(window);
      window.matchMedia = query => query === "(hover: none)" ? original("(min-width: 10000px)") : original(query);
    });
    await prepare(page);
    await expect(page.locator(".footer-information")).toHaveCSS("opacity", "1");
    await page.locator(".music-window").tap({ position: { x: 18, y: 45 } });
    await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
    await page.getByRole("button", { name: "详细参数", exact: true }).tap();
    await expect(page.getByRole("dialog", { name: "Auto-Tune 参数" })).toBeVisible();
  });
});

test("vibrato numeric input accepts negative decimals typed one character at a time", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  const input = page.getByRole("spinbutton", { name: "Natural Vibrato数值" });
  await input.fill(""); await input.pressSequentially("-1.5");
  await input.press("Tab");
  await expect(input).toHaveValue("-1.5");
  await expect(page.getByRole("slider", { name: "Natural Vibrato", exact: true })).toHaveAttribute("aria-valuenow", "-1.5");
});

test("closing the parameter popover and immediately leaving restores the idle footer", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  await page.getByRole("button", { name: "详细参数", exact: true }).click();
  await page.mouse.move(0, 0);
  await expect(page.locator(".footer-information")).toHaveCSS("opacity", "1");
});

test("switching from mouse to keyboard keeps the active control visible after pointer leave", async ({ page }) => {
  await prepare(page); await reveal(page);
  const vocalRemoval = page.getByRole("button", { name: /^(开启|关闭)去人声$/ });
  await vocalRemoval.click();
  await vocalRemoval.press("Space");
  await page.mouse.move(0, 0);
  await expect(vocalRemoval).toBeFocused();
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
});

test("elastic strength background stretches with its slider and contains undistorted side labels", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  const panel = page.getByRole("dialog", { name: "电音强度", exact: true });
  const root = panel.locator(".elastic-slider__root");
  const track = panel.locator(".elastic-slider__track-wrapper");
  const slider = panel.getByRole("slider", { name: "电音强度", exact: true });
  const surface = panel.locator(".elastic-slider__surface");
  await expect(surface).toBeVisible();
  // Measure rest only after Radix's opening scale has finished.
  await expect.poll(() => surface.evaluate(el => el.getBoundingClientRect().width / (el as HTMLElement).offsetWidth)).toBeCloseTo(1, 4);
  const restingSurface = (await surface.boundingBox())!;
  await root.hover();
  // Compare against the settled hover shape, not an intermediate entrance frame.
  await expect.poll(() => panel.locator(".elastic-slider__wrapper").evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeCloseTo(1.12, 3);
  const bounds = (await root.boundingBox())!;
  const popup = (await panel.boundingBox())!;
  const hoverSurface = (await surface.boundingBox())!;
  await page.mouse.move(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2);
  await page.mouse.down();
  await page.mouse.move(466, bounds.y + bounds.height / 2, { steps: 8 });
  await expect(slider).toHaveAttribute("aria-valuenow", "100");
  await expect.poll(() => track.evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeGreaterThan(1.015);
  for (const side of ["right", "left"]) {
    if (side === "left") {
      await page.mouse.move(2, bounds.y + bounds.height / 2, { steps: 8 });
      await expect(slider).toHaveAttribute("aria-valuenow", "0");
    }
    const stretched = (await track.boundingBox())!;
    if (side === "right") expect(stretched.x + stretched.width).toBeGreaterThan(popup.x + popup.width + 1);
    else expect(stretched.x).toBeLessThan(popup.x - 1);
    expect(await panel.boundingBox()).toEqual(popup);
    const left = (await panel.getByText("自然", { exact: true }).boundingBox())!;
    const right = (await panel.getByText("明显", { exact: true }).boundingBox())!;
    const backdrop = (await surface.boundingBox())!;
    expect(backdrop.width).toBeGreaterThan(hoverSurface.width + 20);
    expect(backdrop.height).toBeLessThan(hoverSurface.height);
    expect(backdrop.x).toBeLessThan(left.x - 4);
    expect(backdrop.x + backdrop.width).toBeGreaterThan(right.x + right.width + 4);
    expect(backdrop.x).toBeGreaterThanOrEqual(0);
    expect(backdrop.x + backdrop.width).toBeLessThanOrEqual(468);
    const labelScale = await panel.getByText("自然", { exact: true }).evaluate(el => {
      const m = new DOMMatrix(getComputedStyle(el).transform);
      return { x: m.a, y: m.d };
    });
    expect(labelScale.x).toBeCloseTo(labelScale.y, 4);
    expect(left.x + left.width + 4).toBeLessThanOrEqual(stretched.x);
    expect(stretched.x + stretched.width + 4).toBeLessThanOrEqual(right.x);
    expect(left.x).toBeGreaterThanOrEqual(0);
    expect(right.x + right.width).toBeLessThanOrEqual(468);
    for (const text of ["自然", "明显"]) {
      expect(await panel.getByText(text, { exact: true }).evaluate(el => {
        const r = el.getBoundingClientRect();
        return el.contains(document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2));
      }), `${text} must remain visible outside the background`).toBe(true);
    }
    await page.screenshot({ path: `artifacts/elastic-strength-${side}.png` });
  }
  await page.mouse.up();
  await expect.poll(() => track.evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeCloseTo(1, 3);
  await page.mouse.move(0, 0);
  await expect.poll(async () => (await surface.boundingBox())!.width).toBeCloseTo(restingSurface.width, 1);
  await expect(slider).toHaveAttribute("aria-valuenow", "0");
  await page.keyboard.press("Escape"); await reveal(page);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  await expect(slider).toHaveAttribute("aria-valuenow", "0");
});

test("reduced motion still shows live strength without background or slider deformation", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  const panel = page.getByRole("dialog", { name: "电音强度", exact: true });
  const root = panel.locator(".elastic-slider__root");
  const bounds = (await root.boundingBox())!;
  await page.mouse.move(bounds.x + 4, bounds.y + bounds.height / 2);
  await page.mouse.down();
  await page.mouse.move(bounds.x + bounds.width + 50, bounds.y + bounds.height / 2);
  await expect(panel.getByRole("slider")).toHaveAttribute("aria-valuenow", "100");
  for (const selector of [".elastic-slider__wrapper", ".elastic-slider__track-wrapper", ".elastic-slider__surface"])
    expect(await panel.locator(selector).evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBe(1);
  await expect(page.getByRole("tooltip")).toHaveText("100%");
  await expect(page.locator(".warm-tooltip__box")).toHaveCSS("transform", "none");
  await page.mouse.up();
});

test("dragging strength keeps one live Warm Tooltip at the visual value and dismisses after release", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  const panel = page.getByRole("dialog", { name: "电音强度", exact: true });
  const root = panel.locator(".elastic-slider__root");
  const slider = panel.getByRole("slider");
  const tip = page.getByRole("tooltip");
  await root.hover();
  await expect(tip).toHaveCount(0);
  const bounds = (await root.boundingBox())!;
  await page.mouse.move(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2);
  await page.mouse.down();
  await expect(tip).toBeVisible();
  await expect(tip.locator(".warm-tooltip__arrow")).toHaveCount(0);
  await expect(tip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
  const id = await tip.getAttribute("id");
  for (const fraction of [0.25, 0.75, 1.5, -0.8]) {
    const current = (await root.boundingBox())!;
    await page.mouse.move(current.x + current.width * fraction, current.y + current.height / 2, { steps: 5 });
    const value = Number(await slider.getAttribute("aria-valuenow"));
    expect(value).toBeGreaterThanOrEqual(Math.max(0, Math.min(100, fraction * 100)) - 1);
    expect(value).toBeLessThanOrEqual(Math.max(0, Math.min(100, fraction * 100)) + 1);
    await expect(tip).toHaveText(`${value}%`);
    await expect(tip).toHaveAttribute("id", id!);
    await expect.poll(async () => {
      const hint = (await tip.boundingBox())!;
      const fill = (await panel.locator(".elastic-slider__range").boundingBox())!;
      return Math.abs(hint.x + hint.width / 2 - (fill.x + fill.width));
    }).toBeLessThan(3);
  }
  await page.screenshot({ path: "artifacts/elastic-strength-live-value.png" });
  await page.mouse.up();
  await expect(tip).toHaveText("0%");
  await expect(tip).toHaveCount(0, { timeout: 2000 });
  await expect(panel).toBeVisible();
});

test("strength value tooltip supports keyboard and cannot remain after closing or reopening", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  const slider = page.getByRole("slider", { name: "电音强度", exact: true });
  await slider.press("Home");
  await slider.press("PageUp");
  await slider.press("ArrowRight");
  await expect(page.getByRole("tooltip")).toHaveText("11%");
  await expect(slider).toHaveAttribute("aria-valuenow", "11");
  await page.keyboard.press("Escape");
  // Focus returns to the lightning button, whose normal descriptive hint is valid.
  await expect(page.getByRole("tooltip").filter({ hasText: /^\d+%$/ })).toHaveCount(0);
  await page.keyboard.press("Enter");
  await expect(slider).toHaveAttribute("aria-valuenow", "11");
  await expect(page.getByRole("tooltip")).toHaveCount(0);
});

test("a freshly mounted endpoint stretches without a value change or parent timer rerender", async ({ page }) => {
  // Mount the real component in a controlled consumer without App's clock.
  // That clock must not be required to initialize Motion subscriptions.
  await page.route("**/src/main.tsx", async route => {
    const source = await (await route.fetch()).text();
    const imports = source.slice(0, source.indexOf("const settingsView"));
    await route.fulfill({ contentType: "application/javascript", body: imports + `
      import ElasticSlider from '/src/components/react-bits/ElasticSlider.tsx';
      function EndpointConsumer() {
        const [value, setValue] = React.useState(100);
        return React.createElement('div', { style: { width: 210, padding: 12 } },
          React.createElement(ElasticSlider, { value, onChange: setValue, ariaLabel: '电音强度' }));
      }
      ReactDOM.createRoot(document.getElementById('root')).render(React.createElement(EndpointConsumer));
    ` });
  });
  await page.goto("/");
  const root = page.locator(".elastic-slider__root");
  await root.hover();
  await expect.poll(() => page.locator(".elastic-slider__wrapper").evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeGreaterThan(1.02);
  const bounds = (await root.boundingBox())!;
  await page.mouse.move(bounds.x + bounds.width - 0.1, bounds.y + bounds.height / 2);
  await page.mouse.down();
  await page.mouse.move(bounds.x + bounds.width + 60, bounds.y + bounds.height / 2);
  await expect(page.getByRole("slider")).toHaveAttribute("aria-valuenow", "100");
  await expect.poll(() => page.locator(".elastic-slider__track-wrapper").evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeGreaterThan(1.015);
  await page.mouse.up();
});

test("losing pointer capture releases the elastic stretch and a later drag still works", async ({ page }) => {
  await prepare(page); await reveal(page);
  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  const root = page.locator(".elastic-slider__root");
  const track = page.locator(".elastic-slider__track-wrapper");
  await root.hover();
  await root.evaluate(el => el.addEventListener("pointerdown", event => {
    (window as any).controlPointerId = (event as PointerEvent).pointerId;
    (window as any).controlCaptureTarget = event.target;
  }, { once: true }));
  const bounds = (await root.boundingBox())!;
  await page.mouse.move(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2);
  await page.mouse.down();
  await page.mouse.move(bounds.x + bounds.width + 60, bounds.y + bounds.height / 2);
  await expect.poll(() => track.evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeGreaterThan(1.015);
  await page.evaluate(() => {
    const pointer = (window as any).controlPointerId;
    const target = (window as any).controlCaptureTarget as Element;
    if (!target.hasPointerCapture(pointer)) throw new Error("Expected a captured real pointer");
    target.releasePointerCapture(pointer);
  });
  await page.mouse.move(bounds.x + bounds.width + 61, bounds.y + bounds.height / 2);
  await expect.poll(() => track.evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeCloseTo(1, 3);
  await page.mouse.up();
  await expect(page.getByRole("tooltip")).toHaveCount(0);
  await page.mouse.move(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2);
  await page.mouse.down();
  await page.mouse.move(bounds.x - 60, bounds.y + bounds.height / 2);
  await expect(page.getByRole("slider", { name: "电音强度", exact: true })).toHaveAttribute("aria-valuenow", "0");
  await expect.poll(() => track.evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeGreaterThan(1.015);
  await page.mouse.up();
});
