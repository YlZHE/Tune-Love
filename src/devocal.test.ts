import { describe, expect, it } from "vitest";
import { devocalLabel, isDevocalActive, parseDevocalStatus, OFF_STATUS, type DevocalStatus } from "./devocal";

const status = (patch: Partial<DevocalStatus>): DevocalStatus => ({ ...OFF_STATUS, ...patch });

describe("devocalLabel", () => {
  it("tells the user to adjust this app's volume when the player volume was overridden", () => {
    expect(devocalLabel(status({ phase: "devocal", sessionOverridden: true }))).toBe("播放器音量被调整，请调本应用音量");
  });
  it("reports silent input as a possible exclusive-mode player", () => {
    expect(devocalLabel(status({ phase: "passthrough", held: true, inputSilent: true }))).toBe("未录到播放器声音（可能是独占模式）");
  });
  it("orders sessionOverridden before inputSilent and both before devocal", () => {
    expect(devocalLabel(status({ phase: "devocal", sessionOverridden: true, inputSilent: true, latencyMs: 40 })))
      .toBe("播放器音量被调整，请调本应用音量");
    expect(devocalLabel(status({ phase: "devocal", inputSilent: true, latencyMs: 40 }))).toBe("未录到播放器声音（可能是独占模式）");
  });
  it("rounds latency while devocal is running", () => {
    expect(devocalLabel(status({ phase: "devocal", held: true, latencyMs: 45.4 }))).toBe("去人声中 · 延迟 45 ms");
    expect(devocalLabel(status({ phase: "devocal", held: true, latencyMs: 45.6 }))).toBe("去人声中 · 延迟 46 ms");
  });
  it("omits the latency when the engine has not measured it yet", () => {
    expect(devocalLabel(status({ phase: "devocal", held: true, latencyMs: null }))).toBe("去人声中");
  });
  it("explains each fallback reason", () => {
    expect(devocalLabel(status({ phase: "fallback", held: true, fallbackReason: "overload" }))).toBe("性能不足，已退回原声");
    expect(devocalLabel(status({ phase: "fallback", held: true, fallbackReason: "model_error" }))).toBe("去人声模型出错，已退回原声");
  });
  it("shows a neutral label for a fallback without a reason", () => {
    expect(devocalLabel(status({ phase: "fallback", held: true, fallbackReason: null }))).toBeNull();
  });
  it("shows attaching for attaching and restarting", () => {
    expect(devocalLabel(status({ phase: "attaching" }))).toBe("正在接管播放器…");
    expect(devocalLabel(status({ phase: "restarting" }))).toBe("正在接管播放器…");
  });
  it("reports a failed engine as holding the original sound", () => {
    expect(devocalLabel(status({ phase: "failed", error: "engine_crashed" }))).toBe("去人声引擎多次异常，已保持原声");
  });
  it("tells a missing model apart from an engine that cannot start", () => {
    expect(devocalLabel(status({ phase: "unavailable", error: "model_not_found" }))).toBe("未找到去人声模型");
    expect(devocalLabel(status({ phase: "unavailable", error: "engine_unavailable: spawn failed" }))).toBe("去人声引擎无法启动");
    expect(devocalLabel(status({ phase: "unavailable", error: "engine_unavailable" }))).toBe("去人声引擎无法启动");
    expect(devocalLabel(status({ phase: "unavailable", error: null }))).toBe("未找到去人声模型");
    expect(devocalLabel(status({ phase: "unavailable", error: "no_model: x" }))).toBe("未找到去人声模型");
  });
  it("shows passthrough only while the player is held", () => {
    expect(devocalLabel(status({ phase: "passthrough", held: true }))).toBe("原声直通");
    expect(devocalLabel(status({ phase: "passthrough", held: false }))).toBeNull();
  });
  it("shows nothing for idle phases", () => {
    expect(devocalLabel(status({ phase: "off" }))).toBeNull();
    expect(devocalLabel(status({ phase: "releasing" }))).toBeNull();
  });
});

describe("isDevocalActive", () => {
  it("is true only for attaching, devocal and restarting", () => {
    const active = ["attaching", "devocal", "restarting"];
    for (const phase of ["off", "attaching", "passthrough", "devocal", "fallback", "releasing", "restarting", "failed", "unavailable"] as const)
      expect(isDevocalActive(phase)).toBe(active.includes(phase));
  });
});

describe("parseDevocalStatus", () => {
  it("accepts a well-formed status", () => {
    const value = { phase: "devocal", held: true, latencyMs: 12, loadRatio: 0.4, fallbackReason: null,
      sessionOverridden: false, inputSilent: false, error: null };
    expect(parseDevocalStatus(value)).toEqual(value);
  });
  it("rejects values without a known phase so a stub or null reply cannot change the UI", () => {
    expect(parseDevocalStatus(null)).toBeNull();
    expect(parseDevocalStatus({})).toBeNull();
    expect(parseDevocalStatus({ phase: "exploded" })).toBeNull();
  });
  it("fills missing or mistyped fields with safe defaults", () => {
    expect(parseDevocalStatus({ phase: "off", held: "yes", latencyMs: "x", fallbackReason: "weird" })).toEqual(OFF_STATUS);
  });
});
