export type DevocalPhase = "off" | "attaching" | "passthrough" | "devocal" | "fallback" | "releasing" | "restarting" | "failed" | "unavailable";

export interface DevocalStatus {
  phase: DevocalPhase;
  held: boolean;
  latencyMs: number | null;
  loadRatio: number | null;
  fallbackReason: "overload" | "model_error" | null;
  sessionOverridden: boolean;
  inputSilent: boolean;
  error: string | null;
}

export const OFF_STATUS: DevocalStatus = {
  phase: "off", held: false, latencyMs: null, loadRatio: null, fallbackReason: null,
  sessionOverridden: false, inputSilent: false, error: null,
};

const PHASES: readonly DevocalPhase[] = ["off", "attaching", "passthrough", "devocal", "fallback", "releasing", "restarting", "failed", "unavailable"];

// The toggle counts as "on" while the engine is taking over, running, or being restarted.
export function isDevocalActive(phase: DevocalPhase): boolean {
  return phase === "attaching" || phase === "devocal" || phase === "restarting";
}

const finiteOrNull = (value: unknown) => typeof value === "number" && Number.isFinite(value) ? value : null;

// IPC replies are untyped. A reply without a known phase (a stub, null, a newer
// backend) must not change what the user sees, so it is rejected, not defaulted.
export function parseDevocalStatus(value: unknown): DevocalStatus | null {
  if (!value || typeof value !== "object") return null;
  const raw = value as Record<string, unknown>;
  if (typeof raw.phase !== "string" || !PHASES.includes(raw.phase as DevocalPhase)) return null;
  return {
    phase: raw.phase as DevocalPhase,
    held: raw.held === true,
    latencyMs: finiteOrNull(raw.latencyMs),
    loadRatio: finiteOrNull(raw.loadRatio),
    fallbackReason: raw.fallbackReason === "overload" || raw.fallbackReason === "model_error" ? raw.fallbackReason : null,
    sessionOverridden: raw.sessionOverridden === true,
    inputSilent: raw.inputSilent === true,
    error: typeof raw.error === "string" ? raw.error : null,
  };
}

// "warning" labels need the user's attention while singing along and must stay
// visible; "neutral" ones are routine state that may sit in the hover controls.
export type DevocalLabelKind = "warning" | "neutral";
export interface DevocalNotice { text: string; kind: DevocalLabelKind }

const warning = (text: string): DevocalNotice => ({ text, kind: "warning" });
const neutral = (text: string): DevocalNotice => ({ text, kind: "neutral" });

// First matching row wins. null means "show nothing".
export function devocalNotice(s: DevocalStatus): DevocalNotice | null {
  if (s.sessionOverridden) return warning("播放器音量被调整，请调本应用音量");
  if (s.inputSilent) return warning("未录到播放器声音（可能是独占模式）");
  if (s.phase === "devocal") return neutral(s.latencyMs === null ? "去人声中" : `去人声中 · 延迟 ${Math.round(s.latencyMs)} ms`);
  if (s.phase === "fallback" && s.fallbackReason === "overload") return warning("性能不足，已退回原声");
  if (s.phase === "fallback" && s.fallbackReason === "model_error") return warning("去人声模型出错，已退回原声");
  if (s.phase === "attaching" || s.phase === "restarting") return neutral("正在接管播放器…");
  if (s.phase === "failed") return warning("去人声引擎多次异常，已保持原声");
  if (s.phase === "unavailable") return warning(s.error?.startsWith("engine_unavailable") ? "去人声引擎无法启动" : "未找到去人声模型");
  if (s.phase === "passthrough" && s.held) return neutral("原声直通");
  return null;
}

export const devocalLabel = (s: DevocalStatus): string | null => devocalNotice(s)?.text ?? null;
export const devocalLabelKind = (s: DevocalStatus): DevocalLabelKind | null => devocalNotice(s)?.kind ?? null;
