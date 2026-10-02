import { useEffect, useState } from "react";
import { Progress } from "@radix-ui/themes";
import { clampProgress, formatClock } from "../nowPlaying";
import StarBorder from "./react-bits/StarBorder";

export function PlaybackProgress({ position, duration, playing, hasTrack }: {
  position: number; duration: number; playing: boolean; hasTrack: boolean;
}) {
  const [hidden, setHidden] = useState(() => document.hidden);
  const [reduced, setReduced] = useState(() => matchMedia("(prefers-reduced-motion: reduce)").matches);
  useEffect(() => {
    const query = matchMedia("(prefers-reduced-motion: reduce)");
    const update = () => { setHidden(document.hidden); setReduced(query.matches); };
    update();
    document.addEventListener("visibilitychange", update);
    query.addEventListener("change", update);
    return () => {
      document.removeEventListener("visibilitychange", update);
      query.removeEventListener("change", update);
    };
  }, []);
  const available = hasTrack && Number.isFinite(duration) && duration > 0;
  const value = available ? clampProgress(position, duration) * 100 : 0;
  const active = playing && available && value > 0 && value < 100 && !hidden && !reduced;

  return <StarBorder as="div" className="playback-progress-track" animated={active}
    color="var(--accent)" backgroundColor="color-mix(in srgb, var(--theme-color) 9%, var(--panel))">
    <Progress value={value} size="1" radius="full"
      className={`playback-progress ${available ? "" : "is-unavailable"}`}
      aria-label="播放进度"
      aria-valuetext={available ? `${formatClock(position)} / ${formatClock(duration)}` : "播放器未提供进度"} />
  </StarBorder>;
}
