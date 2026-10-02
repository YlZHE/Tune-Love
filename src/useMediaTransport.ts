import { useEffect, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import type { NowPlaying } from "./nowPlaying";

export type TransportAction = "play" | "pause" | "previous" | "next";

const messages: Record<string, string> = {
  stale_target: "歌曲或播放器已切换，请重试",
  ambiguous_target: "发现多个相同播放器会话，暂时无法确定控制目标",
  unsupported: "当前播放器暂不支持此操作",
  unavailable: "播放器暂时不可用，请稍后重试",
  busy: "正在处理播放操作，请稍候",
  timeout: "播放器响应超时，请确认实际播放状态后重试",
  failed: "播放操作未成功，请重试",
};

export function useMediaTransport(track: NowPlaying | null, trackKey: string, targetGeneration: number | undefined, onNotice: (message: string) => void) {
  const [pending, setPending] = useState(false);
  const inFlight = useRef(false);
  const mounted = useRef(true);
  const identity = `${targetGeneration}\n${track?.transport?.sessionId ?? ""}\n${trackKey}`;
  const latestIdentity = useRef(identity);
  latestIdentity.current = identity;
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);

  async function send(action: TransportAction) {
    if (inFlight.current || !track?.transport || targetGeneration === undefined) return;
    if (!isTauri()) { onNotice("请在桌面应用中控制播放器"); return; }
    const capability = { play: "canPlay", pause: "canPause", previous: "canPrevious", next: "canNext" } as const;
    if (!track.transport[capability[action]]) return;
    inFlight.current = true;
    setPending(true);
    try {
      await invoke("control_media", { request: { action, sessionId: track.transport.sessionId,
        sourceId: track.sourceId, trackKey, targetGeneration } });
      // Playback and song visuals are driven only by real media snapshots.
    } catch (error) {
      if (mounted.current && latestIdentity.current === identity) {
        const code = error && typeof error === "object" && "code" in error ? String(error.code) : "failed";
        onNotice(messages[code] ?? messages.failed);
      }
    } finally {
      inFlight.current = false;
      if (mounted.current) setPending(false);
    }
  }
  return { pending, send };
}
