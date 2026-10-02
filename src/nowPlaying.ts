export type PlaybackStatus = "playing" | "paused" | "stopped" | "unknown";

export type TransportControls = {
  sessionId: string;
  canPlay: boolean;
  canPause: boolean;
  canPrevious: boolean;
  canNext: boolean;
};

export type NowPlaying = {
  title: string;
  artist: string;
  album: string;
  artworkDataUrl: string | null;
  positionSeconds: number;
  durationSeconds: number;
  playbackStatus: PlaybackStatus;
  source: string;
  sourceId: string;
  sourceIconDataUrl: string | null;
  updatedAtMs: number;
  playbackRate: number;
  transport?: TransportControls;
};

export type Snapshot = {
  status: "loading" | "ready" | "idle" | "unavailable";
  track: NowPlaying | null;
  capturedAtMs: number;
  targetGeneration?: number;
};

export function projectedPosition(track: NowPlaying, nowMs: number): number {
  if (!Number.isFinite(track.durationSeconds) || track.durationSeconds <= 0) return 0;
  const elapsed = track.playbackStatus === "playing"
    ? Math.min(8, Math.max(0, (nowMs - track.updatedAtMs) / 1000)) : 0;
  const rate = Number.isFinite(track.playbackRate) ? track.playbackRate : 1;
  const position = Number.isFinite(track.positionSeconds) ? track.positionSeconds : 0;
  return Math.min(track.durationSeconds, Math.max(0, position + elapsed * rate));
}

export function clampProgress(positionSeconds: number, durationSeconds: number): number {
  if (!Number.isFinite(durationSeconds) || durationSeconds <= 0) return 0;
  if (!Number.isFinite(positionSeconds)) return 0;
  return Math.min(1, Math.max(0, positionSeconds / durationSeconds));
}

export function formatClock(totalSeconds: number): string {
  const safeSeconds = Math.max(0, Math.floor(Number.isFinite(totalSeconds) ? totalSeconds : 0));
  const minutes = Math.floor(safeSeconds / 60);
  const seconds = safeSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

export function isPlaying(status: PlaybackStatus): boolean {
  return status === "playing";
}
