import { invoke, isTauri } from "@tauri-apps/api/core";
import type { SettingsSection } from "./settingsSection";

let preview: Window | null = null;

// Opens (or brings forward) the settings window, optionally at one section. Outside Tauri
// the settings view opens as a same-origin preview popup.
export async function openSettings(section?: SettingsSection): Promise<void> {
  if (isTauri()) {
    await invoke("open_settings", section ? { section } : {});
    return;
  }
  const url = section ? `/?view=settings&section=${section}` : "/?view=settings";
  // An open preview is reused as is, unless a section was asked for: then it is reloaded there.
  if (!preview || preview.closed || section) {
    preview = window.open(url, "tune-love-settings", "popup,width=760,height=540");
  }
  if (!preview) throw new Error("Preview window was blocked");
  preview.focus();
}
