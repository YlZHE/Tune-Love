import { expect, test, type Page } from "@playwright/test";

test.use({ viewport: { width: 468, height: 242 } });

type SongName = "first" | "second";

async function installNativeBoundary(page: Page, initialStatus: "idle" | "detected" = "idle",
  deferInitial = false) {
  await page.addInitScript(({ initialStatus, deferInitial }) => {
    const song = (name: SongName, playbackStatus: "playing" | "paused" = "paused") => ({
      title: name === "first" ? "晨光" : "夜航",
      artist: "边界测试歌手",
      album: "边界测试专辑",
      artworkDataUrl: null,
      positionSeconds: 30,
      durationSeconds: 180,
      playbackStatus,
      source: "测试播放器",
      sourceId: "player.exe",
      sourceIconDataUrl: null,
      updatedAtMs: Date.now(),
      playbackRate: 1,
    });
    const trackKey = (track: ReturnType<typeof song>) => JSON.stringify([
      track.sourceId, track.title, track.artist, track.album,
    ]);
    const track = song("first");
    const state: any = {
      media: { status: "ready", track, capturedAtMs: Date.now(), targetGeneration: 4 },
      key: { sourceId: track.sourceId, trackKey: trackKey(track), targetGeneration: 4,
        status: initialStatus, key: initialStatus === "detected" ? { pitchClass: 9, mode: "minor" } : null,
        updatedAtMs: 100 },
      calls: {}, deferreds: [], deferNext: deferInitial, keyInFlight: 0, maxKeyInFlight: 0, hidden: false,
      song,
      trackKey,
    };
    Object.defineProperty(window, "isTauri", { value: true, configurable: true });
    Object.defineProperty(document, "hidden", { configurable: true, get: () => state.hidden });
    (window as any).__keyFixture = state;
    (window as any).__TAURI_INTERNALS__ = {
      metadata: { currentWindow: { label: "main" }, currentWebview: { label: "main" } },
      invoke: async (command: string) => {
        state.calls[command] = (state.calls[command] ?? 0) + 1;
        if (command === "get_now_playing") return structuredClone({
          ...state.media, capturedAtMs: Date.now(),
          track: state.media.track ? { ...state.media.track, updatedAtMs: Date.now() } : null,
        });
        if (command === "get_key_detection") {
          state.keyInFlight++;
          state.maxKeyInFlight = Math.max(state.maxKeyInFlight, state.keyInFlight);
          const snapshot = structuredClone(state.key);
          if (state.deferNext) {
            state.deferNext = false;
            return new Promise(resolve => { state.deferreds.push(() => {
              state.keyInFlight--; resolve(snapshot);
            }); });
          }
          state.keyInFlight--;
          return snapshot;
        }
        if (command === "get_audio_level") return {
          sourceId: state.media.track?.sourceId ?? null,
          trackKey: state.media.track ? state.trackKey(state.media.track) : null,
          status: "idle", bands: [0, 0, 0], rms: 0, peak: 0, updatedAtMs: Date.now(),
        };
        if (command === "plugin:window|is_always_on_top") return true;
        if (command === "plugin:window|is_visible") return true;
        return null;
      },
    };
  }, { initialStatus, deferInitial });
  await page.goto("/");
}

async function changeSong(page: Page, name: SongName, generation: number,
  playbackStatus: "playing" | "paused" = "paused") {
  await page.evaluate(({ name, generation, playbackStatus }) => {
    const state = (window as any).__keyFixture;
    const track = state.song(name, playbackStatus);
    state.media = { status: "ready", track, capturedAtMs: Date.now(), targetGeneration: generation };
  }, { name, generation, playbackStatus });
}

async function setKey(page: Page, pitchClass: number, mode: "major" | "minor", generation: number) {
  await page.evaluate(({ pitchClass, mode, generation }) => {
    const state = (window as any).__keyFixture;
    state.key = { sourceId: state.media.track.sourceId, trackKey: state.trackKey(state.media.track),
      targetGeneration: generation, status: "detected", key: { pitchClass, mode }, updatedAtMs: 100 };
  }, { pitchClass, mode, generation });
}

async function sampleTitleTransition(page: Page, pitchClass: number, mode: "major" | "minor", generation: number) {
  await page.evaluate(() => { (window as any).__keyFixture.deferNext = true; });
  await setKey(page, pitchClass, mode, generation);
  await expect.poll(() => page.evaluate(() => (window as any).__keyFixture.deferreds.length)).toBe(1);
  return page.evaluate(async () => {
    const state = (window as any).__keyFixture;
    const frames: Array<{ stage: DOMRect; actions: DOMRect; titlebar: DOMRect; overflow: string;
      nodes: Array<{ text: string; bounds: DOMRect; hidden: string | null; inert: boolean }> }> = [];
    const start = performance.now();
    state.deferreds.shift()();
    await new Promise<void>(resolve => {
      const frame = () => {
        const stage = document.querySelector(".key-title-stage")!;
        frames.push({
          stage: stage.getBoundingClientRect().toJSON(),
          actions: document.querySelector(".window-actions")!.getBoundingClientRect().toJSON(),
          titlebar: document.querySelector(".titlebar")!.getBoundingClientRect().toJSON(),
          overflow: getComputedStyle(stage).overflow,
          nodes: [...document.querySelectorAll(".key-title-text")].map(node => ({
            text: node.textContent ?? "", bounds: node.getBoundingClientRect().toJSON(),
            hidden: node.getAttribute("aria-hidden"), inert: node.hasAttribute("inert"),
          })),
        });
        if (performance.now() - start < 650) requestAnimationFrame(frame); else resolve();
      };
      requestAnimationFrame(frame);
    });
    return frames;
  });
}

function expectSafeTitleTimeline(frames: Awaited<ReturnType<typeof sampleTitleTransition>>) {
  expect(frames.length).toBeGreaterThan(10);
  expect(frames.every(frame => frame.overflow === "hidden" && frame.titlebar.height === 41)).toBe(true);
  expect(frames.some(frame => frame.nodes.length === 2)).toBe(true);
  expect(frames.some(frame => frame.nodes.some(node => node.bounds.top < frame.stage.top - 0.5
    || node.bounds.bottom > frame.stage.bottom + 0.5))).toBe(true);
  expect(frames.every(frame => frame.nodes.every(node => node.bounds.left >= frame.stage.left - 0.5
    && node.bounds.right <= frame.actions.left + 0.5))).toBe(true);
  const final = frames.at(-1)!;
  expect(final.nodes.filter(node => node.hidden !== "true" && !node.inert)).toHaveLength(1);
}

test("shows the brand first, then a validated A minor label without disturbing native titlebar layout", async ({ page }) => {
  await installNativeBoundary(page);
  const brand = page.locator(".brand");
  await expect(brand).toContainText("AutoTune Helper");
  await expect(brand).toHaveAttribute("data-tauri-drag-region", "true");
  await expect(page.getByRole("status")).toHaveCount(0);

  await setKey(page, 9, "minor", 4);
  await expect(brand).toContainText("A 小调", { timeout: 2500 });
  await expect(page.locator(".key-title-text.is-detected")).toHaveCSS("font-size", "14px");
  await expect(page.locator(".key-title-text.is-detected")).toHaveCSS("font-weight", "650");
  for (const name of ["取消置顶", "设置", "关闭"]) await expect(page.getByRole("button", { name })).toBeVisible();
  const geometry = await page.evaluate(() => {
    const windowNode = document.querySelector(".music-window") as HTMLElement;
    const titlebar = document.querySelector(".titlebar")!.getBoundingClientRect();
    const label = document.querySelector(".key-title-text.is-detected")!.getBoundingClientRect();
    const actions = document.querySelector(".window-actions")!.getBoundingClientRect();
    return { overflow: windowNode.scrollWidth > windowNode.clientWidth || windowNode.scrollHeight > windowNode.clientHeight,
      titlebarHeight: titlebar.height, inside: label.left >= titlebar.left && label.right <= actions.left
        && label.top >= titlebar.top && label.bottom <= titlebar.bottom };
  });
  expect(geometry).toEqual({ overflow: false, titlebarHeight: 41, inside: true });
});

test("rejects a late old-song result, accepts the new generation, and retains it while paused", async ({ page }) => {
  await installNativeBoundary(page, "detected");
  await expect(page.locator(".brand")).toContainText("A 小调");
  await page.evaluate(() => { (window as any).__keyFixture.deferNext = true; });
  await expect.poll(() => page.evaluate(() => (window as any).__keyFixture.deferreds.length)).toBe(1);

  await changeSong(page, "second", 5, "playing");
  await expect(page.getByRole("heading", { name: "夜航" })).toBeVisible({ timeout: 1800 });
  await expect(page.locator(".brand")).toContainText("AutoTune Helper");
  await page.evaluate(() => {
    const state = (window as any).__keyFixture;
    state.deferreds.shift()();
  });
  await page.waitForTimeout(100);
  await expect(page.locator(".brand")).not.toContainText("A 小调");

  await setKey(page, 1, "major", 5);
  await expect(page.locator(".brand")).toContainText("C♯ 大调", { timeout: 2500 });
  await changeSong(page, "second", 5, "paused");
  await expect(page.getByText("已暂停", { exact: true })).toBeVisible({ timeout: 1800 });
  await page.waitForTimeout(1200);
  await expect(page.locator(".brand")).toContainText("C♯ 大调");
});

test("identical polling results keep the same label node and do not restart title motion", async ({ page }) => {
  await installNativeBoundary(page, "detected");
  const label = page.locator(".key-title-text.is-detected");
  await expect(label).toHaveText("A 小调");
  await page.evaluate(() => { (window as any).__steadyKeyNode = document.querySelector(".key-title-text.is-detected"); });
  const beforeCalls = await page.evaluate(() => (window as any).__keyFixture.calls.get_key_detection);
  await expect.poll(() => page.evaluate(() => (window as any).__keyFixture.calls.get_key_detection),
    { timeout: 2500 }).toBeGreaterThan(beforeCalls);
  expect(await page.evaluate(() => (window as any).__steadyKeyNode === document.querySelector(".key-title-text.is-detected"))).toBe(true);
  const positions = await page.evaluate(async () => {
    const values: number[] = [];
    const start = performance.now();
    await new Promise<void>(resolve => {
      const frame = () => {
        values.push(document.querySelector(".key-title-text.is-detected")!.getBoundingClientRect().y);
        if (performance.now() - start < 350) requestAnimationFrame(frame); else resolve();
      };
      requestAnimationFrame(frame);
    });
    return values;
  });
  expect(Math.max(...positions) - Math.min(...positions)).toBeLessThan(1);
});

test("reduced motion changes the key with a fade fallback and no vertical travel", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await installNativeBoundary(page, "detected");
  await expect(page.locator(".brand")).toContainText("A 小调");
  await page.evaluate(() => { (window as any).__keyFixture.deferNext = true; });
  await setKey(page, 11, "major", 4);
  await expect.poll(() => page.evaluate(() => (window as any).__keyFixture.deferreds.length)).toBe(1);
  const samples = await page.evaluate(async () => {
    const state = (window as any).__keyFixture;
    const values: number[] = [];
    const start = performance.now();
    state.deferreds.shift()();
    await new Promise<void>(resolve => {
      const frame = () => {
        const label = [...document.querySelectorAll(".key-title-text")].find(node => node.textContent === "B 大调");
        if (label) values.push(label.getBoundingClientRect().y);
        if (performance.now() - start < 300) requestAnimationFrame(frame); else resolve();
      };
      requestAnimationFrame(frame);
    });
    return values;
  });
  expect(samples.length).toBeGreaterThan(1);
  expect(Math.max(...samples) - Math.min(...samples)).toBeLessThan(1);
  await expect(page.locator(".brand")).toContainText("B 大调");
});

test("a media identity commit synchronously hides the previous song key before effects run", async ({ page }) => {
  await installNativeBoundary(page, "detected");
  await expect(page.locator(".brand")).toContainText("A 小调");
  const observed = page.evaluate(() => new Promise<string>(resolve => {
    const observer = new MutationObserver(() => {
      if ([...document.querySelectorAll("h1")].some(node => node.textContent === "夜航")) {
        const text = document.querySelector(".brand")?.textContent ?? "";
        observer.disconnect(); resolve(text.replace(/\s+/g, " ").trim());
      }
    });
    observer.observe(document.querySelector("#root")!, { childList: true, subtree: true, characterData: true });
  }));
  await changeSong(page, "second", 5);
  expect(await observed).toContain("AutoTune Helper");
});

test("rapid identity changes keep one native key read in flight across hook lifecycles", async ({ page }) => {
  await installNativeBoundary(page, "detected", true);
  await expect.poll(() => page.evaluate(() => (window as any).__keyFixture.deferreds.length)).toBe(1);
  await changeSong(page, "second", 5);
  await expect(page.getByRole("heading", { name: "夜航" })).toBeVisible({ timeout: 1800 });
  await page.waitForTimeout(150);
  expect(await page.evaluate(() => ({ calls: (window as any).__keyFixture.calls.get_key_detection,
    max: (window as any).__keyFixture.maxKeyInFlight }))).toEqual({ calls: 1, max: 1 });
  await setKey(page, 11, "major", 5);
  await page.evaluate(() => { (window as any).__keyFixture.deferreds.shift()(); });
  await expect(page.locator(".brand")).toContainText("B 大调", { timeout: 2500 });
});

test("a read started before hide cannot become a fresh result after visibility returns", async ({ page }) => {
  await installNativeBoundary(page, "detected", true);
  await expect.poll(() => page.evaluate(() => (window as any).__keyFixture.deferreds.length)).toBe(1);
  await page.evaluate(() => {
    const state = (window as any).__keyFixture;
    state.hidden = true;
    document.dispatchEvent(new Event("visibilitychange"));
    state.hidden = false;
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await setKey(page, 11, "major", 4);
  const seen = await page.evaluate(async () => {
    const state = (window as any).__keyFixture;
    const values: string[] = [];
    const brand = document.querySelector(".brand")!;
    const observer = new MutationObserver(() => values.push((brand.textContent ?? "").replace(/\s+/g, " ").trim()));
    observer.observe(brand, { childList: true, subtree: true, characterData: true, attributes: true });
    state.deferreds.shift()();
    await new Promise(resolve => setTimeout(resolve, 450));
    observer.disconnect();
    return values;
  });
  expect(seen).not.toContain("A 小调");
  await expect(page.locator(".brand")).toContainText("B 大调", { timeout: 2500 });
});

test("brand-to-key and key-to-key motion stays stage-clipped without horizontal action overlap", async ({ page }) => {
  await installNativeBoundary(page);
  expectSafeTitleTimeline(await sampleTitleTransition(page, 9, "minor", 4));
  await expect(page.locator(".key-title-text.is-detected:not([aria-hidden='true']):not([inert])")).toHaveText("A 小调");
  expectSafeTitleTimeline(await sampleTitleTransition(page, 1, "major", 4));
  await expect(page.locator(".key-title-text.is-detected:not([aria-hidden='true']):not([inert])")).toHaveText("C♯ 大调");
});
