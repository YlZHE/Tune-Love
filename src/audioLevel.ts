import { invoke, isTauri } from "@tauri-apps/api/core";

export interface AudioLevel {
  sourceId: string | null;
  trackKey: string | null;
  status: "idle" | "resolving" | "capturing" | "unavailable";
  rms: number;
  peak: number;
  level: number;
  bands: [number, number, number];
  updatedAtMs: number;
}

export interface AudioIdentity { sourceId: string | null; trackKey: string; playing: boolean; visible: boolean }

function validatedSample(sample: unknown, identity: AudioIdentity, now: number): Partial<AudioLevel> | null {
  if (!identity.playing || !identity.visible || !identity.sourceId || !sample || typeof sample !== "object") return null;
  const value = sample as Partial<AudioLevel>;
  if (value.status !== "capturing" || value.sourceId !== identity.sourceId || value.trackKey !== identity.trackKey) return null;
  if (![value.rms, value.peak, value.level].every(number => typeof number === "number" && Number.isFinite(number) && number >= 0 && number <= 1)) return null;
  if (typeof value.updatedAtMs !== "number" || !Number.isFinite(value.updatedAtMs) || value.updatedAtMs <= 0 || !Number.isFinite(now)) return null;
  const age = now - value.updatedAtMs;
  return age >= -50 && age <= 400 ? value : null;
}

export function validAudioLevel(sample: unknown, identity: AudioIdentity, now: number): number {
  return validatedSample(sample, identity, now)?.level ?? 0;
}

export function validAudioBands(sample: unknown, identity: AudioIdentity, now: number): [number, number, number] {
  const bands = validatedSample(sample, identity, now)?.bands;
  if (!Array.isArray(bands) || bands.length !== 3 || !bands.every(v => typeof v === "number" && Number.isFinite(v) && v >= 0 && v <= 1)) return [0, 0, 0];
  return [bands[0], bands[1], bands[2]];
}

export function smoothAudioLevel(current: number, target: number, elapsedMs: number): number {
  const next = target + (current - target) * Math.exp(-Math.max(0, elapsedMs) / (target > current ? 14 : 32));
  return target === 0 && next < 0.001 ? 0 : next;
}

// Also serialize across StrictMode remounts and rapid pause/resume cycles.
let requestInFlight = false;
export async function readAudioLevel(): Promise<unknown> {
  if (!isTauri() || requestInFlight) return null;
  requestInFlight = true;
  try { return await invoke<AudioLevel>("get_audio_level"); }
  finally { requestInFlight = false; }
}
