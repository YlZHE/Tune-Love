import { useState } from "react";
import { Switch } from "@radix-ui/themes";
import { Sparkle } from "@phosphor-icons/react";
import { setBackgroundEnabled, useBackgroundEnabled } from "../backgroundPreferences";

export function BackgroundSettings() {
  const enabled = useBackgroundEnabled();
  const [error, setError] = useState("");
  return <section className="background-settings" aria-labelledby="background-settings-title">
    <div className="color-heading"><Sparkle size={21} aria-hidden="true" />
      <div><h2 id="background-settings-title">背景</h2><p>让碎片随音乐轻轻流动。</p></div>
    </div>
    <div className="color-mode-row">
      <div><label htmlFor="audio-background-enabled">音频响应背景</label>
        <p>Aero Shards · 跟随当前播放器与界面配色。</p></div>
      <Switch id="audio-background-enabled" aria-label="音频响应背景" className="color-mode-switch" radius="full"
        checked={enabled} onCheckedChange={value => {
          try { setBackgroundEnabled(value); setError(""); }
          catch { setError("未能保存背景设置，请重试"); }
        }} />
    </div>
    <p className="background-note">暂停时静止，不支持动态效果的设备保留静态背景。</p>
    {error && <p role="alert" className="color-error">{error}</p>}
  </section>;
}
