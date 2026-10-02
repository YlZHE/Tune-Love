// Exercises only local UI state in the real debug app. Genuine media/audio
// reads pass through; unexpected native commands are blocked and fail the run.
import { chromium, expect } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";

const out = `artifacts/player-controls-native-${Date.now()}`;
await mkdir(out, { recursive: true });
await expect.poll(async () => {
  try { return (await fetch("http://127.0.0.1:19224/json/version", { signal: AbortSignal.timeout(1000) })).ok; }
  catch { return false; }
}, { timeout: 15000, message: "Waiting for the debug WebView to become ready" }).toBe(true);
const browser = await chromium.connectOverCDP("http://127.0.0.1:19224");
const pages = () => browser.contexts().flatMap(context => context.pages());
await expect.poll(() => pages().some(candidate => candidate.url() === "http://tauri.localhost/"),
  { timeout: 15000, message: "Waiting for the main renderer navigation" }).toBe(true);
const page = pages().find(candidate => candidate.url() === "http://tauri.localhost/");
if (!page) throw new Error("Expected the debug helper main window");
const errors = [];
page.on("pageerror", error => errors.push(error.message));
page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
const report = { output: out, errors, passed: false };
const originals = { strength: null, parameters: null, vocalRemovalEnabled: null };
const originalSettings = await page.evaluate(() => ({
  colors: localStorage.getItem("helper-colors-v1"), background: localStorage.getItem("helper-background-v1"),
}));
const details = page.getByRole("dialog", { name: "Auto-Tune 参数", exact: true });
const vocalRemoval = page.locator(".vocal-removal-control");
const strength = page.getByRole("dialog", { name: "电音强度", exact: true });
const strengthSlider = strength.getByRole("slider", { name: "电音强度", exact: true });
const reveal = () => page.locator(".music-window").hover({ position: { x: 18, y: 45 } });
const readMedia = () => page.evaluate(() => window.__TAURI_INTERNALS__.invoke("get_now_playing"));
// WebView2 can acknowledge pointer/React updates before Motion paints them.
const paintedFrame = () => page.evaluate(() => new Promise(resolve =>
  requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));

async function setStrengthValue(value) {
  const target = Number(value);
  if (!Number.isInteger(target) || target < 0 || target > 100) throw new Error("Invalid preview strength");
  await strengthSlider.focus(); await strengthSlider.press("Home");
  for (let i = 0; i < Math.floor(target / 10); i++) await strengthSlider.press("PageUp");
  for (let i = 0; i < target % 10; i++) await strengthSlider.press("ArrowRight");
  await expect(strengthSlider).toHaveAttribute("aria-valuenow", String(target));
}

async function checkPanel(panel, elasticSurface = false) {
  const dimensions = await panel.evaluate(el => ({
    x: el.getBoundingClientRect().x, y: el.getBoundingClientRect().y,
    width: el.getBoundingClientRect().width, height: el.getBoundingClientRect().height,
    scrollHeight: el.scrollHeight, clientHeight: el.clientHeight,
    viewportWidth: innerWidth, viewportHeight: innerHeight,
  }));
  expect(dimensions.x).toBeGreaterThanOrEqual(0);
  expect(dimensions.y).toBeGreaterThanOrEqual(0);
  expect(dimensions.x + dimensions.width).toBeLessThanOrEqual(dimensions.viewportWidth);
  expect(dimensions.y + dimensions.height).toBeLessThanOrEqual(dimensions.viewportHeight);
  if (elasticSurface) {
    // The decorative surface reaches the outer border, so scrollHeight includes
    // that 1 px border even at rest. It is intentionally not a scroll container.
    await expect(panel).toHaveCSS("overflow-y", "visible");
    const content = await panel.locator(".elastic-slider__wrapper").boundingBox();
    const surface = await panel.locator(".elastic-slider__surface").boundingBox();
    expect(content.x).toBeGreaterThanOrEqual(dimensions.x);
    expect(content.y).toBeGreaterThanOrEqual(dimensions.y);
    expect(content.x + content.width).toBeLessThanOrEqual(dimensions.x + dimensions.width);
    expect(content.y + content.height).toBeLessThanOrEqual(dimensions.y + dimensions.height);
    expect(surface.x).toBeCloseTo(dimensions.x, 1);
    expect(surface.y).toBeCloseTo(dimensions.y, 1);
    expect(surface.width).toBeCloseTo(dimensions.width, 1);
    expect(surface.height).toBeCloseTo(dimensions.height, 1);
  } else expect(dimensions.scrollHeight).toBeLessThanOrEqual(dimensions.clientHeight);
  return dimensions;
}

await page.evaluate(() => {
  if (window.__playerControlsVerification) throw new Error("A previous controls verification is still active");
  // Tauri freezes its public invoke/ipc methods. Wrap the two actual Windows
  // transports instead, without replacing media results or logging payloads.
  const webview = window.chrome?.webview;
  if (!webview?.postMessage) throw new Error("Expected the Windows WebView transport");
  const state = { originalFetch: window.fetch, originalPost: webview.postMessage, counts: {}, blocked: [], pointers: [] };
  state.pointerTypes = ["pointerdown", "pointermove", "pointerup", "pointercancel", "lostpointercapture"];
  state.pointerTrace = event => {
    state.pointers.push({ type: event.type, x: event.clientX, y: event.clientY,
      id: event.pointerId, target: event.target?.className ?? "" });
    if (state.pointers.length > 160) state.pointers.shift();
  };
  for (const type of state.pointerTypes) document.addEventListener(type, state.pointerTrace, true);
  const readOnly = new Set(["get_now_playing", "get_audio_level", "get_key_detection", "plugin:window|is_always_on_top",
    "plugin:window|is_visible", "plugin:window|get_all_windows", "plugin:window|inner_size", "plugin:window|scale_factor"]);
  const allowed = command => {
    state.counts[command] = (state.counts[command] ?? 0) + 1;
    if (!readOnly.has(command)) {
      state.blocked.push(command);
      return false;
    }
    return true;
  };
  state.fetchWrapper = function(input, init) {
    const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url, location.href);
    if (url.hostname === "ipc.localhost" || url.protocol === "ipc:") {
      const command = decodeURIComponent(url.pathname.slice(1));
      if (!allowed(command)) return Promise.resolve(new Response(JSON.stringify("Blocked unexpected control command"), {
        headers: { "Content-Type": "application/json", "Tauri-Response": "error" },
      }));
    }
    return state.originalFetch.call(window, input, init);
  };
  state.postWrapper = function(message) {
    const parsed = typeof message === "string" ? JSON.parse(message) : message;
    if (typeof parsed?.cmd === "string" && !allowed(parsed.cmd)) {
      queueMicrotask(() => window.__TAURI_INTERNALS__.runCallback(parsed.error, "Blocked unexpected control command"));
      return;
    }
    return state.originalPost.call(webview, message);
  };
  window.fetch = state.fetchWrapper;
  webview.postMessage = state.postWrapper;
  if (window.fetch !== state.fetchWrapper || webview.postMessage !== state.postWrapper) {
    window.fetch = state.originalFetch; webview.postMessage = state.originalPost;
    for (const type of state.pointerTypes) document.removeEventListener(type, state.pointerTrace, true);
    throw new Error("Cannot install the read-only transport guard");
  }
  window.__playerControlsVerification = state;
});

try {
  await page.keyboard.press("Escape");
  await page.evaluate(() => { if (document.activeElement instanceof HTMLElement) document.activeElement.blur(); });
  await page.mouse.move(0, 0);
  await expect(page.locator(".footer-information")).toHaveCSS("opacity", "1");
  const initialMedia = await readMedia();
  expect(await page.evaluate(() => window.__playerControlsVerification.counts.get_now_playing ?? 0)).toBeGreaterThan(0);
  const timeline = await page.locator(".timeline").boundingBox();
  report.initial = { status: initialMedia.status, playbackStatus: initialMedia.track?.playbackStatus,
    source: initialMedia.track?.source, position: initialMedia.track?.positionSeconds };
  await page.screenshot({ path: `${out}/idle.png` });
  await reveal();
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
  expect(await page.locator(".timeline").boundingBox()).toEqual(timeline);
  await page.screenshot({ path: `${out}/hover.png` });

  await page.getByRole("button", { name: "电音强度", exact: true }).click();
  originals.strength = await strengthSlider.getAttribute("aria-valuenow");
  await expect(strength.getByRole("spinbutton")).toHaveCount(0);
  await expect(strength.getByRole("button")).toHaveCount(0);
  await expect(strength.getByText("自然", { exact: true })).toBeVisible();
  await expect(strength.getByText("明显", { exact: true })).toBeVisible();
  await setStrengthValue(67);
  await strengthSlider.focus(); await strengthSlider.press("ArrowRight");
  await expect(strengthSlider).toHaveAttribute("aria-valuenow", "68");
  const valueTip = page.getByRole("tooltip").filter({ hasText: /^\d+%$/ });
  await expect(valueTip).toHaveText("68%");
  const elasticRoot = strength.locator(".elastic-slider__root");
  const elasticTrack = strength.locator(".elastic-slider__track-wrapper");
  const elasticSurface = strength.locator(".elastic-slider__surface");
  const reducedMotion = await page.evaluate(() => matchMedia("(prefers-reduced-motion: reduce)").matches);
  await elasticRoot.hover();
  if (!reducedMotion) await expect.poll(() => strength.locator(".elastic-slider__wrapper")
    .evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeCloseTo(1.12, 3);
  const hoverSurface = await elasticSurface.boundingBox();
  const dragBounds = await elasticRoot.boundingBox();
  const viewportWidth = await page.evaluate(() => innerWidth);
  const popupBounds = await strength.boundingBox();
  const stretchSamples = [];
  await page.mouse.move(dragBounds.x + dragBounds.width / 2, dragBounds.y + dragBounds.height / 2);
  await page.mouse.down();
  const liveValues = [];
  for (const fraction of [0.25, 0.75]) {
    await page.mouse.move(dragBounds.x + dragBounds.width * fraction, dragBounds.y + dragBounds.height / 2, { steps: 5 });
    await paintedFrame();
    const value = Number(await strengthSlider.getAttribute("aria-valuenow"));
    expect(value).toBeGreaterThanOrEqual(fraction * 100 - 1);
    expect(value).toBeLessThanOrEqual(fraction * 100 + 1);
    await expect(valueTip).toHaveText(`${value}%`);
    liveValues.push(value);
  }
  for (const side of ["right", "left"]) {
    await page.mouse.move(side === "right" ? viewportWidth - 2 : 2, dragBounds.y + dragBounds.height / 2, { steps: 8 });
    await paintedFrame();
    await expect(strengthSlider).toHaveAttribute("aria-valuenow", side === "right" ? "100" : "0");
    if (!reducedMotion) await expect.poll(() => elasticTrack
      .evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeGreaterThan(1.015);
    const track = await elasticTrack.boundingBox();
    const left = await strength.getByText("自然", { exact: true }).boundingBox();
    const right = await strength.getByText("明显", { exact: true }).boundingBox();
    const backdrop = await elasticSurface.boundingBox();
    if (!reducedMotion) {
      if (side === "right") expect(track.x + track.width).toBeGreaterThan(popupBounds.x + popupBounds.width + 1);
      else expect(track.x).toBeLessThan(popupBounds.x - 1);
      expect(backdrop.width).toBeGreaterThan(hoverSurface.width + 20);
      expect(backdrop.height).toBeLessThan(hoverSurface.height);
    }
    expect(backdrop.x).toBeLessThan(left.x - 4);
    expect(backdrop.x + backdrop.width).toBeGreaterThan(right.x + right.width + 4);
    expect(backdrop.x).toBeGreaterThanOrEqual(0);
    expect(backdrop.x + backdrop.width).toBeLessThanOrEqual(viewportWidth);
    expect(await strength.boundingBox()).toEqual(popupBounds);
    expect(left.x + left.width + 4).toBeLessThanOrEqual(track.x);
    expect(track.x + track.width + 4).toBeLessThanOrEqual(right.x);
    expect(left.x).toBeGreaterThanOrEqual(0);
    expect(right.x + right.width).toBeLessThanOrEqual(viewportWidth);
    for (const name of ["自然", "明显"]) {
      expect(await strength.getByText(name, { exact: true }).evaluate(el => {
        const bounds = el.getBoundingClientRect();
        return el.contains(document.elementFromPoint(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2));
      }), `${name} must not be clipped by the popup or native window`).toBe(true);
    }
    await expect(valueTip).toHaveText(side === "right" ? "100%" : "0%");
    await expect(valueTip.locator(".warm-tooltip__arrow")).toHaveCount(0);
    await expect(valueTip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
    await expect.poll(async () => {
      const hint = await valueTip.boundingBox();
      const fill = await strength.locator(".elastic-slider__range").boundingBox();
      return Math.abs(hint.x + hint.width / 2 - (fill.x + fill.width));
    }).toBeLessThan(3);
    stretchSamples.push({ side, track, left, right, backdrop, tooltip: await valueTip.textContent(),
      scale: await elasticTrack.evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11) });
    await page.screenshot({ path: `${out}/strength-stretched-${side}.png` });
  }
  await page.mouse.up();
  await expect(valueTip).toHaveText("0%");
  await expect.poll(() => elasticTrack.evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeCloseTo(1, 3);
  await expect(valueTip).toHaveCount(0, { timeout: 2000 });
  await setStrengthValue(68);
  await page.mouse.move(0, 0);
  await expect.poll(() => strength.locator(".elastic-slider__wrapper")
    .evaluate(el => new DOMMatrix(getComputedStyle(el).transform).m11)).toBeCloseTo(1, 4);
  await paintedFrame();
  await expect(strength).toBeVisible();
  await expect(page.locator(".footer-controls")).toHaveCSS("opacity", "1");
  const compactBounds = await checkPanel(strength, true);
  expect(compactBounds.width).toBeLessThanOrEqual(240);
  expect(compactBounds.height).toBeLessThanOrEqual(54);
  const restingTrack = await elasticRoot.boundingBox();
  for (const name of ["自然", "明显"]) {
    const label = await strength.getByText(name, { exact: true }).boundingBox();
    expect(Math.abs(label.y + label.height / 2 - (restingTrack.y + restingTrack.height / 2))).toBeLessThan(1);
  }
  report.strength = { keyboardAndPointer: true, staysOpen: true, bounds: compactBounds,
    singleRow: true, elastic: { reducedMotion, springReturn: true, backgroundFollowsSlider: !reducedMotion,
      labelsUnclipped: true, samples: stretchSamples },
    tooltip: { liveValues, accurateEndpoints: true, followsVisualValue: true, dismissesAfterRelease: true } };
  await page.evaluate(() => { if (document.activeElement instanceof HTMLElement) document.activeElement.blur(); });
  await expect(valueTip).toHaveCount(0);
  await expect.poll(async () => (await elasticSurface.boundingBox()).width).toBeCloseTo(compactBounds.width, 1);
  await page.screenshot({ path: `${out}/strength.png` });
  await setStrengthValue(originals.strength);
  await page.keyboard.press("Escape");
  await expect(strength).toHaveCount(0);
  await expect(page.getByRole("button", { name: "电音强度", exact: true })).toBeFocused();

  await reveal(); await page.getByRole("button", { name: "详细参数", exact: true }).click();
  await expect(details.getByRole("slider")).toHaveCount(3);
  await expect(details.getByText(/Key|Scale|调式|调性/)).toHaveCount(0);
  const names = ["Flex-Tune", "Natural Vibrato", "Humanize"];
  originals.parameters = Object.fromEntries(await Promise.all(names.map(async name => [name,
    await details.getByRole("spinbutton", { name: `${name}数值` }).inputValue()])));
  for (const [name, value] of [["Flex-Tune", "35"], ["Natural Vibrato", "-2.5"], ["Humanize", "42"]]) {
    const input = details.getByRole("spinbutton", { name: `${name}数值` });
    await input.fill(value); await input.press("Tab");
    await expect(details.getByRole("slider", { name, exact: true })).toHaveAttribute("aria-valuenow", value);
  }
  const detailsBounds = await checkPanel(details);
  expect(detailsBounds.width).toBeLessThanOrEqual(350);
  expect(detailsBounds.height).toBeLessThanOrEqual(150);
  await expect(details.getByRole("heading")).toHaveCount(0);
  await expect(details.getByRole("button")).toHaveCount(0);
  const dialTip = page.getByRole("tooltip");
  const centering = [];
  for (const [name, meaning] of [["Flex-Tune", /滑音|音高变化/], ["Natural Vibrato", /原有.*颤音/], ["Humanize", /长音.*自然/]]) {
    const input = details.getByRole("spinbutton", { name: `${name}数值` });
    await expect(input).toHaveCSS("text-indent", "0px");
    await expect(input).toHaveCSS("text-align", "center");
    // Read both geometries in the same frame: the popover entrance can still
    // scale between two separate CDP calls even though the centers coincide.
    const { numberBox, ringBox } = await input.evaluate(el => ({
      numberBox: el.getBoundingClientRect().toJSON(),
      ringBox: el.closest(".parameter-dial").querySelector(".comet-dial__ring").getBoundingClientRect().toJSON(),
    }));
    expect(numberBox.x + numberBox.width / 2).toBeCloseTo(ringBox.x + ringBox.width / 2, 1);
    expect(numberBox.y + numberBox.height / 2).toBeCloseTo(ringBox.y + ringBox.height / 2, 1);
    centering.push({ name, numberBox, ringBox, textIndent: 0 });
    await details.getByText(name, { exact: true }).hover();
    await expect(dialTip).toContainText(meaning);
    await expect(dialTip).toBeInViewport({ ratio: 1 });
    await expect(dialTip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
    await expect(dialTip.locator(".warm-tooltip__arrow")).toHaveCount(0);
    await page.screenshot({ path: `${out}/help-${name.replaceAll(" ", "-")}.png` });
    await page.mouse.move(0, 0); await expect(dialTip).toHaveCount(0);
  }
  const vibrato = details.getByRole("slider", { name: "Natural Vibrato", exact: true });
  const vibratoInput = details.getByRole("spinbutton", { name: "Natural Vibrato数值" });
  const lit = vibrato.locator(".comet-dial__lit");
  const vibratoSamples = [];
  for (const [value, length, side] of [["0", 0, 0], ["-0.3", 4.54, -1], ["0.3", 4.54, 1], ["-12", 181.51, -1], ["12", 181.51, 1], ["0", 0, 0]]) {
    await vibratoInput.fill(value); await vibratoInput.press("Enter");
    if (side === 0) {
      // A spring can pass through zero before overshooting. Require sustained
      // empty progress across frames, not a lucky zero-crossing snapshot.
      await expect.poll(() => lit.evaluate(async el => {
        const samples = [];
        for (let i = 0; i < 12; i++) {
          await new Promise(resolve => requestAnimationFrame(resolve));
          samples.push(el.getAttribute("d"));
        }
        return samples.every(path => path === "");
      })).toBe(true);
      await expect(vibrato.locator(".comet-dial__head")).toHaveAttribute("cx", "100.000");
      await expect(vibrato.locator(".comet-dial__head")).toHaveAttribute("cy", "20.000");
    } else {
      await expect.poll(() => lit.evaluate(el => el.getTotalLength())).toBeCloseTo(length, 1);
      const points = await lit.evaluate(el => [0, 0.25, 0.5, 0.75, 1].map(f => {
        const p = el.getPointAtLength(el.getTotalLength() * f); return { x: p.x, y: p.y };
      }));
      expect(points.some(p => Math.abs(p.x - 100) < 0.01 && Math.abs(p.y - 20) < 0.01)).toBe(true);
      expect(points.every(p => side < 0 ? p.x <= 100.01 : p.x >= 99.99)).toBe(true);
    }
    vibratoSamples.push({ value, ...await lit.evaluate(el => ({ d: el.getAttribute("d"), length: el.getTotalLength() })) });
    await page.mouse.move(0, 0); await expect(dialTip).toHaveCount(0);
    await page.screenshot({ path: `${out}/vibrato-${value}.png` });
  }
  const dial = details.getByRole("slider", { name: "Flex-Tune", exact: true });
  await expect.poll(async () => (await dial.boundingBox()).width).toBeCloseTo(86, 1);
  const dialBounds = await dial.boundingBox();
  const dialPoint = fraction => {
    const angle = (140 + fraction * 260) * Math.PI / 180;
    return { x: dialBounds.x + dialBounds.width * (0.5 + 0.4 * Math.cos(angle)),
      y: dialBounds.y + dialBounds.height * (0.5 + 0.4 * Math.sin(angle)) };
  };
  const start = dialPoint(0.25);
  await page.mouse.move(start.x, start.y); await page.mouse.down();
  await expect(dial).toHaveAttribute("aria-valuenow", "25");
  for (const fraction of [0.5, 0.75]) {
    const point = dialPoint(fraction);
    await page.mouse.move(point.x, point.y, { steps: 4 }); await paintedFrame();
    await expect(dial).toHaveAttribute("aria-valuenow", String(fraction * 100));
    await expect(details.getByRole("spinbutton", { name: "Flex-Tune数值" })).toHaveValue(String(fraction * 100));
    await expect(dialTip).toHaveCount(0);
  }
  if (!reducedMotion) await expect.poll(() => dial.locator(".comet-dial__comet path").evaluateAll(nodes =>
    nodes.some(node => Number(getComputedStyle(node).opacity) > 0.01))).toBe(true);
  await page.screenshot({ path: `${out}/details-dial-dragging.png` });
  await page.mouse.up();
  await expect(dialTip).toHaveCount(0);
  await expect.poll(() => dial.locator(".comet-dial__comet path").evaluateAll(nodes =>
    nodes.every(node => Number(getComputedStyle(node).opacity) === 0))).toBe(true);
  await expect(dial).toHaveAttribute("aria-valuenow", "75");
  await expect(details.getByRole("spinbutton", { name: "Flex-Tune数值" })).toHaveValue("75");
  await dial.press("Home");
  await details.getByRole("spinbutton", { name: "Flex-Tune数值" }).fill("120");
  const minimum = dialPoint(0.0001);
  await page.mouse.move(minimum.x, minimum.y); await page.mouse.down();
  await expect(dial).toHaveAttribute("aria-valuenow", "0");
  await expect(details.getByRole("spinbutton", { name: "Flex-Tune数值" })).toHaveValue("0");
  await page.mouse.up(); await expect(dialTip).toHaveCount(0);
  report.details = { parameters: names, excludesKeyScale: true, bounds: detailsBounds, centering, vibratoSamples,
    help: { describesPurpose: true, labelHover: true, black: true, arrow: false, fitsNativeWindow: true },
    comet: { angularDrag: true, exactValues: [25, 50, 75], noValueFling: true, draftSync: true,
      tailSettles: true, noDuplicateNumericTooltip: true, numericInput: true } };
  await page.screenshot({ path: `${out}/details.png` });
  for (const [name, value] of Object.entries(originals.parameters)) {
    const input = details.getByRole("spinbutton", { name: `${name}数值` });
    await input.fill(value); await input.press("Tab");
  }
  await page.keyboard.press("Escape");

  originals.vocalRemovalEnabled = await vocalRemoval.getAttribute("aria-pressed");
  if (originals.vocalRemovalEnabled === "true") await vocalRemoval.click();
  await reveal();
  await expect(vocalRemoval).toHaveAccessibleName("开启去人声");
  await expect(vocalRemoval).toHaveAttribute("aria-pressed", "false");
  await expect(vocalRemoval).toHaveCSS("color", "rgb(154, 159, 172)");
  const regularVocalIcon = await vocalRemoval.locator("svg").innerHTML();
  await page.screenshot({ path: `${out}/vocal-removal-off.png` });
  await vocalRemoval.hover();
  await expect(page.getByRole("tooltip")).toContainText("开启去人声");
  await vocalRemoval.click();
  await expect(vocalRemoval).toHaveAccessibleName("关闭去人声");
  await expect(vocalRemoval).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByRole("status")).toContainText("去人声处理尚未接入");
  await reveal();
  await expect(page.getByRole("tooltip")).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => getComputedStyle(document.querySelector(".vocal-removal-control")).color
    === getComputedStyle(document.querySelector(".transport-primary")).color)).toBe(true);
  expect(await vocalRemoval.locator("svg").innerHTML()).not.toBe(regularVocalIcon);
  await page.screenshot({ path: `${out}/vocal-removal-on.png` });
  await vocalRemoval.hover();
  const vocalTip = page.getByRole("tooltip");
  await expect(vocalTip).toContainText("关闭去人声，恢复原唱");
  await expect(vocalTip.locator(".warm-tooltip__arrow")).toHaveCount(0);
  await expect(vocalTip.locator(".warm-tooltip__box")).toHaveCSS("background-color", "rgb(10, 10, 10)");
  await expect(vocalTip).toBeInViewport({ ratio: 1 });
  await page.screenshot({ path: `${out}/vocal-removal-tooltip.png` });
  await vocalRemoval.press("Space");
  await expect(vocalRemoval).toHaveAttribute("aria-pressed", "false");
  expect(await vocalRemoval.locator("svg").innerHTML()).toBe(regularVocalIcon);
  if (originals.vocalRemovalEnabled === "true") await vocalRemoval.click();
  await expect(vocalRemoval).toHaveAttribute("aria-pressed", originals.vocalRemovalEnabled);
  report.vocalRemoval = { bothStates: true, mouseAndKeyboard: true, distinctIconWeights: true,
    offIsMuted: true, onUsesAccent: true, explanatoryTooltip: true, previewOnly: true };
  const transport = page.locator(".transport-primary");
  report.transportDisabled = await transport.isDisabled();
  // Transport is now real. This read-only UI verifier must never click it.
  report.transportReadOnly = true;
  await page.mouse.move(0, 0);
  await page.evaluate(() => { if (document.activeElement instanceof HTMLElement) document.activeElement.blur(); });
  await expect(page.locator(".footer-information")).toHaveCSS("opacity", "1");
  const finalMedia = await readMedia();
  report.final = { status: finalMedia.status, playbackStatus: finalMedia.track?.playbackStatus,
    position: finalMedia.track?.positionSeconds };
  report.mainVisible = await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|is_visible", { label: "main" }));
  expect(report.mainVisible).toBe(true);
  expect(errors).toEqual([]);
  report.passed = true;
} catch (error) {
  report.failure = String(error);
} finally {
  try {
    // Restore all local preview values even when a check failed partway through.
    await page.mouse.up();
    await page.keyboard.press("Escape"); await reveal();
    if (originals.strength !== null) {
      await page.getByRole("button", { name: "电音强度", exact: true }).click();
      await setStrengthValue(originals.strength);
      await page.keyboard.press("Escape");
      await expect(strength).toHaveCount(0);
      await expect(page.getByRole("button", { name: "电音强度", exact: true })).toBeFocused();
    }
    if (originals.parameters) {
      await page.getByRole("button", { name: "详细参数", exact: true }).click();
      for (const [name, value] of Object.entries(originals.parameters)) {
        const input = details.getByRole("spinbutton", { name: `${name}数值` });
        await input.fill(value); await input.press("Tab");
        await expect(input).toHaveValue(value);
      }
      await page.keyboard.press("Escape");
      // Popover restores trigger focus when its exit animation unmounts.
      // Wait for that restoration before clearing focus for the idle screenshot.
      await expect(details).toHaveCount(0);
      await expect(page.getByRole("button", { name: "详细参数", exact: true })).toBeFocused();
    }
    if (originals.vocalRemovalEnabled !== null && await vocalRemoval.getAttribute("aria-pressed") !== originals.vocalRemovalEnabled)
      await vocalRemoval.click();
    await page.mouse.move(0, 0);
    await paintedFrame();
    await page.evaluate(() => { if (document.activeElement instanceof HTMLElement) document.activeElement.blur(); });
    await paintedFrame();
    await expect(page.locator(".footer-information")).toHaveCSS("opacity", "1");
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await expect(page.getByRole("tooltip").filter({ hasText: /^\d+%$/ })).toHaveCount(0);
    await expect(page.getByRole("status")).toHaveCount(0, { timeout: 5000 });
    report.previewsRestored = true;
  } catch (error) { report.cleanupFailure = String(error); report.passed = false; }
  report.instrumentation = await page.evaluate(() => {
    const state = window.__playerControlsVerification;
    const fetchIntact = window.fetch === state.fetchWrapper;
    const postIntact = window.chrome.webview.postMessage === state.postWrapper;
    if (fetchIntact) window.fetch = state.originalFetch;
    if (postIntact) window.chrome.webview.postMessage = state.originalPost;
    for (const type of state.pointerTypes) document.removeEventListener(type, state.pointerTrace, true);
    const wrapperIntact = fetchIntact && postIntact;
    const restored = window.fetch === state.originalFetch && window.chrome.webview.postMessage === state.originalPost;
    delete window.__playerControlsVerification;
    return { commands: state.counts, blocked: state.blocked, pointers: state.pointers, wrapperIntact, restored };
  });
  report.settingsUnchanged = JSON.stringify(originalSettings) === JSON.stringify(await page.evaluate(() => ({
    colors: localStorage.getItem("helper-colors-v1"), background: localStorage.getItem("helper-background-v1"),
  })));
  if (!report.settingsUnchanged || !report.instrumentation.restored || !report.instrumentation.wrapperIntact
    || report.instrumentation.blocked.length || errors.length) report.passed = false;
  await page.screenshot({ path: `${out}/restored.png` });
  await writeFile(`${out}/verification.json`, JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ ...report, instrumentation: { ...report.instrumentation,
    pointerEvents: report.instrumentation.pointers.length, pointers: undefined } }, null, 2));
}
// End this CDP client without closing the native helper or any music application.
process.exit(report.passed ? 0 : 1);
