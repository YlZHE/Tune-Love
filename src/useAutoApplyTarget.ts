import { useEffect, useReducer, useRef, useState, useSyncExternalStore } from "react";
import { autoTune } from "./useAutoTune";
import { nextWrite, type WrittenPair } from "./autoApplyTarget";
import type { AutoTuneTarget } from "./keyDetection";

// Writes the current track's Key/Scale recommendation into the connected plugin once per
// (connection, track, pair) while the switch is on. Before the song has evidence that is
// Chromatic; a plugin without a Chromatic option gets the Major/Minor candidate instead
// (and nothing when there is none). A failure is reported once and never retried on its own.
export function useAutoApplyTarget(target: AutoTuneTarget | null, trackKey: string, enabled: boolean,
  onNotice: (message: string) => void): WrittenPair | null {
  // Share PlayerControls' status poll instead of starting a second refresh loop.
  const state = useSyncExternalStore(autoTune.subscribe, autoTune.getSnapshot);
  const [chromatic, setChromatic] = useState<{ connectionId: string; supported: boolean } | null>(null);
  const noticed = useRef<string | null>(null);
  const written = useRef<WrittenPair | null>(null);
  const [, rerender] = useReducer((count: number) => count + 1, 0);

  if (!enabled || (written.current && (written.current.connectionId !== state.connectionId
    || written.current.trackKey !== trackKey))) written.current = null;

  // Whether the plugin offers Chromatic is asked once per connection; no write before the answer.
  useEffect(() => {
    if (!enabled || state.phase !== "ready" || !state.connectionId) return;
    const connectionId = state.connectionId;
    let live = true;
    void autoTune.supportsChromatic().then(supported => { if (live) setChromatic({ connectionId, supported }); });
    return () => { live = false; };
  }, [enabled, state.phase, state.connectionId]);
  const supported = chromatic?.connectionId === state.connectionId ? chromatic.supported : null;

  useEffect(() => {
    if (!enabled || supported === null) return;
    const pair = nextWrite(written.current, target, state, trackKey, supported);
    if (!pair || !state.connectionId) return;
    if (!supported && target?.scale === "chromatic" && noticed.current !== state.connectionId) {
      noticed.current = state.connectionId;
      onNotice("插件不支持 Chromatic，已改用候选调");
    }
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
  }, [enabled, supported, target, trackKey, state, onNotice]);

  return written.current;
}
