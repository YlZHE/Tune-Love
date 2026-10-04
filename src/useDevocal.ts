import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { readDevocalModelPreference } from "./devocalModelPreferences";
import { isDevocalActive, OFF_STATUS, parseDevocalStatus, type DevocalStatus } from "./devocal";

type DevocalAction = "enable" | "disable" | "release";

const POLL_MS = 500;

// Polls the backend status and sends toggle/release commands. At most one
// command is in flight; clicks made meanwhile are ignored. Commands reject so
// callers can tell the user; the polled status stays the source of truth.
export function useDevocal(): { status: DevocalStatus; enabled: boolean; toggle(): Promise<void>; release(): Promise<void>; apply(): Promise<void> } {
  const [status, setStatus] = useState<DevocalStatus>(OFF_STATUS);
  const [enabling, setEnabling] = useState(false);
  const inFlight = useRef(false);
  const mounted = useRef(true);
  // A poll that began before a command finished may carry the pre-command state.
  const epoch = useRef(0);
  const active = useRef(false);
  const phaseActive = isDevocalActive(status.phase);
  active.current = phaseActive;

  useEffect(() => {
    mounted.current = true;
    if (!isTauri()) return () => { mounted.current = false; };
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      const started = epoch.current;
      try {
        const next = parseDevocalStatus(await invoke("get_devocal_status"));
        if (mounted.current && next && started === epoch.current) setStatus(next);
      } catch { /* Keep the last known status; the next poll retries. */ }
      if (mounted.current) timer = setTimeout(() => void poll(), POLL_MS);
    };
    void poll();
    return () => { mounted.current = false; clearTimeout(timer); };
  }, []);

  // The command in flight (its action and promise), for `apply` to queue behind.
  const flight = useRef<{ action: DevocalAction; done: Promise<void> } | null>(null);
  const trailing = useRef<Promise<void> | null>(null);
  const send = useCallback((action: DevocalAction): Promise<void> => {
    if (inFlight.current) return Promise.resolve();
    if (!isTauri()) return Promise.reject(new Error("请在桌面应用中使用去人声"));
    inFlight.current = true;
    epoch.current++;
    if (action === "enable" && mounted.current) setEnabling(true);
    const done = (async () => {
      try {
        const next = parseDevocalStatus(await invoke("devocal_command", { request: action === "enable" ? { action, ...readDevocalModelPreference() } : { action } }));
        if (mounted.current && next) setStatus(next);
      } finally {
        epoch.current++;
        inFlight.current = false;
        if (mounted.current) setEnabling(false);
      }
    })();
    flight.current = { action, done };
    return done;
  }, []);

  const toggle = useCallback(() => send(active.current ? "disable" : "enable"), [send]);
  // The saved model/device changed: while de-vocal is on, enable again so the backend swaps to the
  // new selection (it resends the model only when it really differs). Off: nothing to do.
  // Applies made while an enable is in flight coalesce into one trailing enable, sent when that
  // one settles with whatever is saved by then. Behind a disable/release they are dropped.
  const apply = useCallback((): Promise<void> => {
    if (!inFlight.current) return active.current ? send("enable") : Promise.resolve();
    const current = flight.current;
    if (!current || current.action !== "enable") return Promise.resolve();
    trailing.current ??= current.done.catch(() => {}).then(() => {
      trailing.current = null;
      return mounted.current ? send("enable") : undefined;
    });
    return trailing.current;
  }, [send]);
  const release = useCallback(() => send("release"), [send]);
  return { status, enabled: phaseActive || enabling, toggle, release, apply };
}
