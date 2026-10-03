import { useEffect, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { autotuneTarget, keyLabel, type AutoTuneTarget } from "./keyDetection";

const POLL_INTERVAL_MS = 1_000;

// One native read per second, single-flight, paused while hidden; a read that
// started before the window was hidden can never land as a fresh result.
// The key label and the Key/Scale recommendation share this one IPC poll.
export function useKeyDetectionSnapshot(sourceId: string | null, trackKey: string,
  targetGeneration: number | undefined): unknown {
  const [snapshot, setSnapshot] = useState<unknown>(null);
  const visibilityGeneration = useRef(0);
  const inFlight = useRef<{ promise: Promise<unknown>; visibilityGeneration: number } | null>(null);

  const read = () => {
    if (inFlight.current) return inFlight.current;
    const request = {
      visibilityGeneration: visibilityGeneration.current,
      promise: Promise.resolve<unknown>(null),
    };
    request.promise = invoke("get_key_detection").finally(() => {
      if (inFlight.current === request) inFlight.current = null;
    });
    inFlight.current = request;
    return request;
  };

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;

    const validIdentity = !!sourceId && !!trackKey && Number.isSafeInteger(targetGeneration)
      && Number(targetGeneration) >= 0;
    if (!validIdentity || !isTauri()) return () => { disposed = true; };

    const schedule = (delay = POLL_INTERVAL_MS) => {
      clearTimeout(timer);
      if (!disposed && !document.hidden) timer = setTimeout(() => void poll(), delay);
    };
    const poll = async () => {
      if (disposed || document.hidden) return;
      const request = read();
      try {
        const value = await request.promise;
        if (!disposed && !document.hidden
          && request.visibilityGeneration === visibilityGeneration.current) setSnapshot(value);
      } catch {
        if (!disposed && request.visibilityGeneration === visibilityGeneration.current) setSnapshot(null);
      } finally {
        schedule(request.visibilityGeneration === visibilityGeneration.current ? POLL_INTERVAL_MS : 0);
      }
    };
    const visibilityChanged = () => {
      clearTimeout(timer);
      if (document.hidden) visibilityGeneration.current++;
      else void poll();
    };
    document.addEventListener("visibilitychange", visibilityChanged);
    void poll();
    return () => {
      disposed = true;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [sourceId, trackKey, targetGeneration]);

  return snapshot;
}

/** The validated key label and Key/Scale recommendation for the current track. */
export function useAutoTuneTarget(sourceId: string | null, trackKey: string,
  targetGeneration: number | undefined): { keyLabel: string | null; target: AutoTuneTarget | null } {
  const snapshot = useKeyDetectionSnapshot(sourceId, trackKey, targetGeneration);
  const target = autotuneTarget(snapshot, sourceId, trackKey, targetGeneration);
  // Keep one object per (key, scale, candidate) so effects keyed on the target do not
  // re-run just because evidenceSeconds grew by another second.
  const stable = useRef<AutoTuneTarget | null>(null);
  const pair = (t: AutoTuneTarget) => `${t.key}/${t.scale}/${t.candidate?.key}/${t.candidate?.scale}`;
  if (!target) stable.current = null;
  else if (!stable.current || pair(stable.current) !== pair(target)) stable.current = target;
  return { keyLabel: keyLabel(snapshot, sourceId, trackKey, targetGeneration), target: stable.current };
}
