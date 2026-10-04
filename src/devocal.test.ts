import { describe, expect, it } from "vitest";
import { approxLatency, deviceLine, devocalLabel, devocalLabelKind, devocalNotice, isDevocalActive, parseDevocalStatus, OFF_STATUS, type DevocalStatus } from "./devocal";

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
  it("shows the latency as an estimate rounded to 5 ms while devocal is running", () => {
    expect(devocalLabel(status({ phase: "devocal", held: true, latencyMs: 45.4 }))).toBe("去人声中 · 延迟约 45 ms");
    expect(devocalLabel(status({ phase: "devocal", held: true, latencyMs: 77.4 }))).toBe("去人声中 · 延迟约 75 ms");
    expect(devocalLabel(status({ phase: "devocal", held: true, latencyMs: 77.6 }))).toBe("去人声中 · 延迟约 80 ms");
  });
  it("keeps the latency out of the announced text", () => {
    expect(devocalNotice(status({ phase: "devocal", held: true, latencyMs: 75.1 })))
      .toEqual({ text: "去人声中", kind: "neutral", detail: " · 延迟约 75 ms" });
    expect(devocalNotice(status({ phase: "devocal", held: true }))).toEqual({ text: "去人声中", kind: "neutral" });
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
  it("says it is waiting for a player when there is nothing to take over", () => {
    expect(devocalLabel(status({ phase: "attaching", waitingForPlayer: true }))).toBe("等待播放器");
    expect(devocalLabel(status({ phase: "restarting", waitingForPlayer: true }))).toBe("等待播放器");
    expect(devocalLabelKind(status({ phase: "attaching", waitingForPlayer: true }))).toBe("neutral");
    expect(isDevocalActive("attaching")).toBe(true);
  });
  it("reports a failed engine as holding the original sound", () => {
    expect(devocalLabel(status({ phase: "failed", error: "engine_crashed" }))).toBe("去人声引擎多次异常，已保持原声");
  });
  it("tells a player that cannot be taken over apart from an engine crash", () => {
    expect(devocalLabel(status({ phase: "failed", error: "attach_failed" }))).toBe("无法接管这个播放器");
    expect(devocalLabel(status({ phase: "failed", error: "attach_failed: access denied" }))).toBe("无法接管这个播放器");
    expect(devocalLabelKind(status({ phase: "failed", error: "attach_failed" }))).toBe("warning");
    expect(devocalLabel(status({ phase: "failed", error: "engine_crashed" }))).not.toBe("无法接管这个播放器");
  });
  it("warns when the engine could not load the model", () => {
    expect(devocalLabel(status({ phase: "passthrough", held: true, error: "model_load_failed: bad file" }))).toBe("去人声模型加载失败，已保持原声");
    expect(devocalLabel(status({ phase: "passthrough", held: true, error: "no_model: x" }))).toBe("去人声模型加载失败，已保持原声");
    expect(devocalLabelKind(status({ phase: "passthrough", held: true, error: "model_load_failed: x" }))).toBe("warning");
    // Other errors do not hide the passthrough label.
    expect(devocalLabel(status({ phase: "passthrough", held: true, error: "capture_failed: x" }))).toBe("原声直通");
    // A running devocal wins over a stale error.
    expect(devocalLabel(status({ phase: "devocal", held: true, error: "model_load_failed: x" }))).toBe("去人声中");
  });
  it("tells a missing model apart from an engine that cannot start", () => {
    expect(devocalLabel(status({ phase: "unavailable", error: "model_not_found:stemgenrt-hop128" }))).toBe("未找到去人声模型，请在设置中下载");
    expect(devocalLabel(status({ phase: "unavailable", error: "engine_unavailable: spawn failed" }))).toBe("去人声引擎无法启动");
    expect(devocalLabel(status({ phase: "unavailable", error: "engine_unavailable" }))).toBe("去人声引擎无法启动");
    expect(devocalLabel(status({ phase: "unavailable", error: null }))).toBe("未找到去人声模型，请在设置中下载");
    expect(devocalLabel(status({ phase: "unavailable", error: "no_model: x" }))).toBe("未找到去人声模型，请在设置中下载");
  });
  it("offers to open the model settings only for a missing model", () => {
    expect(devocalNotice(status({ phase: "unavailable", error: "model_not_found:stemgenrt-hop128" })))
      .toEqual({ text: "未找到去人声模型，请在设置中下载", kind: "warning", action: "open-model-settings" });
    expect(devocalNotice(status({ phase: "unavailable", error: "engine_unavailable: spawn failed" })))
      .toEqual({ text: "去人声引擎无法启动", kind: "warning" });
    expect(devocalNotice(status({ phase: "unavailable", error: "engine_unavailable" }))).not.toHaveProperty("action");
    expect(devocalNotice(status({ phase: "failed", error: "engine_crashed" }))).not.toHaveProperty("action");
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

describe("devocalLabelKind", () => {
  const rows: [string, Partial<DevocalStatus>, "warning" | "neutral"][] = [
    ["sessionOverridden", { phase: "devocal", sessionOverridden: true }, "warning"],
    ["inputSilent", { phase: "passthrough", held: true, inputSilent: true }, "warning"],
    ["devocal with latency", { phase: "devocal", held: true, latencyMs: 45.4 }, "neutral"],
    ["devocal without latency", { phase: "devocal", held: true }, "neutral"],
    ["fallback overload", { phase: "fallback", held: true, fallbackReason: "overload" }, "warning"],
    ["fallback model_error", { phase: "fallback", held: true, fallbackReason: "model_error" }, "warning"],
    ["attaching", { phase: "attaching" }, "neutral"],
    ["waiting for a player", { phase: "attaching", waitingForPlayer: true }, "neutral"],
    ["attach given up", { phase: "failed", error: "attach_failed" }, "warning"],
    ["model load failed", { phase: "passthrough", held: true, error: "model_load_failed: x" }, "warning"],
    ["restarting", { phase: "restarting" }, "neutral"],
    ["failed", { phase: "failed" }, "warning"],
    ["unavailable, model missing", { phase: "unavailable", error: "model_not_found:stemgenrt-hop128" }, "warning"],
    ["unavailable, engine missing", { phase: "unavailable", error: "engine_unavailable: x" }, "warning"],
    ["unavailable, no error", { phase: "unavailable" }, "warning"],
    ["passthrough while held", { phase: "passthrough", held: true }, "neutral"],
  ];
  for (const [name, patch, kind] of rows)
    it(`${name} is ${kind}`, () => {
      expect(devocalLabelKind(status(patch))).toBe(kind);
      expect(devocalLabel(status(patch))).not.toBeNull();
    });
  it("has no kind when there is no label", () => {
    expect(devocalLabelKind(status({ phase: "off" }))).toBeNull();
    expect(devocalLabelKind(status({ phase: "passthrough", held: false }))).toBeNull();
    expect(devocalLabelKind(status({ phase: "fallback", fallbackReason: null }))).toBeNull();
    expect(devocalLabelKind(status({ phase: "releasing" }))).toBeNull();
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
      sessionOverridden: false, inputSilent: false, waitingForPlayer: false, error: null,
      modelId: "bytesep-mobilenet-1s", device: "gpu", deviceNote: null };
    expect(parseDevocalStatus(value)).toEqual(value);
    expect(parseDevocalStatus({ ...value, phase: "attaching", waitingForPlayer: true })?.waitingForPlayer).toBe(true);
  });
  it("reads a status without waitingForPlayer as not waiting", () => {
    expect(parseDevocalStatus({ phase: "attaching" })?.waitingForPlayer).toBe(false);
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

describe("approxLatency", () => {
  it("rounds to the nearest 5 ms", () => {
    expect(approxLatency(75.1)).toBe("约 75 ms");
    expect(approxLatency(72.4)).toBe("约 70 ms");
    expect(approxLatency(72.5)).toBe("约 75 ms");
    expect(approxLatency(0)).toBe("约 0 ms");
  });
});

describe("device fields", () => {
  it("parses model id, device and note, rejecting unknown values", () => {
    const parsed = parseDevocalStatus({ phase: "devocal", modelId: "bytesep-mobilenet-1s", device: "gpu", deviceNote: "gpu_overloaded" });
    expect(parsed).toMatchObject({ modelId: "bytesep-mobilenet-1s", device: "gpu", deviceNote: "gpu_overloaded" });
    expect(parseDevocalStatus({ phase: "devocal", modelId: 3, device: "tpu", deviceNote: "x" }))
      .toMatchObject({ modelId: null, device: null, deviceNote: null });
    expect(parseDevocalStatus({ phase: "off" })).toMatchObject({ modelId: null, device: null, deviceNote: null });
  });
});

describe("deviceLine", () => {
  const live = (patch: Partial<DevocalStatus>) => status({ phase: "devocal", held: true, ...patch });
  it("names the device in use", () => {
    expect(deviceLine(live({ device: "gpu" }), "bytesep-mobilenet-1s")).toBe("正在使用：GPU");
    expect(deviceLine(live({ device: "cpu" }), "bytesep-mobilenet-1s")).toBe("正在使用：CPU");
  });
  it("says so when auto fell back to the CPU", () => {
    expect(deviceLine(live({ device: "cpu", deviceNote: "gpu_unavailable" }), "bytesep-mobilenet-1s")).toBe("GPU 不可用，已改用 CPU");
    expect(deviceLine(live({ device: "cpu", deviceNote: "gpu_check_failed" }), "bytesep-mobilenet-1s")).toBe("GPU 不可用，已改用 CPU");
  });
  it("says a GPU-only model cannot run", () => {
    expect(deviceLine(status({ phase: "passthrough", held: true, error: "gpu_required: no gpu" }), "htdemucs-ft-vocals-1s"))
      .toBe("所选模型需要 GPU，当前不可用");
    expect(devocalLabel(status({ phase: "passthrough", held: true, error: "gpu_required: no gpu" }))).toBe("所选模型需要 GPU，当前不可用");
    expect(devocalLabelKind(status({ phase: "passthrough", held: true, error: "gpu_required: no gpu" }))).toBe("warning");
  });
  it("tells an overload switch to StemgenRT from one to the CPU", () => {
    expect(deviceLine(live({ device: "cpu", modelId: "stemgenrt-hop128", deviceNote: "gpu_overloaded" }), "htdemucs-ft-vocals-1s"))
      .toBe("GPU 负载过高，已改用 StemgenRT");
    expect(deviceLine(live({ device: "cpu", modelId: "bytesep-mobilenet-1s", deviceNote: "gpu_overloaded" }), "bytesep-mobilenet-1s"))
      .toBe("GPU 负载过高，已改用 CPU");
    expect(deviceLine(live({ device: "cpu", modelId: "stemgenrt-hop128", deviceNote: "gpu_overloaded" }), "stemgenrt-hop128"))
      .toBe("GPU 负载过高，已改用 CPU");
  });
  it("shows the model-load failure of an explicit GPU choice", () => {
    expect(deviceLine(status({ phase: "passthrough", held: true, error: "model_load_failed: dml" }), "bytesep-mobilenet-1s"))
      .toBe("去人声模型加载失败，已保持原声");
  });
  it("shows nothing before the engine reports", () => {
    expect(deviceLine(OFF_STATUS, "stemgenrt-hop128")).toBeNull();
  });
});
