import { useState } from "react";
import { Switch } from "@radix-ui/themes";
import { MusicNotes } from "@phosphor-icons/react";
import { setAutoApplyEnabled, useAutoApplyEnabled } from "../autoApplyPreferences";
import "./AutoApplySettings.css";

export function AutoApplySettings() {
  const enabled = useAutoApplyEnabled();
  const [error, setError] = useState("");
  return <section className="auto-apply-settings" aria-labelledby="auto-apply-settings-title">
    <div className="color-heading"><MusicNotes size={21} aria-hidden="true" />
      <div><h2 id="auto-apply-settings-title">调性</h2><p>把当前歌曲的调性分析结果交给 Auto-Tune。</p></div>
    </div>
    <div className="color-mode-row">
      <div><label htmlFor="auto-apply-key-scale">自动写入 Key/Scale</label>
        <p>已连接插件时，把当前的 Key/Scale 建议写入。</p></div>
      <Switch id="auto-apply-key-scale" aria-label="自动写入 Key/Scale" className="color-mode-switch" radius="full"
        checked={enabled} onCheckedChange={value => {
          try { setAutoApplyEnabled(value); setError(""); }
          catch { setError("未能保存自动写入设置，请重试"); }
        }} />
    </div>
    <p className="auto-apply-note">未连接时不写入。拿不准或刚开始时写入 Chromatic，确定后写入 Major/Minor，之后只在明显更合适时更换。写入失败不会自动重试。</p>
    {error && <p role="alert" className="color-error">{error}</p>}
  </section>;
}
