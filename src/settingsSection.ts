import { useEffect, useState } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// Sent by `open_settings` to a settings window that is already open; a new window reads the
// same value from its `section` query parameter instead. The payload is a section name, or
// null when settings were opened plainly (from the settings button).
export const SETTINGS_SECTION_EVENT = "settings-section";

export type SettingsSection = "devocal-model";

// `section` null: a plain open. `seq` grows with every request, so asking for the same section
// again is still a change.
export type SettingsSectionRequest = { section: SettingsSection | null; seq: number };

export function parseSettingsSection(v: unknown): SettingsSection | null {
  return v === "devocal-model" ? v : null;
}

// The request after one `settings-section` event. A known section or null counts; anything else
// is ignored and leaves the previous request as it was.
export function nextSectionRequest(previous: SettingsSectionRequest | null, payload: unknown): SettingsSectionRequest | null {
  const section = payload === null ? null : parseSettingsSection(payload);
  if (payload !== null && section === null) return previous;
  return { section, seq: (previous?.seq ?? 0) + 1 };
}

// The latest request to open settings, from the address of a new window and then from events.
export function useSettingsSectionRequest(): SettingsSectionRequest | null {
  const [request, setRequest] = useState<SettingsSectionRequest | null>(() => {
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
      if (!disposed) setRequest(previous => nextSectionRequest(previous, event.payload));
    }).then(fn => { if (disposed) stop(fn); else unlisten = fn; }, () => { /* No event bridge: only the address counts. */ });
    return () => { disposed = true; if (unlisten) stop(unlisten); };
  }, []);
  return request;
}
