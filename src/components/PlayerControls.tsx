import { useEffect, useId, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { IconButton, Popover } from "@radix-ui/themes";
import { Lightning, SlidersHorizontal, UserSound, SkipBack, SkipForward, Play, Pause } from "@phosphor-icons/react";
import { AppTooltip, WarmTooltipGroup } from "./AppTooltip";
import { ParameterDial } from "./ParameterDial";
import ElasticSlider from "./react-bits/ElasticSlider";
import type { TransportControls } from "../nowPlaying";
import type { TransportAction } from "../useMediaTransport";
import { autoTune, useAutoTune } from "../useAutoTune";
import { controlLabel, type ControlRole } from "../autotuneControl";
import "./AutoTuneSettings.css";
import "./PlayerControls.css";

type Panel = "strength" | "details" | null;

export function PlayerControls({ children, hovered, playing, hasTrack, transport, transportPending, onTransport, paletteStyle, onNotice, onReveal }: {
  children(visible: boolean): ReactNode;
  hovered: boolean; playing: boolean; hasTrack: boolean; songKey: string;
  transport?: TransportControls; transportPending: boolean; onTransport(action: TransportAction): Promise<void>;
  paletteStyle: CSSProperties; onNotice(message: string): void; onReveal(): void;
}) {
  const controls = useRef<HTMLDivElement>(null);
  const keyboard = useRef(true);
  const [focused, setFocused] = useState(false);
  const [touch, setTouch] = useState(() => matchMedia("(hover: none)").matches);
  const [panel, setPanel] = useState<Panel>(null);
  const [strength, setStrength] = useState(50);
  const [flex, setFlex] = useState(0);
  const [vibrato, setVibrato] = useState(0);
  const [humanize, setHumanize] = useState(0);
  const autoTuneState = useAutoTune();
  const changeParameter = (role: ControlRole, value: number, update: (value: number) => void) => {
    update(value);
    if (autoTuneState.phase !== "ready") { onNotice("参数预览：请先在设置中连接 Auto-Tune"); return; }
    void autoTune.setValue(role, value).catch(error => onNotice(error instanceof Error ? error.message : "参数未能发送"));
  };
  const [vocalRemovalEnabled, setVocalRemovalEnabled] = useState(false);
  const visible = hovered || focused || panel !== null || touch;
  const previousDisabled = !hasTrack || !transport?.canPrevious || transportPending;
  const nextDisabled = !hasTrack || !transport?.canNext || transportPending;
  const playbackDisabled = !hasTrack || !(playing ? transport?.canPause : transport?.canPlay) || transportPending;
  const previewId = useId();
  const strengthId = useId();
  const detailsId = useId();

  useEffect(() => {
    const key = () => {
      keyboard.current = true;
      if (controls.current?.contains(document.activeElement)) setFocused(true);
    };
    const pointer = () => { keyboard.current = false; };
    const query = matchMedia("(hover: none)");
    const update = () => setTouch(query.matches);
    document.addEventListener("keydown", key, true);
    document.addEventListener("pointerdown", pointer, true);
    query.addEventListener("change", update);
    return () => {
      document.removeEventListener("keydown", key, true);
      document.removeEventListener("pointerdown", pointer, true);
      query.removeEventListener("change", update);
    };
  }, []);
  useEffect(() => { if (visible) onReveal(); }, [visible, onReveal]);
  useEffect(() => {
    if (hovered || panel) return;
    if (!keyboard.current && controls.current?.contains(document.activeElement)) {
      (document.activeElement as HTMLElement).blur(); setFocused(false);
    }
  }, [hovered, panel]);

  const setOpen = (name: Exclude<Panel, null>, open: boolean) => setPanel(open ? name : null);
  const hint = (name: string) => `${name} · 前端预览`;
  return <footer className="player-footer" data-controls-visible={visible} data-panel-open={panel !== null}>
    <div className="footer-information" aria-hidden={visible} inert={visible}>{children(visible)}</div>
    <div ref={controls} className="footer-controls" role="group" aria-label="音乐和电音控制" aria-describedby={previewId}
      onFocusCapture={() => setFocused(keyboard.current)}
      onBlurCapture={event => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setFocused(false); }}>
      <div className="effect-controls">
        <Popover.Root open={panel === "strength"} onOpenChange={open => setOpen("strength", open)}>
          <AppTooltip content={autoTuneState.phase === "ready" ? "电音强度 · Retune 归一化百分比（非毫秒）" : "电音强度 · 未连接，预览模式"} side="top" disabled={panel !== null}>
            <Popover.Trigger><IconButton className="footer-control-button" size="1" variant="ghost" radius="full"
              aria-label="电音强度"><Lightning size={17} weight="fill" /></IconButton></Popover.Trigger>
          </AppTooltip>
          <Popover.Content className="control-popover" size="1" side="top" align="start" sideOffset={10}
            collisionPadding={{ top: 12, bottom: 12, left: 80, right: 80 }} style={paletteStyle}
            aria-label="电音强度" aria-describedby={strengthId}>
            <ElasticSlider value={strength} onChange={value => changeParameter("retune", value, setStrength)} ariaLabel="电音强度" startLabel="自然" endLabel="明显" />
            <span id={strengthId} className="controls-accessible-note">Retune 调试归一化百分比，不是毫秒。{controlLabel(autoTuneState)}</span>
          </Popover.Content>
        </Popover.Root>
        <Popover.Root open={panel === "details"} onOpenChange={open => setOpen("details", open)}>
          <AppTooltip content="详细参数" side="top" disabled={panel !== null}>
            <Popover.Trigger><IconButton className="footer-control-button" size="1" variant="ghost" radius="full"
              aria-label="详细参数"><SlidersHorizontal size={17} /></IconButton></Popover.Trigger>
          </AppTooltip>
          <Popover.Content className="control-details" size="1" side="top" align="start" sideOffset={10}
            collisionPadding={12} style={paletteStyle} aria-label="Auto-Tune 参数" aria-describedby={detailsId}>
            <WarmTooltipGroup lean={10}>
              <div className="parameter-dial-grid">
                <ParameterDial label="Flex-Tune" value={flex} onChange={value => changeParameter("flex", value, setFlex)}
                  description="保留滑音和音高变化，数值越大，演唱表达越自由。" />
                <ParameterDial label="Natural Vibrato" value={vibrato} onChange={value => changeParameter("vibrato", value, setVibrato)} min={-12} max={12} step={0.1} progressOrigin={0}
                  description="减弱或增强人声原有的颤音。" />
                <ParameterDial label="Humanize" value={humanize} onChange={value => changeParameter("humanize", value, setHumanize)}
                  description="让长音更自然，减少快速修音带来的僵硬感。" />
              </div>
            </WarmTooltipGroup>
            <span id={detailsId} className="control-connection-note">{controlLabel(autoTuneState)}</span>
          </Popover.Content>
        </Popover.Root>
        <AppTooltip content={hint(vocalRemovalEnabled ? "关闭去人声，恢复原唱" : "开启去人声")} side="top" disabled={panel !== null}>
          <IconButton size="1" variant="ghost" radius="full" className={`footer-control-button vocal-removal-control ${vocalRemovalEnabled ? "is-enabled" : ""}`}
            aria-label={vocalRemovalEnabled ? "关闭去人声" : "开启去人声"} aria-pressed={vocalRemovalEnabled} onClick={() => {
              setVocalRemovalEnabled(value => !value); onNotice("前端预览：去人声处理尚未接入，实际音频不变");
            }}><UserSound size={17} weight={vocalRemovalEnabled ? "fill" : "regular"} /></IconButton>
        </AppTooltip>
      </div>
      <div className="transport-controls" aria-busy={transportPending}>
        <AppTooltip content="上一首" side="top" disabled={previousDisabled || panel !== null}>
          <IconButton size="1" variant="ghost" radius="full" className="footer-control-button" aria-label="上一首" disabled={previousDisabled}
            onClick={() => void onTransport("previous")}><SkipBack size={16} weight="fill" /></IconButton>
        </AppTooltip>
        <AppTooltip content={playing ? "暂停" : "播放"} side="top" disabled={playbackDisabled || panel !== null}>
          <IconButton size="1" variant="ghost" radius="full" className="footer-control-button transport-primary"
            aria-label={playing ? "暂停" : "播放"} disabled={playbackDisabled} onClick={() => void onTransport(playing ? "pause" : "play")}
            >{playing ? <Pause size={17} weight="fill" /> : <Play size={17} weight="fill" />}</IconButton>
        </AppTooltip>
        <AppTooltip content="下一首" side="top" disabled={nextDisabled || panel !== null}>
          <IconButton size="1" variant="ghost" radius="full" className="footer-control-button" aria-label="下一首" disabled={nextDisabled}
            onClick={() => void onTransport("next")}><SkipForward size={16} weight="fill" /></IconButton>
        </AppTooltip>
      </div>
    </div>
    <span id={previewId} className="controls-accessible-note">播放按钮控制当前播放器；{controlLabel(autoTuneState)}；去人声仍为前端预览。旋钮显示最近操作值，不代表插件实时回读。</span>
  </footer>;
}
