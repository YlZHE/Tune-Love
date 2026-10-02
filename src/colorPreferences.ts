import { useSyncExternalStore } from "react";

export type ColorPreferences = { mode: "manual" | "cover"; manualColor: string };
export const COLOR_STORAGE_KEY = "helper-colors-v1";
export const DEFAULT_COLORS: ColorPreferences = { mode: "manual", manualColor: "#c9eee2" };

export function parseColorPreferences(raw: string | null): ColorPreferences {
  try {
    const value = JSON.parse(raw ?? "null");
    return {
      mode: value?.mode === "cover" ? "cover" : "manual",
      manualColor: typeof value?.manualColor === "string" && /^#[0-9a-f]{6}$/i.test(value.manualColor)
        ? value.manualColor.toLowerCase() : DEFAULT_COLORS.manualColor,
    };
  } catch { return DEFAULT_COLORS; }
}

function readPreferences() {
  try { return parseColorPreferences(localStorage.getItem(COLOR_STORAGE_KEY)); }
  catch { return DEFAULT_COLORS; }
}
let current = readPreferences();
const listeners = new Set<() => void>();

function subscribe(listener: () => void) {
  listeners.add(listener);
  const receive = (event: StorageEvent) => {
    if (event.key !== COLOR_STORAGE_KEY && event.key !== null) return;
    current = readPreferences();
    listener();
  };
  window.addEventListener("storage", receive);
  return () => { listeners.delete(listener); window.removeEventListener("storage", receive); };
}

export function setColorPreferences(patch: Partial<ColorPreferences>) {
  const next = parseColorPreferences(JSON.stringify({ ...current, ...patch }));
  // Persist before announcing success. The same-origin storage event updates the
  // other Tauri webview, while subscribers update this window immediately.
  localStorage.setItem(COLOR_STORAGE_KEY, JSON.stringify(next));
  current = next;
  listeners.forEach(listener => listener());
}

export function useColorPreferences() {
  return useSyncExternalStore(subscribe, () => current);
}
