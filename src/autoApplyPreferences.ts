import { useSyncExternalStore } from "react";

// "自动写入 Key/Scale": default off, persisted like the other helper-*-v1 prefs
// and synced to the other Tauri webview through the same-origin storage event.
export const AUTO_APPLY_STORAGE_KEY = "helper-auto-apply-v1";

export function parseAutoApplyPreference(raw: string | null): boolean {
  try { return JSON.parse(raw ?? "null")?.enabled === true; }
  catch { return false; }
}

function readPreference() {
  try { return parseAutoApplyPreference(localStorage.getItem(AUTO_APPLY_STORAGE_KEY)); }
  catch { return false; }
}
let enabled = readPreference();
const listeners = new Set<() => void>();
function subscribe(listener: () => void) {
  listeners.add(listener);
  const receive = (event: StorageEvent) => {
    if (event.key !== AUTO_APPLY_STORAGE_KEY && event.key !== null) return;
    enabled = readPreference(); listener();
  };
  window.addEventListener("storage", receive);
  return () => { listeners.delete(listener); window.removeEventListener("storage", receive); };
}
export function setAutoApplyEnabled(value: boolean) {
  localStorage.setItem(AUTO_APPLY_STORAGE_KEY, JSON.stringify({ enabled: value }));
  enabled = value;
  listeners.forEach(listener => listener());
}
export function useAutoApplyEnabled() {
  return useSyncExternalStore(subscribe, () => enabled);
}
