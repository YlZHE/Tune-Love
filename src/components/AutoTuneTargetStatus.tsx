import { AppTooltip } from "./AppTooltip";
import { targetStatusLabel, type WrittenPair } from "../autoApplyTarget";
import type { AutoTuneTarget } from "../keyDetection";
import "./AutoTuneTargetStatus.css";

// Secondary titlebar text: "建议 F♯ 小调 · 已写入". Rendered only while the
// current track has a validated recommendation.
export function AutoTuneTargetStatus({ target, written, enabled }: {
  target: AutoTuneTarget | null; written: WrittenPair | null; enabled: boolean;
}) {
  if (!target) return null;
  const label = targetStatusLabel(target, written, enabled);
  const evidence = target.source === "cache"
    ? `来自上次播放的分析结果（本次已分析 ${Math.round(target.evidenceSeconds)} 秒，足够后会重新确认）。`
    : `已分析 ${Math.round(target.evidenceSeconds)} 秒。`;
  const detail = !enabled ? `${evidence}自动写入已关闭，可在设置中开启。`
    : written?.status === "failed" ? `${evidence}写入失败，不会自动重试。`
    : written?.status === "written" ? `${evidence}已写入当前连接的插件。`
    : `${evidence}连接 Auto-Tune 后自动写入。`;
  return <AppTooltip content={detail} className="autotune-target-trigger">
    <span className="autotune-target-status" tabIndex={0}>{label}</span>
  </AppTooltip>;
}
