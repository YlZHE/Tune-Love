import { AnimatePresence, motion, useIsPresent, useReducedMotion } from "motion/react";
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

export function KeyTitle({ label }: { label: string | null }) {
  const displayed = label ?? "Tune Love";
  return <span className="key-title-stage">
    <AnimatePresence initial={false}>
      <KeyTitleFrame key={displayed} displayed={displayed} detected={label !== null} />
    </AnimatePresence>
  </span>;
}
