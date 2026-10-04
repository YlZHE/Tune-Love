import { afterEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_DEVOCAL_MODEL, DEVOCAL_MODEL_STORAGE_KEY, parseDevocalModelPreference, qualityLatencyMs } from "./devocalModelPreferences";
import { MODEL_MANIFEST } from "./modelDownload";

const model = (id: string) => MODEL_MANIFEST.models.find(m => m.id === id)!;

describe("parseDevocalModelPreference", () => {
  it("defaults to StemgenRT on auto", () => {
    expect(DEFAULT_DEVOCAL_MODEL).toEqual({ modelId: "stemgenrt-hop128", device: "auto" });
    expect(parseDevocalModelPreference(null)).toEqual(DEFAULT_DEVOCAL_MODEL);
  });
  it("reads a saved selection", () => {
    expect(parseDevocalModelPreference(JSON.stringify({ modelId: "bytesep-mobilenet-1s", device: "gpu" })))
      .toEqual({ modelId: "bytesep-mobilenet-1s", device: "gpu" });
  });
  it("falls back to the default for garbage, per field", () => {
    expect(parseDevocalModelPreference("{nope")).toEqual(DEFAULT_DEVOCAL_MODEL);
    expect(parseDevocalModelPreference("42")).toEqual(DEFAULT_DEVOCAL_MODEL);
    expect(parseDevocalModelPreference(JSON.stringify({ modelId: "gone", device: "tpu" }))).toEqual(DEFAULT_DEVOCAL_MODEL);
    expect(parseDevocalModelPreference(JSON.stringify({ modelId: "htdemucs-ft-vocals-1s", device: "tpu" })))
      .toEqual({ modelId: "htdemucs-ft-vocals-1s", device: "auto" });
  });
});

describe("qualityLatencyMs", () => {
  it("uses the selected device, auto preferring the GPU entry", () => {
    expect(qualityLatencyMs(model("bytesep-mobilenet-1s"), "cpu")).toBe(360);
    expect(qualityLatencyMs(model("bytesep-mobilenet-1s"), "gpu")).toBe(230);
    expect(qualityLatencyMs(model("bytesep-mobilenet-1s"), "auto")).toBe(230);
  });
  it("falls back to the entry the model has", () => {
    expect(qualityLatencyMs(model("htdemucs-ft-vocals-1s"), "cpu")).toBe(230);
  });
});

describe("the shared store", () => {
  afterEach(() => { vi.unstubAllGlobals(); vi.resetModules(); });

  it("saves under its key and follows storage events from another window", async () => {
    const data = new Map<string, string>();
    let onStorage: (e: { key: string | null }) => void = () => {};
    vi.stubGlobal("localStorage", { getItem: (k: string) => data.get(k) ?? null, setItem: (k: string, v: string) => { data.set(k, v); } });
    vi.stubGlobal("window", { addEventListener: (_: string, fn: typeof onStorage) => { onStorage = fn; }, removeEventListener: () => {} });
    vi.resetModules();
    const store = await import("./devocalModelPreferences");
    expect(store.readDevocalModelPreference()).toEqual(DEFAULT_DEVOCAL_MODEL);
    store.setDevocalModelPreference({ modelId: "bytesep-mobilenet-1s", device: "cpu" });
    expect(JSON.parse(data.get(DEVOCAL_MODEL_STORAGE_KEY)!)).toEqual({ modelId: "bytesep-mobilenet-1s", device: "cpu" });
    const heard = vi.fn();
    store.subscribeDevocalModelPreference(heard);
    data.set(DEVOCAL_MODEL_STORAGE_KEY, JSON.stringify({ modelId: "htdemucs-ft-vocals-1s", device: "gpu" }));
    onStorage({ key: DEVOCAL_MODEL_STORAGE_KEY });
    expect(heard).toHaveBeenCalledTimes(1);
    expect(store.readDevocalModelPreference()).toEqual({ modelId: "htdemucs-ft-vocals-1s", device: "gpu" });
  });
});
