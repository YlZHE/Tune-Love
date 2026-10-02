// Read-only smoke test of our own debug application. Never connects a host.
import { chromium, expect } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";

const browser = await chromium.connectOverCDP("http://127.0.0.1:19225");
const pages = () => browser.contexts().flatMap(context => context.pages());
const main = pages().find(page => page.url().includes("tauri.localhost") && !page.url().includes("view=settings"));
if (!main) throw new Error("The owned Tauri debug main window was not found");
const errors = [];
main.on("pageerror", error => errors.push(error.message));
await expect(main.locator(".music-window")).toBeVisible();
const initial = await main.evaluate(() => window.__TAURI_INTERNALS__.invoke("autotune_command", { request: { op: "status" } }));
expect(initial.ok).toBe(true);
expect(initial.state.phase).toBe("disconnected");
await main.getByRole("button", { name: "设置", exact: true }).click();
await expect.poll(() => pages().some(page => page.url().includes("view=settings"))).toBe(true);
const settings = pages().find(page => page.url().includes("view=settings"));
settings.on("pageerror", error => errors.push(error.message));
await expect(settings.getByText("未连接 Auto-Tune", { exact: true })).toBeVisible();
// Exercise native read-only discovery. No connect/apply, no selection automation.
const scan = await settings.evaluate(() => window.__TAURI_INTERNALS__.invoke("autotune_command", { request: { op: "scan" } }));
expect(scan.ok, JSON.stringify(scan)).toBe(true);
expect(scan.state.phase).toBe("disconnected");
expect(scan.state.audioVerified).toBe(false);
expect(Array.isArray(scan.candidates)).toBe(true);
await mkdir("artifacts/app-control", { recursive: true });
await settings.screenshot({ path: "artifacts/app-control/native-settings.png" });
expect(errors).toEqual([]);
const result = { nativeIPC: true, initial, scan, errors, connectedAnyHost: false };
await writeFile("artifacts/app-control/native-smoke.json", JSON.stringify(result, null, 2));
console.log(JSON.stringify({ ok: true, candidates: scan.candidates.length, skippedCount: scan.skippedCount, connectedAnyHost: false }));
await browser.close();
