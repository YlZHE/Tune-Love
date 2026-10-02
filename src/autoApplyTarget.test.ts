import { describe, expect, test } from "vitest";
import { initialControlState, type ControlState } from "./autotuneControl";
import type { AutoTuneTarget } from "./keyDetection";
import { nextWrite, targetStatusLabel, type WrittenPair } from "./autoApplyTarget";
import { AUTO_APPLY_STORAGE_KEY, parseAutoApplyPreference } from "./autoApplyPreferences";

const ready = (connectionId = "session-a", capabilities = ["retune", "key", "scale"]): ControlState => ({
  ...initialControlState, phase: "ready", connectionId, capabilities, instanceCount: 1,
  target: { pid: 1, processName: "host.exe", pluginName: "Auto-Tune Pro", profileId: "autotune-pro-38c42d0b-x64" },
});
const minor: AutoTuneTarget = { key: 6, scale: "minor", evidenceSeconds: 12, source: "analysis" };
const major: AutoTuneTarget = { key: 11, scale: "major", evidenceSeconds: 20, source: "analysis" };
const written = (overrides: Partial<WrittenPair> = {}): WrittenPair => ({
  connectionId: "session-a", trackKey: "song-a", key: "F#", scale: "Minor", status: "written", ...overrides,
});

describe("nextWrite", () => {
  test("writes a new Major/Minor pair as profile option labels", () => {
    expect(nextWrite(null, minor, ready(), "song-a")).toEqual({ key: "F#", scale: "Minor" });
    expect(nextWrite(null, major, ready(), "song-a")).toEqual({ key: "B", scale: "Major" });
  });

  test("writes nothing before the song has a target", () => {
    expect(nextWrite(null, null, ready(), "song-a")).toBeNull();
    expect(nextWrite(written(), null, ready(), "song-a")).toBeNull();
  });

  test("never re-sends the pair already written for this connection and track", () => {
    expect(nextWrite(written(), minor, ready(), "song-a")).toBeNull();
    expect(nextWrite(written({ status: "pending" }), minor, ready(), "song-a")).toBeNull();
  });

  test("a failed write is not retried for the same pair", () => {
    expect(nextWrite(written({ status: "failed" }), minor, ready(), "song-a")).toBeNull();
  });

  test("a different pair for the same track is written", () => {
    expect(nextWrite(written(), major, ready(), "song-a")).toEqual({ key: "B", scale: "Major" });
    expect(nextWrite(written(), { ...minor, scale: "major" }, ready(), "song-a")).toEqual({ key: "F#", scale: "Major" });
  });

  test("track or connection changes reset the memory", () => {
    expect(nextWrite(written(), minor, ready(), "song-b")).toEqual({ key: "F#", scale: "Minor" });
    expect(nextWrite(written(), minor, ready("session-b"), "song-a")).toEqual({ key: "F#", scale: "Minor" });
  });

  test.each(["disconnected", "awaiting", "error"] as const)("does not write while %s", phase => {
    expect(nextWrite(null, minor, { ...ready(), phase }, "song-a")).toBeNull();
  });

  test("requires both key and scale capabilities and a connection id", () => {
    expect(nextWrite(null, minor, ready("session-a", ["retune", "key"]), "song-a")).toBeNull();
    expect(nextWrite(null, minor, ready("session-a", ["scale"]), "song-a")).toBeNull();
    expect(nextWrite(null, minor, { ...ready(), connectionId: null }, "song-a")).toBeNull();
    expect(nextWrite(null, minor, ready(), "")).toBeNull();
  });
});

describe("targetStatusLabel", () => {
  test("describes the recommendation and whether it was written", () => {
    expect(targetStatusLabel(minor, null, true)).toBe("建议 F♯ 小调 · 未写入");
    expect(targetStatusLabel(minor, written(), true)).toBe("建议 F♯ 小调 · 已写入");
    expect(targetStatusLabel(minor, written({ status: "pending" }), true)).toBe("建议 F♯ 小调 · 写入中");
    expect(targetStatusLabel(minor, written({ status: "failed" }), true)).toBe("建议 F♯ 小调 · 写入失败");
  });

  test("a remembered pair for another target or a disabled switch reads as not written", () => {
    expect(targetStatusLabel(major, written(), true)).toBe("建议 B 大调 · 未写入");
    expect(targetStatusLabel(minor, written(), false)).toBe("建议 F♯ 小调 · 未写入");
  });
});

describe("parseAutoApplyPreference", () => {
  test("defaults off and only accepts an explicit true", () => {
    expect(AUTO_APPLY_STORAGE_KEY).toBe("helper-auto-apply-v1");
    expect(parseAutoApplyPreference(null)).toBe(false);
    expect(parseAutoApplyPreference("")).toBe(false);
    expect(parseAutoApplyPreference("{bad json")).toBe(false);
    expect(parseAutoApplyPreference(JSON.stringify({ enabled: "true" }))).toBe(false);
    expect(parseAutoApplyPreference(JSON.stringify({ enabled: 1 }))).toBe(false);
    expect(parseAutoApplyPreference(JSON.stringify({ enabled: true }))).toBe(true);
    expect(parseAutoApplyPreference(JSON.stringify({ enabled: false }))).toBe(false);
  });
});
