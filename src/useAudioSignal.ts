import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { readAudioLevel, validAudioBands } from "./audioLevel";

type SignalEvent = "sample" | "clear";
export interface AudioSignal {
  readBands(): [number, number, number];
  subscribe(listener: (event: SignalEvent) => void): () => void;
  reducedMotion: boolean;
}

// One validated sample stream per player window. Consumers own their renderers,
// but never create another IPC polling loop or change audio capture settings.
export function useAudioSignal(sourceId: string | null, trackKey: string, playing: boolean): AudioSignal {
  const sample = useRef<unknown>(null);
  const listeners = useRef(new Set<(event: SignalEvent) => void>());
  const identity = useRef({ sourceId, trackKey, playing });
  identity.current = { sourceId, trackKey, playing };
  const [reduced, setReduced] = useState(() => matchMedia("(prefers-reduced-motion: reduce)").matches);
  useEffect(() => {
    const query = matchMedia("(prefers-reduced-motion: reduce)");
    const update = () => setReduced(query.matches);
    query.addEventListener("change", update);
    return () => query.removeEventListener("change", update);
  }, []);
  const readBands = useCallback(() => validAudioBands(sample.current,
    { ...identity.current, visible: !document.hidden }, Date.now()), []);
  const subscribe = useCallback((listener: (event: SignalEvent) => void) => {
    listeners.current.add(listener);
    return () => { listeners.current.delete(listener); };
  }, []);

  useEffect(() => {
    let disposed = false, generation = 0, pending = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const clear = () => { sample.current = null; listeners.current.forEach(listener => listener("clear")); };
    clear();
    const canPoll = () => !disposed && playing && !!sourceId && !reduced && !document.hidden;
    const schedule = () => { if (canPoll()) timer = setTimeout(() => void poll(), 16); };
    const poll = async () => {
      if (!canPoll()) return;
      if (pending) { schedule(); return; }
      pending = true;
      const started = generation;
      try {
        const value = await readAudioLevel();
        if (canPoll() && started === generation) {
          sample.current = value;
          listeners.current.forEach(listener => listener("sample"));
        }
      } catch {
        // Notify loss once; repeated failures must not wake sleeping renderers.
        if (!disposed && started === generation && sample.current !== null) clear();
      } finally {
        pending = false;
        if (started === generation) schedule();
      }
    };
    const visibilityChanged = () => {
      generation++; clearTimeout(timer); clear();
      if (canPoll()) void poll();
    };
    document.addEventListener("visibilitychange", visibilityChanged);
    void poll();
    return () => {
      disposed = true; generation++; clearTimeout(timer); clear();
      document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [sourceId, trackKey, playing, reduced]);

  return useMemo(() => ({ readBands, subscribe, reducedMotion: reduced }), [readBands, subscribe, reduced]);
}
