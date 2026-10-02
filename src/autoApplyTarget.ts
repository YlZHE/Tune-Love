import type { ControlState } from "./autotuneControl";
import { targetLabel, targetOptionLabels, type AutoTuneTarget } from "./keyDetection";

export type OptionPair = { key: string; scale: "Major" | "Minor" };
// The last pair handed to the bridge for one (connection, track). A failed
// write stays remembered so the same pair is never retried automatically.
export type WrittenPair = OptionPair & {
  connectionId: string; trackKey: string; status: "pending" | "written" | "failed";
};

/** The pair to send now, or null when nothing should be written. */
export function nextWrite(prev: WrittenPair | null, target: AutoTuneTarget | null,
  state: ControlState, trackKey: string): OptionPair | null {
  if (!trackKey || state.phase !== "ready" || !state.connectionId
    || !state.capabilities.includes("key") || !state.capabilities.includes("scale")) return null;
  // No target means the song has no evidence yet; nothing is written. Chromatic
  // is never recommended (it barely corrects a voice in practice).
  const labels = target && targetOptionLabels(target);
  if (!labels) return null;
  if (prev && prev.connectionId === state.connectionId && prev.trackKey === trackKey
    && prev.key === labels.key && prev.scale === labels.scale) return null;
  return labels;
}

export function matchesTarget(prev: WrittenPair | null, target: AutoTuneTarget): prev is WrittenPair {
  const labels = targetOptionLabels(target);
  return !!prev && prev.key === labels.key && prev.scale === labels.scale;
}

export function targetStatusLabel(target: AutoTuneTarget, prev: WrittenPair | null, enabled: boolean): string {
  const status = !enabled || !matchesTarget(prev, target) ? "未写入"
    : prev.status === "written" ? "已写入" : prev.status === "pending" ? "写入中" : "写入失败";
  return `建议 ${targetLabel(target)} · ${status}`;
}
