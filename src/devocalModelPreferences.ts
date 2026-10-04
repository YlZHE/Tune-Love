import { useSyncExternalStore } from "react";
import { MODEL_MANIFEST, STEMGENRT_ID, type ModelInfo } from "./modelDownload";

// The de-vocal model and compute device the user picked: persisted like the other
// preferences and synced to the other Tauri webview through the same-origin storage event.
export const DEVOCAL_MODEL_STORAGE_KEY = "tune-love.devocal-model";

export type DevocalDevice = "auto" | "cpu" | "gpu";
export interface DevocalModelPreference { modelId: string; device: DevocalDevice }
export const DEFAULT_DEVOCAL_MODEL: DevocalModelPreference = { modelId: STEMGENRT_ID, device: "auto" };

// Each field falls back to its default on its own, so a saved model survives a bad device.
export function parseDevocalModelPreference(raw: string | null): DevocalModelPreference {
  try {
    const v = JSON.parse(raw ?? "null");
    const modelId = typeof v?.modelId === "string" && MODEL_MANIFEST.models.some(m => m.id === v.modelId) ? v.modelId : DEFAULT_DEVOCAL_MODEL.modelId;
    const device = v?.device === "auto" || v?.device === "cpu" || v?.device === "gpu" ? v.device : DEFAULT_DEVOCAL_MODEL.device;
    return { modelId, device };
  } catch { return DEFAULT_DEVOCAL_MODEL; }
}

// Added latency to name in the quality-tier box: the selected device's entry, auto taking the
// GPU one when the model has it; a model without that entry shows the one it has.
export function qualityLatencyMs(model: ModelInfo, device: DevocalDevice): number {
  const { cpu, gpu } = model.devices;
  return ((device === "cpu" ? cpu ?? gpu : gpu ?? cpu))?.latencyMs ?? 0;
}

function load(): DevocalModelPreference {
  try { return parseDevocalModelPreference(localStorage.getItem(DEVOCAL_MODEL_STORAGE_KEY)); }
  catch { return DEFAULT_DEVOCAL_MODEL; }
}
let current = load();
const listeners = new Set<() => void>();

export function readDevocalModelPreference(): DevocalModelPreference { return current; }
export function subscribeDevocalModelPreference(listener: () => void) {
  listeners.add(listener);
  const receive = (event: StorageEvent) => {
    if (event.key !== DEVOCAL_MODEL_STORAGE_KEY && event.key !== null) return;
    current = load(); listener();
  };
  window.addEventListener("storage", receive);
  return () => { listeners.delete(listener); window.removeEventListener("storage", receive); };
}
export function setDevocalModelPreference(next: DevocalModelPreference) {
  localStorage.setItem(DEVOCAL_MODEL_STORAGE_KEY, JSON.stringify(next));
  current = next;
  listeners.forEach(listener => listener());
}
export function useDevocalModelPreference() {
  return useSyncExternalStore(subscribeDevocalModelPreference, readDevocalModelPreference);
}
