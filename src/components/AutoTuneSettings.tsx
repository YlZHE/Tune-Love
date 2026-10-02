import { useState } from "react";
import { Badge, Button, Select } from "@radix-ui/themes";
import { PlugsConnected } from "@phosphor-icons/react";
import { isTauri } from "@tauri-apps/api/core";
import { autoTune, useAutoTune } from "../useAutoTune";
import { controlLabel, type Candidate } from "../autotuneControl";
import "./AutoTuneSettings.css";

export function AutoTuneSettings() {
  const state = useAutoTune();
  const [candidates, setCandidates] = useState<Candidate[]>([]);
  const [selected, setSelected] = useState("");
  const [scanned, setScanned] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [skipped, setSkipped] = useState(0);
  const desktop = isTauri();
  const action = async (operation: () => Promise<unknown>) => {
    setBusy(true); setError("");
    try { await operation(); }
    catch (reason) { setError(reason instanceof Error ? reason.message : "操作失败，请重试"); }
    finally { setBusy(false); }
  };
  return <section className="autotune-settings" aria-labelledby="autotune-heading">
    <div className="color-heading">
      <PlugsConnected size={21} aria-hidden="true" />
      <div><h2 id="autotune-heading">Auto-Tune</h2><p>连接后，在悬浮窗调整电音参数。</p></div>
      <Badge color="gray" variant="soft">调试版</Badge>
    </div>
    <div className="autotune-connection" role="status">
      <span className="autotune-dot" data-ready={state.phase === "ready"} />
      <span>{controlLabel(state)}</span>
    </div>
    {state.target && <p className="autotune-target">{state.target.processName} · {state.target.pluginName} · {state.instanceCount} 个匹配实例</p>}
    <div className="autotune-actions">
      <Button variant="soft" color="gray" disabled={busy || !desktop} onClick={() => void action(async () => {
        const response = await autoTune.scan();
        const options = response.candidates ?? [];
        setCandidates(options); setSelected(""); setScanned(true); setSkipped(response.skippedCount ?? 0);
      })}>扫描插件</Button>
      <Select.Root value={selected} onValueChange={setSelected} disabled={busy || !candidates.length}>
        <Select.Trigger placeholder="选择加载了 Auto-Tune 的进程" aria-label="Auto-Tune 目标进程" />
        <Select.Content position="popper">
          {candidates.map(candidate => <Select.Item key={candidate.candidateId} value={candidate.candidateId} disabled={!candidate.compatible}>
            {candidate.processName} · {candidate.pluginName} · PID {candidate.pid}{!candidate.compatible ? "（版本待适配）" : ""}
          </Select.Item>)}
        </Select.Content>
      </Select.Root>
      <Button disabled={busy || !selected || !desktop} onClick={() => void action(() => autoTune.connect(selected))}>连接</Button>
      {state.connectionId && <Button color="gray" variant="ghost" disabled={busy} onClick={() => void action(() => autoTune.disconnect())}>停止控制</Button>}
      {state.connectionId && <Button color="gray" variant="ghost" disabled={busy || state.phase !== "ready"}
        onClick={() => void action(() => autoTune.clear())}>清空助手缓存</Button>}
    </div>
    {!desktop && <p className="autotune-note">浏览器仅展示界面，请在桌面调试版连接插件。</p>}
    {scanned && !candidates.length && <p className="autotune-note">未发现可用目标。请在测试宿主中加载 Auto-Tune 后重新扫描。</p>}
    {!!skipped && <p className="autotune-note">部分进程无法读取，扫描结果可能不完整。</p>}
    {candidates.filter(candidate => !candidate.compatible).map(candidate => <p className="autotune-note" key={candidate.candidateId}>
      {candidate.pluginName}：{candidate.reason || "此版本暂无经过核对的配置"}
    </p>)}
    <p className="autotune-note">控制所选进程中所有匹配实例；连接时保留插件现有参数。停止控制不会还原参数。</p>
    <p className="autotune-note">强度使用归一化百分比，不是毫秒；旋钮显示本次操作值，不是插件实时回读。</p>
    <p className="autotune-note">四参数调试已接入。调式自动推送、声音效果及工程同步仍待验收。</p>
    {(error || state.error) && <p className="autotune-error" role="alert">{error || state.error}</p>}
  </section>;
}
