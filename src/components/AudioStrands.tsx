import { useEffect, useRef } from "react";
import type { AudioSignal } from "../useAudioSignal";
import Strands, { type StrandsHandle } from "./react-bits/Strands";

export function AudioStrands({ signal }: { signal: AudioSignal }) {
  const renderer = useRef<StrandsHandle>(null);
  useEffect(() => signal.subscribe(event => {
    if (event === "clear") renderer.current?.clear();
    else if (signal.readBands().some(value => value > 0)) renderer.current?.wake();
  }), [signal]);

  return <span className="audio-strands" aria-hidden="true">
    <Strands ref={renderer} reducedMotion={signal.reducedMotion} audioBands={signal.readBands} />
  </span>;
}
