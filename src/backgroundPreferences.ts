import { useSyncExternalStore } from "react";

export const BACKGROUND_STORAGE_KEY = "helper-background-v1";

export function parseBackgroundPreference(raw: string | null): boolean {
  try { return JSON.parse(raw ?? "null")?.enabled === true; }
  catch { return false; }
}

function readPreference() {
  try { return parseBackgroundPreference(localStorage.getItem(BACKGROUND_STORAGE_KEY)); }
  catch { return false; }
}
let enabled = readPreference();
const listeners = new Set<() => void>();
function subscribe(listener: () => void) {
  listeners.add(listener);
  const receive = (event: StorageEvent) => {
    if (event.key !== BACKGROUND_STORAGE_KEY && event.key !== null) return;
    enabled = readPreference(); listener();
  };
  window.addEventListener("storage", receive);
  return () => { listeners.delete(listener); window.removeEventListener("storage", receive); };
}
export function setBackgroundEnabled(value: boolean) {
  localStorage.setItem(BACKGROUND_STORAGE_KEY, JSON.stringify({ enabled: value }));
  enabled = value;
  listeners.forEach(listener => listener());
}
export function useBackgroundEnabled() {
  return useSyncExternalStore(subscribe, () => enabled);
}
