export type DetectedKey = { pitchClass: number; mode: "major" | "minor" };

// Scale-match recommendation: always Major/Minor (Chromatic barely corrects a
// voice in practice); null until the current song has any evidence.
// source "cache": remembered from an earlier play of this song, held until this
// play has enough evidence to overrule it.
export type AutoTuneTarget = { key: number; scale: "major" | "minor"; evidenceSeconds: number;
  source: "analysis" | "cache" };

export type DetectionSnapshot = {
  sourceId: string | null;
  trackKey: string | null;
  targetGeneration: number;
  status: "idle" | "analyzing" | "detected" | "unavailable";
  key: DetectedKey | null;
  updatedAtMs: number;
  autotuneTarget: AutoTuneTarget | null;
};

const pitchNames = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"] as const;

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}

function validGeneration(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) >= 0;
}

export function keyLabel(value: unknown, sourceId: string | null,
  trackKey: string, targetGeneration: number | undefined): string | null {
  if (!sourceId || !trackKey || !validGeneration(targetGeneration)) return null;
  const snapshot = record(value);
  if (!snapshot || snapshot.status !== "detected" || snapshot.sourceId !== sourceId
    || snapshot.trackKey !== trackKey || snapshot.targetGeneration !== targetGeneration
    || !Number.isSafeInteger(snapshot.updatedAtMs) || Number(snapshot.updatedAtMs) <= 0) return null;
  const key = record(snapshot.key);
  const pitchClass = key?.pitchClass;
  const mode = key?.mode;
  if (!Number.isInteger(pitchClass) || Number(pitchClass) < 0 || Number(pitchClass) >= pitchNames.length
    || (mode !== "major" && mode !== "minor")) return null;
  return `${pitchNames[Number(pitchClass)]} ${mode === "major" ? "大调" : "小调"}`;
}

// Profile option labels (reference/profiles/*.json use "C#"-style sharps).
const profileKeyLabels = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"] as const;

/** The current song's Key/Scale recommendation, or null unless it belongs to this exact track. */
export function autotuneTarget(value: unknown, sourceId: string | null,
  trackKey: string, targetGeneration: number | undefined): AutoTuneTarget | null {
  if (!sourceId || !trackKey || !validGeneration(targetGeneration)) return null;
  const snapshot = record(value);
  if (!snapshot || snapshot.sourceId !== sourceId || snapshot.trackKey !== trackKey
    || snapshot.targetGeneration !== targetGeneration) return null;
  const target = record(snapshot.autotuneTarget);
  const key = target?.key;
  const scale = target?.scale;
  const seconds = target?.evidenceSeconds;
  const source = target?.source;
  if (!Number.isInteger(key) || Number(key) < 0 || Number(key) >= 12
    || (scale !== "major" && scale !== "minor")
    || typeof seconds !== "number" || !Number.isFinite(seconds) || seconds < 0
    || (source !== "analysis" && source !== "cache")) return null;
  return { key: Number(key), scale, evidenceSeconds: seconds, source };
}

/** Labels to send through the bridge: the profile's option labels. */
export function targetOptionLabels(target: AutoTuneTarget): { key: string; scale: "Major" | "Minor" } {
  return { key: profileKeyLabels[target.key], scale: target.scale === "major" ? "Major" : "Minor" };
}

export function targetLabel(target: AutoTuneTarget): string {
  return `${pitchNames[target.key]} ${target.scale === "major" ? "大调" : "小调"}`;
}
