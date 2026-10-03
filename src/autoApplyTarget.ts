import type { ControlState } from "./autotuneControl";
import { targetLabel, targetOptionLabels, type AutoTuneTarget, type OptionPair } from "./keyDetection";

export type { OptionPair };
// The last pair handed to the bridge for one (connection, track). A failed
// write stays remembered so the same pair is never retried automatically.
export type WrittenPair = OptionPair & {
  connectionId: string; trackKey: string; status: "pending" | "written" | "failed";
};

/** What the plugin should be set to: a target (or no evidence yet) as an option pair. Chromatic
 * degrades to the Major/Minor candidate when the plugin has no Chromatic option. */
function effectivePair(target: AutoTuneTarget | null, chromaticSupported: boolean): OptionPair | null {
  if (!target) return chromaticSupported ? { key: null, scale: "Chromatic" } : null;
  if (target.scale !== "chromatic" || chromaticSupported) return targetOptionLabels(target);
  return target.candidate ? targetOptionLabels({ ...target, ...target.candidate }) : null;
}

/** The pair to send now, or null when nothing should be written. */
export function nextWrite(prev: WrittenPair | null, target: AutoTuneTarget | null,
  state: ControlState, trackKey: string, chromaticSupported: boolean): OptionPair | null {
  if (!trackKey || state.phase !== "ready" || !state.connectionId
    || !state.capabilities.includes("scale")) return null;
  const pair = effectivePair(target, chromaticSupported);
  if (!pair || (pair.key !== null && !state.capabilities.includes("key"))) return null;
  if (prev && prev.connectionId === state.connectionId && prev.trackKey === trackKey
    && prev.key === pair.key && prev.scale === pair.scale) return null;
  return pair;
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
