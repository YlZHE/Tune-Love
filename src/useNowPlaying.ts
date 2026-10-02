import { useEffect, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import type { Snapshot } from "./nowPlaying";

export function useNowPlaying() {
  const [snapshot, setSnapshot] = useState<Snapshot>({
    status: "loading", track: null, capturedAtMs: Date.now(),
  });
  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      try {
        const value = isTauri()
          ? await invoke<Snapshot>("get_now_playing")
          : { status: "idle", track: null, capturedAtMs: Date.now() } as Snapshot;
        if (!disposed) setSnapshot(value);
      } catch {
        if (!disposed) setSnapshot({ status: "unavailable", track: null, capturedAtMs: Date.now() });
      }
      if (!disposed) timer = setTimeout(poll, 700);
    };
    void poll();
    return () => { disposed = true; clearTimeout(timer); };
  }, []);
  return snapshot;
}
