import { test, expect } from "@playwright/test";
import { installTransportGuard } from "../scripts/transport-guard.mjs";

test("native verifier enforces source and generation at the actual IPC boundary and observes real results", async ({ page }) => {
  await page.goto("about:blank");
  await page.evaluate(() => {
    const state = window as any;
    state.forwarded = [];
    state.responseKind = "ok";
    state.fetch = async (_input: unknown, init: any) => {
      state.forwarded.push(JSON.parse(init.body));
      return new Response(JSON.stringify(state.responseKind === "ok" ? null : { code: "stale_target" }), {
        headers: { "Tauri-Response": state.responseKind, "Content-Type": "application/json" },
      });
    };
    state.chrome ??= {};
    state.chrome.webview = { postMessage: () => { state.posted = true; } };
    state.__TAURI_INTERNALS__ = { runCallback: () => {} };
    state.baselineFetch = state.fetch;
    state.baselinePost = state.chrome.webview.postMessage;
  });
  await page.evaluate(installTransportGuard, "player.exe");
  const outcomes = await page.evaluate(async () => {
    const state = window as any;
    const expected = { action: "next", sourceId: "player.exe", sessionId: "player.exe", trackKey: "song", targetGeneration: 1 };
    const send = (request: unknown) => fetch("http://ipc.localhost/control_media", { method: "POST", body: JSON.stringify({ request }) });
    state.__transportVerification.expected = expected;
    await send({ ...expected, sourceId: "other.exe" });
    await send({ ...expected, targetGeneration: 2 });
    const blockedCalls = state.forwarded.length;
    state.__transportVerification.expected = expected;
    const ok = await send(expected);
    await send(expected); // No second request is authorized by the same click.
    state.__transportVerification.expected = expected;
    state.responseKind = "error";
    const error = await send(expected);
    return { blockedCalls, forwarded: state.forwarded.length, blocked: state.__transportVerification.blocked.length,
      records: state.__transportVerification.records, ok: await ok.json(), error: await error.json() };
  });
  expect(outcomes.blockedCalls).toBe(0);
  expect(outcomes.forwarded).toBe(2);
  expect(outcomes.blocked).toBe(3);
  expect(outcomes.records.map((record: any) => record.outcome)).toEqual(["ok", "error"]);
  expect(outcomes.ok).toBeNull();
  expect(outcomes.error).toEqual({ code: "stale_target" });
  expect(await page.evaluate(() => {
    const state = window as any;
    return state.__transportVerification.restore().restored && state.fetch === state.baselineFetch
      && state.chrome.webview.postMessage === state.baselinePost && !state.__transportVerification;
  })).toBe(true);
});

test("native verifier forwards key detection as read-only IPC and still blocks unknown commands", async ({ page }) => {
  await page.goto("about:blank");
  await page.evaluate(() => {
    const state = window as any;
    state.forwarded = [];
    state.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      state.forwarded.push(new URL(String(input), location.href).pathname.slice(1));
      return new Response(JSON.stringify({ status: "idle" }), {
        headers: { "Tauri-Response": "ok", "Content-Type": "application/json" },
      });
    };
    state.chrome = { webview: { postMessage: () => { state.posted = true; } } };
    state.__TAURI_INTERNALS__ = { runCallback: () => {} };
  });
  await page.evaluate(installTransportGuard, "player.exe");
  const result = await page.evaluate(async () => {
    const state = window as any;
    const key = await fetch("http://ipc.localhost/get_key_detection", { method: "POST", body: "{}" });
    const unknown = await fetch("http://ipc.localhost/write_something", { method: "POST", body: "{}" });
    const restored = state.__transportVerification.restore();
    return { key: await key.json(), unknown: await unknown.json(), forwarded: state.forwarded, restored };
  });
  expect(result.key).toEqual({ status: "idle" });
  expect(result.unknown).toEqual({ code: "verification_scope" });
  expect(result.forwarded).toEqual(["get_key_detection"]);
  expect(result.restored.restored).toBe(true);
});
