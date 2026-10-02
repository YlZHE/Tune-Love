import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { TextField } from "@radix-ui/themes";
import { AppTooltip } from "./AppTooltip";
import CometDial from "./react-bits/CometDial";

type ParameterDialProps = {
  label: string; value: number; onChange(value: number): void;
  description: string; min?: number; max?: number; step?: number; progressOrigin?: number;
};

export function ParameterDial({ label, description, value, onChange, min = 0, max = 100, step = 1, progressOrigin }: ParameterDialProps) {
  const [draft, setDraft] = useState(String(value));
  const [interacting, setInteracting] = useState(false);
  const [tipSide, setTipSide] = useState<"top" | "bottom">("bottom");
  const root = useRef<HTMLDivElement>(null);
  const inputId = useId();
  const descriptionId = useId();
  const decimals = String(step).split(".")[1]?.length ?? 0;
  useEffect(() => setDraft(String(value)), [value]);
  const positionHelp = () => {
    const box = root.current?.getBoundingClientRect();
    if (box) setTipSide(box.top > innerHeight - box.bottom ? "top" : "bottom");
  };
  useLayoutEffect(() => {
    positionHelp();
    window.addEventListener("resize", positionHelp);
    return () => window.removeEventListener("resize", positionHelp);
  }, []);
  const normalize = (number: number) => Number((Math.round((Math.max(min, Math.min(max, number)) - min) / step) * step + min).toFixed(decimals));
  const commit = () => {
    const parsed = draft.trim() === "" ? NaN : Number(draft);
    const next = Number.isFinite(parsed) ? normalize(parsed) : value;
    setDraft(String(next)); onChange(next);
  };

  return <div ref={root} className="parameter-dial" onPointerOverCapture={positionHelp} onFocusCapture={positionHelp}>
    <AppTooltip content={<span className="parameter-dial__help">{description}</span>}
      disabled={interacting} side={tipSide} gap={8}>
      <div className="parameter-dial__body" onPointerDown={event => {
        // Comet Dial owns ring gestures (especially touch capture). Let only
        // unhandled interactions reach Warm Tooltip's normal long-press logic.
        if (event.defaultPrevented) event.stopPropagation();
      }}>
      <CometDial label={label} value={value} onChange={next => { setDraft(String(next)); onChange(next); }} min={min} max={max} step={step} unit=""
        progressOrigin={progressOrigin} aria-describedby={descriptionId}
        size={86} sweep={260} speed={59} tapBounce={0.26} flickBounce={0.12}
        momentum={0} accent="var(--accent)" ink="#c7cbd5" onInteractionChange={setInteracting}
        readout={<TextField.Root id={inputId} className="parameter-dial__number" size="1" type="number"
          aria-label={`${label}数值`} aria-describedby={descriptionId} min={min} max={max} step={step} value={draft}
          onFocus={event => event.target.select()}
          onChange={event => {
            const raw = event.target.value; setDraft(raw);
            const number = raw.trim() === "" ? NaN : Number(raw);
            if (Number.isFinite(number) && number >= min && number <= max) onChange(normalize(number));
          }} onBlur={commit} onKeyDown={event => {
            if (event.key === "Enter") { commit(); event.currentTarget.blur(); }
          }} />} />
        <label htmlFor={inputId}>{label}</label>
      </div>
    </AppTooltip>
    <span id={descriptionId} className="controls-accessible-note">{description}</span>
  </div>;
}
