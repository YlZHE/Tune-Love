import type { CSSProperties } from "react";
import { useState } from "react";
import { Switch } from "@radix-ui/themes";
import { HexColorInput, HexColorPicker } from "react-colorful";
import { Check, Palette, VinylRecord } from "@phosphor-icons/react";
import { colord } from "colord";
import { setColorPreferences } from "../colorPreferences";
import { useAppColors } from "../useAppColors";
import type { NowPlaying } from "../nowPlaying";

const swatches = [
  ["薄荷", "#c9eee2"], ["天空", "#8cc8f4"], ["薰衣草", "#b7a2ed"], ["玫瑰", "#e6a1c4"],
  ["珊瑚", "#f4a58c"], ["琥珀", "#e8c078"], ["青柠", "#b8d88b"], ["银白", "#dce1e8"],
] as const;

export function ColorSettings({ track, colors }: { track: NowPlaying | null; colors: ReturnType<typeof useAppColors> }) {
  const artwork = track?.artworkDataUrl ?? null;
  const { preferences, primaryColor, style, coverColor, extracting } = colors;
  const automatic = preferences.mode === "cover";
  const [error, setError] = useState("");
  const [failedCover, setFailedCover] = useState<string | null>(null);
  const change = (patch: Parameters<typeof setColorPreferences>[0]) => {
    try { setColorPreferences(patch); setError(""); }
    catch { setError("未能保存颜色设置，请重试"); }
  };
  const status = !automatic ? "手动主色"
    : !artwork ? "暂无封面，暂用手动主色"
    : extracting ? "正在提取封面颜色…"
    : coverColor ? "已从当前封面提取" : "暂时无法取色，使用手动主色";

  return <section className="color-settings" style={style} aria-labelledby="color-settings-title">
    <div className="color-heading">
      <Palette size={21} aria-hidden="true" />
      <div><h2 id="color-settings-title">颜色</h2><p>选一种喜欢的颜色，或让界面跟随封面。</p></div>
    </div>
    <div className="color-mode-row">
      <div><label htmlFor="cover-color-mode">从封面提取主色</label><p>切歌时，自动跟随新封面的颜色。</p></div>
      <Switch id="cover-color-mode" aria-label="从封面提取主色" className="color-mode-switch" radius="full"
        checked={automatic} onCheckedChange={checked => change({ mode: checked ? "cover" : "manual" })} />
    </div>
    <div className="color-editor">
      {automatic ? <div className="cover-color-preview">
        {artwork && artwork !== failedCover
          ? <img src={artwork} alt="当前歌曲封面" onError={() => setFailedCover(artwork)} />
          : <VinylRecord size={72} weight="thin" aria-hidden="true" />}
        <span className="cover-color-caption">跟随封面</span>
      </div> : <div className="color-picker" aria-label="主色调色盘">
        <HexColorPicker color={preferences.manualColor} onChange={manualColor => change({ manualColor })} />
      </div>}
      <div className="color-details">
        {automatic ? <div className="cover-color-description">
          <h3>{track?.title || "等待音乐"}</h3>
          <p>{track?.artist || "播放音乐后，会在这里显示封面主色。"}</p>
          <p className="color-mode-note">自动优先选择彩色，黑白封面使用白色系。</p>
        </div> : <>
          <div className="color-swatches" role="group" aria-label="预设颜色">
            {swatches.map(([name, color]) => <button key={color} type="button" className="color-swatch"
              aria-label={name} aria-pressed={preferences.manualColor === color}
              style={{ "--swatch": color } as CSSProperties} onClick={() => change({ manualColor: color })}>
              {preferences.manualColor === color && <Check size={16} weight="bold" />}
            </button>)}
          </div>
          <label className="color-hex-label" htmlFor="manual-primary-color">主色值</label>
          <HexColorInput id="manual-primary-color" className="color-hex-input" aria-label="主色值" prefixed
            color={preferences.manualColor}
            onChange={manualColor => { if (manualColor.length === 7) change({ manualColor }); }}
            onBlur={event => {
              const value = colord(event.currentTarget.value);
              if (value.isValid()) change({ manualColor: value.toHex() });
            }} />
        </>}
        <div className="active-color" aria-label="当前主色">
          <span className="active-color-swatch" style={{ background: primaryColor }} />
          <div><span className="active-color-value">{primaryColor.toUpperCase()}</span><p role="status">{status}</p></div>
        </div>
      </div>
    </div>
    <p className="color-save-note">即时生效，自动保存</p>
    {error && <p className="color-error" role="alert">{error}</p>}
  </section>;
}
