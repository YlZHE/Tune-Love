import { useCallback, useEffect, useState } from "react";
import { IconButton, Theme } from "@radix-ui/themes";
import { X } from "@phosphor-icons/react";
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { AppTooltip, WarmTooltipGroup } from "./components/AppTooltip";
import { ColorSettings } from "./components/ColorSettings";
import { BackgroundSettings } from "./components/BackgroundSettings";
import { AutoTuneSettings } from "./components/AutoTuneSettings";
import { AutoApplySettings } from "./components/AutoApplySettings";
import { useNowPlaying } from "./useNowPlaying";
import { useAppColors } from "./useAppColors";

export function SettingsPage() {
  const snapshot = useNowPlaying();
  const track = snapshot.status === "ready" ? snapshot.track : null;
  const colors = useAppColors(track?.artworkDataUrl ?? null);
  const [error, setError] = useState("");
  const close = useCallback(async () => {
    try {
      if (isTauri()) await getCurrentWindow().close();
      else window.close();
    } catch { setError("未能关闭设置，请重试"); }
  }, []);
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.defaultPrevented) void close();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [close]);
  return <Theme appearance="dark" accentColor="jade" grayColor="gray" radius="large">
    <WarmTooltipGroup lean={10}>
    <main className="settings-page" style={colors.style}>
      <header className="settings-titlebar">
        <div className="settings-title" data-tauri-drag-region><h1>设置</h1></div>
        <AppTooltip content="关闭设置">
          <IconButton size="2" variant="ghost" color="gray" radius="full"
            aria-label="关闭设置" onClick={() => void close()}><X size={18} /></IconButton>
        </AppTooltip>
      </header>
      <section className="settings-page-content" aria-label="设置内容">
        <AutoTuneSettings /><AutoApplySettings /><ColorSettings track={track} colors={colors} /><BackgroundSettings />
      </section>
      {error && <p className="settings-error" role="status">{error}</p>}
    </main>
    </WarmTooltipGroup>
  </Theme>;
}
