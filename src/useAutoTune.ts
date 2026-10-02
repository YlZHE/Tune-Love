import { useEffect, useSyncExternalStore } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { AutoTuneControl, initialControlState, type BridgeResponse } from "./autotuneControl";

export const autoTune = new AutoTuneControl(request => isTauri()
  ? invoke<BridgeResponse>("autotune_command", { request })
  : Promise.resolve({ ok: true, state: initialControlState, candidates: [], skippedCount: 0 }));

export function useAutoTune() {
  const state = useSyncExternalStore(autoTune.subscribe, autoTune.getSnapshot);
  useEffect(() => {
    if (!isTauri()) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try { await autoTune.refresh(); } catch { /* The shared state exposes the error. */ }
      if (active) timer = setTimeout(() => void poll(), 1000);
    };
    void poll();
    return () => { active = false; clearTimeout(timer); };
  }, []);
  return state;
}
