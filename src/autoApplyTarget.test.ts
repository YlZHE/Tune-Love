import { describe, expect, test } from "vitest";
import { initialControlState, type ControlState } from "./autotuneControl";
import type { AutoTuneTarget } from "./keyDetection";
import { nextWrite, targetTooltip, writeState, type WrittenPair } from "./autoApplyTarget";
import { AUTO_APPLY_STORAGE_KEY, parseAutoApplyPreference } from "./autoApplyPreferences";

const ready = (connectionId = "session-a", capabilities = ["retune", "key", "scale"]): ControlState => ({
  ...initialControlState, phase: "ready", connectionId, capabilities, instanceCount: 1,
  target: { pid: 1, processName: "host.exe", pluginName: "Auto-Tune Pro", profileId: "autotune-pro-38c42d0b-x64" },
});
const base = { candidate: null, uncoveredNotes: [] as number[], source: "analysis" as const };
const minor: AutoTuneTarget = { ...base, key: 6, scale: "minor", evidenceSeconds: 12 };
const major: AutoTuneTarget = { ...base, key: 11, scale: "major", evidenceSeconds: 20 };
const chromatic: AutoTuneTarget = { ...base, key: null, scale: "chromatic", evidenceSeconds: 3,
  candidate: { key: 2, scale: "major" } };
const written = (overrides: Partial<WrittenPair> = {}): WrittenPair => ({
  connectionId: "session-a", trackKey: "song-a", key: "F#", scale: "Minor", status: "written", ...overrides,
});

describe("nextWrite", () => {
  test("writes a new Major/Minor pair as profile option labels", () => {
    expect(nextWrite(null, minor, ready(), "song-a")).toEqual({ key: "F#", scale: "Minor" });
    expect(nextWrite(null, major, ready(), "song-a")).toEqual({ key: "B", scale: "Major" });
  });

  test("a null target (no valid snapshot) writes nothing", () => {
    expect(nextWrite(null, null, ready(), "song-a")).toBeNull();
    expect(nextWrite(written(), null, ready(), "song-a")).toBeNull();
  });

  test("a no-evidence target as Rust reports it is written as Chromatic with no key", () => {
    const noEvidence: AutoTuneTarget = { ...base, key: null, scale: "chromatic", evidenceSeconds: 0 };
    expect(nextWrite(null, noEvidence, ready(), "song-a")).toEqual({ key: null, scale: "Chromatic" });
    expect(nextWrite(written({ key: null, scale: "Chromatic" }), noEvidence, ready(), "song-a")).toBeNull();
  });

  test("Chromatic targets write the scale only, or key plus Chromatic when a key is known", () => {
    expect(nextWrite(null, chromatic, ready(), "song-a")).toEqual({ key: null, scale: "Chromatic" });
    expect(nextWrite(null, { ...chromatic, key: 8 }, ready(), "song-a")).toEqual({ key: "G#", scale: "Chromatic" });
    expect(nextWrite(written({ key: null, scale: "Chromatic" }), chromatic, ready(), "song-a")).toBeNull();
    expect(nextWrite(written(), chromatic, ready("session-a", ["scale"]), "song-a"))
      .toEqual({ key: null, scale: "Chromatic" });
    expect(nextWrite(null, { ...chromatic, key: 8 }, ready("session-a", ["scale"]), "song-a")).toBeNull();
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

describe("targetTooltip", () => {
  const fChromatic: AutoTuneTarget = { ...base, key: 5, scale: "chromatic", evidenceSeconds: 14,
    candidate: { key: 5, scale: "minor" }, uncoveredNotes: [8, 11] };
  const fWritten = written({ key: "F", scale: "Chromatic" });

  test("evidence and write status for a Major/Minor target", () => {
    expect(targetTooltip(minor, null, true)).toBe("已分析 12 秒。连接 Auto-Tune 后自动写入。");
    expect(targetTooltip(minor, written(), true)).toBe("已分析 12 秒。已写入当前连接的插件。");
    expect(targetTooltip(minor, written({ status: "pending" }), true)).toBe("已分析 12 秒。连接 Auto-Tune 后自动写入。");
    expect(targetTooltip(minor, written({ status: "failed" }), true)).toBe("已分析 12 秒。写入失败，不会自动重试。");
    expect(targetTooltip(minor, written(), false)).toBe("已分析 12 秒。自动写入已关闭，可在设置中开启。");
    expect(targetTooltip({ ...minor, source: "cache", evidenceSeconds: 2.4 }, null, true))
      .toBe("来自上次播放的分析结果（本次已分析 2 秒，足够后会重新确认）。连接 Auto-Tune 后自动写入。");
  });

  test("a remembered pair for another target is not this target's status", () => {
    expect(targetTooltip(major, written(), true)).toBe("已分析 20 秒。连接 Auto-Tune 后自动写入。");
    expect(writeState(minor, written({ status: "failed" }), true)).toBe("failed");
    expect(writeState(major, written({ status: "failed" }), true)).toBe("idle");
    expect(writeState(minor, written({ status: "failed" }), false)).toBe("off");
  });

  test("the Chromatic uncertainty sentences use profile spellings", () => {
    expect(targetTooltip(fChromatic, fWritten, true))
      .toBe("已分析 14 秒。拿不准：G# 与 B 都常用，暂用 Chromatic。排第一的候选是 F Minor。已写入当前连接的插件。");
    expect(targetTooltip({ ...fChromatic, uncoveredNotes: [1] }, null, true))
      .toBe("已分析 14 秒。拿不准：C# 常用但不在候选调内，暂用 Chromatic。排第一的候选是 F Minor。连接 Auto-Tune 后自动写入。");
    expect(targetTooltip({ ...fChromatic, uncoveredNotes: [] }, null, true))
      .toBe("已分析 14 秒。暂用 Chromatic。排第一的候选是 F Minor。连接 Auto-Tune 后自动写入。");
  });

  test("no candidate means the analysis just started; Major/Minor never gets the sentence", () => {
    expect(targetTooltip({ ...chromatic, candidate: null, evidenceSeconds: 0 }, null, true))
      .toBe("已分析 0 秒。刚开始分析，暂用 Chromatic。连接 Auto-Tune 后自动写入。");
    expect(targetTooltip({ ...minor, uncoveredNotes: [1] }, null, true)).not.toContain("Chromatic");
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
