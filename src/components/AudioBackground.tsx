import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import { colord } from "colord";
import type { AudioSignal } from "../useAudioSignal";
import type { Palette } from "../coverPalette";
import type { AeroShardsHandle } from "./react-bits/AeroShards";

const AeroShards = lazy(() => import("./react-bits/AeroShards"));

export function AudioBackground({ signal, palette }: { signal: AudioSignal; palette: Palette }) {
  const container = useRef<HTMLDivElement>(null);
  const renderer = useRef<AeroShardsHandle>(null);
  const [failed, setFailed] = useState(false);
  const readPalette = useCallback((): [string, string, string] => {
    const style = container.current && getComputedStyle(container.current);
    return [0, 1, 2].map(index => colord(style?.getPropertyValue(`--strand-${index}`) || "#c9eee2").toHex()) as [string, string, string];
  }, []);
  useEffect(() => signal.subscribe(event => {
    if (event === "clear") renderer.current?.clear();
    else if (signal.readBands().some(value => value > 0)) renderer.current?.wake();
  }), [signal]);
  useEffect(() => {
    const owner = container.current?.closest(".music-window");
    if (!owner) return;
    const observer = new MutationObserver(() => renderer.current?.refreshPalette());
    observer.observe(owner, { attributes: true, attributeFilter: ["style", "class"] });
    return () => observer.disconnect();
  }, []);

  return <div ref={container} className="audio-background" aria-hidden="true"
    data-state={signal.reducedMotion ? "reduced" : failed ? "unavailable" : "active"}>
    {!signal.reducedMotion && !failed && <Suspense fallback={null}>
      <AeroShards ref={renderer} audioBands={signal.readBands} audioPalette={readPalette}
        backgroundColor="#0d111c" shardColor={palette[0]} accentColor={palette[1]}
        placement="full" flow="stream" detail="bold" material="pearl"
        density={1.35} shardSize={1.3} scale={1.15} depth={1} speed={0.45} spin={0.35}
        turbulence={0.8} glow={1.8} bloom={0.8} grain={0} chromaticAberration={0}
        interaction="none" holdToGather={false} onError={() => setFailed(true)} />
    </Suspense>}
  </div>;
}
