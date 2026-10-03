import { useCallback, useEffect, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { modelErrorText, parseModelStatuses, type ModelStatus } from "./modelDownload";

export type ModelAction = "download" | "cancel" | "import" | "delete";

const POLL_MS = 500;

// Polls the model statuses and sends download/cancel/import/delete commands. `statuses` is
// null until the first reply. A poll that began before a command finished may carry the
// pre-command state, so commands bump an epoch and stale poll results are dropped. Commands
// reject with a Chinese message so callers can show it; the polled status stays the truth.
// A command resolves to the statuses it returned (null if the reply was not understood).
export function useModelDownload(): {
  statuses: ModelStatus[] | null;
  send(id: string, action: ModelAction, options?: { autoEnable?: boolean; mirrorPrefix?: string }): Promise<ModelStatus[] | null>;
} {
  const [statuses, setStatuses] = useState<ModelStatus[] | null>(null);
  const mounted = useRef(true);
  const epoch = useRef(0);

  useEffect(() => {
    mounted.current = true;
    if (!isTauri()) return () => { mounted.current = false; };
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      const started = epoch.current;
      try {
        const next = parseModelStatuses(await invoke("get_model_status"));
        if (mounted.current && next && started === epoch.current) setStatuses(next);
      } catch { /* Keep the last known statuses; the next poll retries. */ }
      if (mounted.current) timer = setTimeout(() => void poll(), POLL_MS);
    };
    void poll();
    return () => { mounted.current = false; clearTimeout(timer); };
  }, []);

  const send = useCallback(async (id: string, action: ModelAction, options?: { autoEnable?: boolean; mirrorPrefix?: string }) => {
    if (!isTauri()) throw new Error("请在桌面应用中下载模型");
    epoch.current++;
    try {
      const next = parseModelStatuses(await invoke("model_command", {
        request: { id, action, autoEnable: options?.autoEnable ?? false, mirrorPrefix: options?.mirrorPrefix || null },
      }));
      if (mounted.current && next) setStatuses(next);
      return next;
    } catch (reason) {
      throw new Error(modelErrorText(String(reason)));
    } finally {
      epoch.current++;
    }
  }, []);

  return { statuses, send };
}
