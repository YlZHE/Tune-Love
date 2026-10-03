import type { ControlState } from "./autotuneControl";
import { targetOptionLabels, titleLabel, type AutoTuneTarget, type OptionPair } from "./keyDetection";

export type { OptionPair };
// The last pair handed to the bridge for one (connection, track). A failed
// write stays remembered so the same pair is never retried automatically.
export type WrittenPair = OptionPair & {
  connectionId: string; trackKey: string; status: "pending" | "written" | "failed";
};

/** The pair to send now, or null when nothing should be written. */
export function nextWrite(prev: WrittenPair | null, target: AutoTuneTarget | null,
  state: ControlState, trackKey: string): OptionPair | null {
  if (!trackKey || state.phase !== "ready" || !state.connectionId
    || !state.capabilities.includes("scale")) return null;
  // A null target only means "no valid snapshot right now" (failed poll, generation
  // mismatch): never write on it. Rust reports Chromatic itself once a song exists.
  if (!target) return null;
  const pair = targetOptionLabels(target);
  if (pair.key !== null && !state.capabilities.includes("key")) return null;
  if (prev && prev.connectionId === state.connectionId && prev.trackKey === trackKey
    && prev.key === pair.key && prev.scale === pair.scale) return null;
  return pair;
}

const profileKeyNames = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/** Where the target stands for this connection: written/pending/failed only if the remembered
 * write is for exactly this target's pair. */
export function writeState(target: AutoTuneTarget, prev: WrittenPair | null,
  enabled: boolean): "off" | "idle" | WrittenPair["status"] {
  if (!enabled) return "off";
  const labels = targetOptionLabels(target);
  return prev && prev.key === labels.key && prev.scale === labels.scale ? prev.status : "idle";
}

// The uncertainty sentence of a Chromatic target; Major/Minor targets have none.
function uncertainty(target: AutoTuneTarget): string {
  const { candidate, uncoveredNotes } = target;
  if (target.scale !== "chromatic") return "";
  if (!candidate) return "刚开始分析，暂用 Chromatic。";
  const best = `排第一的候选是 ${titleLabel({ ...target, ...candidate })}。`;
  const [first, second] = uncoveredNotes.map(note => profileKeyNames[note]);
  if (first === undefined) return `暂用 Chromatic。${best}`;
  return second === undefined ? `拿不准：${first} 常用但不在候选调内，暂用 Chromatic。${best}`
    : `拿不准：${first} 与 ${second} 都常用，暂用 Chromatic。${best}`;
}

/** The title tooltip: evidence, uncertainty (Chromatic only), then the write status. */
export function targetTooltip(target: AutoTuneTarget, prev: WrittenPair | null, enabled: boolean): string {
  const evidence = target.source === "cache"
    ? `来自上次播放的分析结果（本次已分析 ${Math.round(target.evidenceSeconds)} 秒，足够后会重新确认）。`
    : `已分析 ${Math.round(target.evidenceSeconds)} 秒。`;
  const state = writeState(target, prev, enabled);
  const status = state === "off" ? "自动写入已关闭，可在设置中开启。"
    : state === "failed" ? "写入失败，不会自动重试。"
    : state === "written" ? "已写入当前连接的插件。"
    : "连接 Auto-Tune 后自动写入。";
  return `${evidence}${uncertainty(target)}${status}`;
}
