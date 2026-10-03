import { useEffect, useState } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// Sent by `open_settings` to a settings window that is already open; a new window reads the
// same value from its `section` query parameter instead.
export const SETTINGS_SECTION_EVENT = "settings-section";

export type SettingsSection = "devocal-model";

export function parseSettingsSection(v: unknown): SettingsSection | null {
  return v === "devocal-model" ? v : null;
}

// The latest request to show a settings section. `seq` grows with every request, so asking
// for the same section again is still a change.
export function useSettingsSectionRequest(): { section: SettingsSection; seq: number } | null {
  const [request, setRequest] = useState(() => {
    const section = parseSettingsSection(new URLSearchParams(window.location.search).get("section"));
    return section ? { section, seq: 1 } : null;
  });
  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let unlisten: UnlistenFn | null = null;
    // Unlisten may throw or reject (no event bridge); either way there is nothing left to do.
    const stop = (fn: UnlistenFn) => { Promise.resolve().then(fn).catch(() => {}); };
    listen<unknown>(SETTINGS_SECTION_EVENT, event => {
      const section = parseSettingsSection(event.payload);
      if (section && !disposed) setRequest(previous => ({ section, seq: (previous?.seq ?? 0) + 1 }));
    }).then(fn => { if (disposed) stop(fn); else unlisten = fn; }, () => { /* No event bridge: only the address counts. */ });
    return () => { disposed = true; if (unlisten) stop(unlisten); };
  }, []);
  return request;
}
