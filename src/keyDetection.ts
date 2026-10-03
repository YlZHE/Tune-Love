export type DetectedKey = { pitchClass: number; mode: "major" | "minor" };

// Scale-match recommendation. Chromatic means the song's key is not settled:
// key may be null, candidate is the best Major/Minor guess (shown, and used for
// plugins without Chromatic), uncoveredNotes are pitch classes the candidate
// scale would not cover. source "cache": remembered from an earlier play of this
// song, held until this play has enough evidence to overrule it.
export type TargetScale = "major" | "minor" | "chromatic";
export type Candidate = { key: number; scale: "major" | "minor" };
export type AutoTuneTarget = { key: number | null; scale: TargetScale; candidate: Candidate | null;
  uncoveredNotes: number[]; evidenceSeconds: number; source: "analysis" | "cache" };
/** Labels sent to the bridge; key null means write the scale only. */
export type OptionPair = { key: string | null; scale: "Major" | "Minor" | "Chromatic" };

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

function pitchClass(value: unknown): value is number {
  return Number.isInteger(value) && Number(value) >= 0 && Number(value) < 12;
}

/** The current song's Key/Scale recommendation, or null unless it belongs to this exact track. */
export function autotuneTarget(value: unknown, sourceId: string | null,
  trackKey: string, targetGeneration: number | undefined): AutoTuneTarget | null {
  if (!sourceId || !trackKey || !validGeneration(targetGeneration)) return null;
  const snapshot = record(value);
  if (!snapshot || snapshot.sourceId !== sourceId || snapshot.trackKey !== trackKey
    || snapshot.targetGeneration !== targetGeneration) return null;
  const target = record(snapshot.autotuneTarget);
  if (!target) return null;
  const { key, scale, evidenceSeconds: seconds, source, uncoveredNotes } = target;
  if (scale !== "major" && scale !== "minor" && scale !== "chromatic") return null;
  if (key === null ? scale !== "chromatic" : !pitchClass(key)) return null;
  const rawCandidate = target.candidate === null ? null : record(target.candidate);
  if (target.candidate !== null && (!rawCandidate || !pitchClass(rawCandidate.key)
    || (rawCandidate.scale !== "major" && rawCandidate.scale !== "minor"))) return null;
  if (!Array.isArray(uncoveredNotes) || !uncoveredNotes.every(pitchClass)
    || typeof seconds !== "number" || !Number.isFinite(seconds) || seconds < 0
    || (source !== "analysis" && source !== "cache")) return null;
  return { key: key as number | null, scale, candidate: rawCandidate && { key: Number(rawCandidate.key), scale: rawCandidate.scale as "major" | "minor" },
    uncoveredNotes: [...uncoveredNotes], evidenceSeconds: seconds, source };
}

/** Labels to send through the bridge: the profile's option labels. */
export function targetOptionLabels(target: AutoTuneTarget): OptionPair {
  return { key: target.key === null ? null : profileKeyLabels[target.key],
    scale: target.scale === "major" ? "Major" : target.scale === "minor" ? "Minor" : "Chromatic" };
}

/** "F Minor" / "F Chromatic" / "Chromatic", spelled like the profile options. */
export function titleLabel(target: AutoTuneTarget): string {
  const { key, scale } = targetOptionLabels(target);
  return key === null ? scale : `${key} ${scale}`;
}

export function targetLabel(target: AutoTuneTarget): string {
  return target.scale === "chromatic" || target.key === null ? titleLabel(target)
    : `${pitchNames[target.key]} ${target.scale === "major" ? "大调" : "小调"}`;
}

/** Same target for effect purposes: everything shown or written, but not evidenceSeconds growth. */
export function sameTarget(a: AutoTuneTarget, b: AutoTuneTarget): boolean {
  return a.key === b.key && a.scale === b.scale && a.source === b.source
    && a.candidate?.key === b.candidate?.key && a.candidate?.scale === b.candidate?.scale
    && a.uncoveredNotes.length === b.uncoveredNotes.length
    && a.uncoveredNotes.every((note, i) => note === b.uncoveredNotes[i]);
}
