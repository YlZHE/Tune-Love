import { useEffect, useState } from "react";
import ShinyText from "./react-bits/ShinyText";

export function PlaybackLabel({ text, playing }: { text: string; playing: boolean }) {
  const [hidden, setHidden] = useState(() => document.hidden);
  const [reduced, setReduced] = useState(() => matchMedia("(prefers-reduced-motion: reduce)").matches);

  useEffect(() => {
    const query = matchMedia("(prefers-reduced-motion: reduce)");
    const visibilityChanged = () => setHidden(document.hidden);
    const preferenceChanged = () => setReduced(query.matches);
    document.addEventListener("visibilitychange", visibilityChanged);
    query.addEventListener("change", preferenceChanged);
    return () => {
      document.removeEventListener("visibilitychange", visibilityChanged);
      query.removeEventListener("change", preferenceChanged);
    };
  }, []);

  // Unmount the Motion frame subscriber when inactive; don't keep an idle clock.
  if (!playing || hidden || reduced) return <span className="playback-label">{text}</span>;

  return <ShinyText text={text} className="playback-label" speed={2.4} spread={90}
    color="color-mix(in srgb, var(--accent) 70%, var(--muted) 30%)"
    shineColor="color-mix(in srgb, var(--accent) 15%, white 85%)" />;
}
