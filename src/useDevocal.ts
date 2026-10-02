import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { isDevocalActive, OFF_STATUS, parseDevocalStatus, type DevocalStatus } from "./devocal";

type DevocalAction = "enable" | "disable" | "release";

const POLL_MS = 500;

// Polls the backend status and sends toggle/release commands. At most one
// command is in flight; clicks made meanwhile are ignored. Commands reject so
// callers can tell the user; the polled status stays the source of truth.
export function useDevocal(): { status: DevocalStatus; enabled: boolean; toggle(): Promise<void>; release(): Promise<void> } {
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

  const send = useCallback(async (action: DevocalAction) => {
    if (inFlight.current) return;
    if (!isTauri()) throw new Error("请在桌面应用中使用去人声");
    inFlight.current = true;
    epoch.current++;
    if (action === "enable" && mounted.current) setEnabling(true);
    try {
      const next = parseDevocalStatus(await invoke("devocal_command", { request: { action } }));
      if (mounted.current && next) setStatus(next);
    } finally {
      epoch.current++;
      inFlight.current = false;
      if (mounted.current) setEnabling(false);
    }
  }, []);

  const toggle = useCallback(() => send(active.current ? "disable" : "enable"), [send]);
  const release = useCallback(() => send("release"), [send]);
  return { status, enabled: phaseActive || enabling, toggle, release };
}
