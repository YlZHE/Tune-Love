import { useEffect, useReducer, useRef, useSyncExternalStore } from "react";
import { autoTune } from "./useAutoTune";
import { nextWrite, type WrittenPair } from "./autoApplyTarget";
import type { AutoTuneTarget } from "./keyDetection";

// Writes the current track's Key/Scale recommendation into the connected plugin once per
// (connection, track, pair) while the switch is on. Before the song has evidence that is
// Chromatic. A failure (e.g. a label the profile lacks) is reported once and never retried on its own.
export function useAutoApplyTarget(target: AutoTuneTarget | null, trackKey: string, enabled: boolean,
  onNotice: (message: string) => void): WrittenPair | null {
  // Share PlayerControls' status poll instead of starting a second refresh loop.
  const state = useSyncExternalStore(autoTune.subscribe, autoTune.getSnapshot);
  const written = useRef<WrittenPair | null>(null);
  const [, rerender] = useReducer((count: number) => count + 1, 0);

  if (!enabled || (written.current && (written.current.connectionId !== state.connectionId
    || written.current.trackKey !== trackKey))) written.current = null;

  useEffect(() => {
    if (!enabled) return;
    const pair = nextWrite(written.current, target, state, trackKey);
    if (!pair || !state.connectionId) return;
    const record: WrittenPair = { ...pair, connectionId: state.connectionId, trackKey, status: "pending" };
    written.current = record;
    rerender();
    // Both labels join one apply batch (see AutoTuneControl.queue).
    Promise.resolve()
      .then(() => Promise.all([pair.key === null ? undefined : autoTune.setDiscrete("key", pair.key),
        autoTune.setDiscrete("scale", pair.scale)]))
      .then(() => { record.status = "written"; })
      .catch(error => {
        record.status = "failed";
        onNotice(`未能自动写入 Key/Scale：${error instanceof Error ? error.message : "请检查连接"}`);
      })
      .finally(() => { if (written.current === record) rerender(); });
  }, [enabled, target, trackKey, state, onNotice]);

  return written.current;
}
