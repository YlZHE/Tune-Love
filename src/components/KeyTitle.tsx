import { AnimatePresence, motion, useIsPresent, useReducedMotion } from "motion/react";
import { targetTooltip, writeState, type WrittenPair } from "../autoApplyTarget";
import { titleLabel, type AutoTuneTarget } from "../keyDetection";
import { AppTooltip } from "./AppTooltip";
import { textTransition } from "./textTransition";

function KeyTitleFrame({ displayed, detected }: { displayed: string; detected: boolean }) {
  const present = useIsPresent();
  const transition = textTransition(!!useReducedMotion());
  return <motion.span
    className={`key-title-text ${detected ? "is-detected" : "is-brand"}`}
    aria-hidden={!present} inert={!present}
    initial={transition.initial} animate={transition.title} exit={transition.exit}
  >{detected ? displayed : <>Tune <span className="brand-light">Love</span></>}</motion.span>;
}

// The one Key/Scale title: the plugin pair being (or about to be) written, e.g. "F Minor",
// "F Chromatic" or "Chromatic". Evidence, uncertainty and write status live in its tooltip.
// Without a valid target it shows the brand.
export function KeyTitle({ target, written, enabled }: {
  target: AutoTuneTarget | null; written: WrittenPair | null; enabled: boolean;
}) {
  const label = target && titleLabel(target);
  const displayed = label ?? "Tune Love";
  const stage = <span className="key-title-stage">
    {/* In-flow, invisible copy of the text: gives the absolutely placed frames a width, so
        the tooltip trigger covers the title instead of the whole drag region. */}
    <span className={`key-title-sizer${label ? " is-detected" : ""}`} data-text={displayed} aria-hidden="true" />
    <AnimatePresence initial={false}>
      <KeyTitleFrame key={displayed} displayed={displayed} detected={label !== null} />
    </AnimatePresence>
  </span>;
  // One constant tree, so the brand-to-key change animates inside the same AnimatePresence;
  // without a target the tooltip is disabled and the title stays part of the drag region.
  return <AppTooltip disabled={!target} content={target ? targetTooltip(target, written, enabled) : ""}
    className={target ? "key-title-trigger" : undefined}>
    <span className="key-title-hit" tabIndex={target ? 0 : undefined}>
      {stage}
      {target && writeState(target, written, enabled) === "failed"
        && <span className="key-title-failed" role="img" aria-label="写入失败" />}
    </span>
  </AppTooltip>;
}
