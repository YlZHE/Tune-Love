import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Theme, IconButton, Text } from "@radix-ui/themes";
import { MusicNotes, PushPin, X, Pause } from "@phosphor-icons/react";
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { AnimatedCounter } from "@/components/ui/animated-counter";
import { TrackArtwork, TrackMetadata } from "./components/TrackTransition";
import { SettingsButton } from "./components/SettingsButton";
import { SourceIcon } from "./components/SourceIcon";
import { AudioStrands } from "./components/AudioStrands";
import { AudioBackground } from "./components/AudioBackground";
import { useAudioSignal } from "./useAudioSignal";
import { useBackgroundEnabled } from "./backgroundPreferences";
import { PlaybackLabel } from "./components/PlaybackLabel";
import { PlaybackProgress } from "./components/PlaybackProgress";
import { PlayerControls } from "./components/PlayerControls";
import { AppTooltip, WarmTooltipGroup, type WarmTooltipGroupHandle } from "./components/AppTooltip";
import { useNowPlaying } from "./useNowPlaying";
import { useMediaTransport } from "./useMediaTransport";
import { formatClock, projectedPosition } from "./nowPlaying";
import { useAppColors } from "./useAppColors";
import { useAutoTuneTarget } from "./useAutoTuneTarget";
import { useAutoApplyTarget } from "./useAutoApplyTarget";
import { useAutoApplyEnabled } from "./autoApplyPreferences";
import { KeyTitle } from "./components/KeyTitle";

function WindowButton({ label, children, active, onClick }: {
  label: string; children: ReactNode; active?: boolean; onClick: () => void;
}) {
  return <AppTooltip content={label}>
    <IconButton size="1" variant="ghost" color="gray" radius="full"
      className={`window-button ${active ? "is-active" : ""}`} aria-label={label}
      aria-pressed={active} onClick={onClick}>{children}</IconButton>
  </AppTooltip>;
}

function Clock({ seconds }: { seconds: number }) {
  const value = Math.max(0, Math.floor(seconds));
  const [reduced, setReduced] = useState(false);
  useEffect(() => {
    const query = matchMedia("(prefers-reduced-motion: reduce)");
    const update = () => setReduced(query.matches);
    update(); query.addEventListener("change", update);
    return () => query.removeEventListener("change", update);
  }, []);
  return <span className="clock" aria-label={formatClock(value)}>
    {reduced ? formatClock(value) : <span aria-hidden="true" className="clock-digits">
      <AnimatedCounter value={Math.floor(value / 60)} separator="" duration={0.28} />
      <span className="clock-colon">:</span>
      <AnimatedCounter value={value % 60} padStart={2} separator="" duration={0.28} />
    </span>}
  </span>;
}

export function App() {
  const tooltipGroup = useRef<WarmTooltipGroupHandle>(null);
  const snapshot = useNowPlaying();
  const [now, setNow] = useState(Date.now());
  const [pinned, setPinned] = useState(true);
  const [notice, setNotice] = useState("");
  const [hovered, setHovered] = useState(false);
  const resetTooltip = useCallback(() => tooltipGroup.current?.reset(), []);
  const [failedCover, setFailedCover] = useState<string | null>(null);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 250);
    if (isTauri()) void getCurrentWindow().isAlwaysOnTop().then(setPinned).catch(() => {});
    return () => clearInterval(timer);
  }, []);
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(""), 3500);
    return () => clearTimeout(timer);
  }, [notice]);

  const stale = now - snapshot.capturedAtMs > 8000;
  const status = stale ? "unavailable" : snapshot.status;
  const track = status === "ready" ? snapshot.track : null;
  // Progress, playback state and delayed artwork do not identify a new song.
  const songKey = track ? JSON.stringify([track.sourceId ?? track.source, track.title, track.artist, track.album]) : "empty";
  const { target, evidenceSeconds } = useAutoTuneTarget(track?.sourceId ?? null, songKey,
    track ? snapshot.targetGeneration : undefined);
  const autoApplyEnabled = useAutoApplyEnabled();
  const written = useAutoApplyTarget(target, track ? songKey : "", autoApplyEnabled, setNotice);
  const previousSongKey = useRef(songKey);
  useEffect(() => {
    if (previousSongKey.current === songKey) return;
    previousSongKey.current = songKey;
    tooltipGroup.current?.reset();
  }, [songKey]);
  const playing = track?.playbackStatus === "playing";
  const transport = useMediaTransport(track, songKey, snapshot.targetGeneration, setNotice);
  const audio = useAudioSignal(track?.sourceId ?? null, songKey, playing);
  const backgroundEnabled = useBackgroundEnabled();
  const position = track ? projectedPosition(track, now) : 0;
  const duration = track?.durationSeconds ?? 0;
  const cover = track?.artworkDataUrl && track.artworkDataUrl !== failedCover ? track.artworkDataUrl : null;
  const colors = useAppColors(cover);
  const loading = status === "loading";
  const unavailable = status === "unavailable";
  const emptyTitle = unavailable ? "暂时无法读取音乐" : "等一首好听的歌";
  const emptyDescription = unavailable ? "正在重新连接，请稍候" : "播放音乐，歌曲信息会自动出现在这里";
  const playbackLabel = playing ? "正在播放" : track?.playbackStatus === "paused" ? "已暂停"
    : track?.playbackStatus === "stopped" ? "已停止" : "等待播放";

  async function togglePin() {
    try {
      if (!isTauri()) { setNotice("请在桌面应用中使用置顶功能"); return; }
      await getCurrentWindow().setAlwaysOnTop(!pinned);
      setPinned(!pinned);
    } catch { setNotice("未能更改置顶状态，请重试"); }
  }
  async function close() {
    try {
      if (isTauri()) await getCurrentWindow().close();
      else setNotice("浏览器预览，请直接关闭此标签页");
    } catch { setNotice("未能关闭窗口，请重试"); }
  }

  return <Theme appearance="dark" accentColor="jade" grayColor="gray" radius="large" scaling="100%">
    <WarmTooltipGroup ref={tooltipGroup} delay={400} warmWindow={300} travel={320} lean={10}>
    <main className="music-window" data-theme="dark" style={colors.style}
      onPointerOver={event => {
        // Portal events also bubble through React parents; only the real card is hovered.
        if (event.pointerType !== "touch" && event.currentTarget.contains(event.target as Node)) setHovered(true);
      }}
      onPointerOut={event => {
        if (event.pointerType !== "touch" && (!(event.relatedTarget instanceof Node)
          || !event.currentTarget.contains(event.relatedTarget))) setHovered(false);
      }}
      onPointerDownCapture={event => { if (event.pointerType === "touch") setHovered(true); }}
      data-color-mode={colors.preferences.mode} data-primary-color={colors.primaryColor}>
      <div className="color-atmosphere" aria-hidden="true" />
      {backgroundEnabled && <AudioBackground signal={audio} palette={colors.palette} />}
      <header className="titlebar">
        <div className="brand" data-tauri-drag-region>
          <MusicNotes size={17} weight="bold" aria-hidden="true" />
          <KeyTitle target={target} evidenceSeconds={evidenceSeconds} written={written} enabled={autoApplyEnabled} />
        </div>
        <div className="window-actions">
          <WindowButton label={pinned ? "取消置顶" : "窗口置顶"} active={pinned} onClick={() => void togglePin()}>
            <PushPin size={15} weight={pinned ? "fill" : "regular"} />
          </WindowButton>
          <SettingsButton onError={setNotice} />
          <WindowButton label="关闭" onClick={() => void close()}><X size={16} /></WindowButton>
        </div>
      </header>

      <section className={`music-content ${track ? "has-track" : "is-empty"}`}>
        <TrackArtwork songKey={songKey} cover={cover} title={track?.title ?? "歌曲"}
          hasTrack={!!track} loading={loading} onCoverError={setFailedCover} />

        <div className="track-details">
        <TrackMetadata songKey={songKey} title={track?.title || emptyTitle}
          artist={track ? track.artist || "未提供歌手信息" : emptyDescription}
          hasTrack={!!track} loading={loading} />

        <div className="timeline">
          <PlaybackProgress key={songKey} position={position} duration={duration}
            playing={playing} hasTrack={!!track} />
          <div className="time-labels">
            {track && duration > 0 ? <><Clock seconds={position} /><span>{formatClock(duration)}</span></>
              : <><span>--:--</span><span>--:--</span></>}
          </div>
        </div>
      <PlayerControls hovered={hovered} playing={playing} hasTrack={!!track} songKey={songKey}
        transport={snapshot.targetGeneration !== undefined ? track?.transport : undefined} transportPending={transport.pending} onTransport={transport.send}
        paletteStyle={colors.style} onNotice={setNotice} onReveal={resetTooltip}>
        {controlsVisible => <><AppTooltip content={track?.source} side="top" disabled={controlsVisible || !track?.source}
          className="source-tooltip-trigger">
        <div className="source" tabIndex={track?.source ? 0 : undefined}>
          <SourceIcon key={track?.sourceId ?? track?.source} dataUrl={track?.sourceIconDataUrl} />
          <Text size="1" truncate>{track?.source || (isTauri() ? "自动发现音乐" : "界面预览")}</Text>
        </div>
        </AppTooltip>
        <div className={`playback-state ${playing ? "is-playing" : ""}`}>
          {playing ? <AudioStrands signal={audio} /> : track ? <Pause size={13} aria-hidden="true" /> : <span className="status-dot" />}
          <PlaybackLabel text={loading ? "正在连接" : unavailable ? "重连中" : playbackLabel}
            playing={playing && !loading && !unavailable} />
        </div></>}
      </PlayerControls>
        </div>
      </section>
      {notice && <div className="notice" role="status">{notice}</div>}
    </main>
    </WarmTooltipGroup>
  </Theme>;
}
