'use client';

// React Bits Elastic Slider — Copyright (c) 2026 David Haz.
// Source: https://reactbits.dev/r/ElasticSlider-TS-CSS.json (2026-09-29).
// License: MIT + Commons Clause; see licenses/React-Bits.txt.
// Retains the original decay, overflow transforms and spring return. Local
// adaptation: compact single-row labels, controlled value, Radix input,
// shared elastic surface, live Warm Tooltip, reduced motion and scoped CSS.
import { useEffect, useRef, useState, type PointerEvent } from 'react';
import { animate, motion, useMotionValue, useMotionValueEvent, useReducedMotion, useTransform } from 'motion/react';
import { Slider } from 'radix-ui';
import { AppTooltip, WarmTooltipGroup } from '../AppTooltip';
import './ElasticSlider.css';

const MAX_OVERFLOW = 50;
const HOVER_SCALE = 1.12;

interface ElasticSliderProps {
  value: number;
  onChange(value: number): void;
  ariaLabel: string;
  startLabel?: string;
  endLabel?: string;
}

export default function ElasticSlider({ value, onChange, ariaLabel, startLabel, endLabel }: ElasticSliderProps) {
  const sliderRef = useRef<HTMLSpanElement>(null);
  const surfaceRef = useRef<HTMLDivElement>(null);
  const pointerId = useRef<number | null>(null);
  const hovered = useRef(false);
  const [tipOpen, setTipOpen] = useState(false);
  const hideTipTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const reduce = useReducedMotion();
  const clientX = useMotionValue(0);
  const overflow = useMotionValue(0);
  const scale = useMotionValue(1);
  const stretchSide = useMotionValue(0);
  // Match the visual track extension after the wrapper's hover scale so the
  // endpoint text keeps its gap, including while the spring returns.
  const startX = useTransform(() => overflow.get() * Math.min(stretchSide.get(), 0) / scale.get());
  const endX = useTransform(() => overflow.get() * Math.max(stretchSide.get(), 0) / scale.get());

  useMotionValueEvent(clientX, 'change', latest => {
    if (!sliderRef.current || reduce) return;
    const { left, right } = sliderRef.current.getBoundingClientRect();
    stretchSide.set(latest < left ? -1 : latest > right ? 1 : 0);
    const distance = latest < left ? left - latest : latest > right ? latest - right : 0;
    overflow.jump(decay(distance, MAX_OVERFLOW));
  });

  useEffect(() => {
    if (reduce) { overflow.jump(0); scale.jump(1); }
  }, [reduce, overflow, scale]);
  useEffect(() => () => {
    clearTimeout(hideTipTimer.current);
    overflow.stop(); scale.stop();
  }, [overflow, scale]);

  const showValue = () => { clearTimeout(hideTipTimer.current); setTipOpen(true); };
  const hideValueSoon = () => {
    clearTimeout(hideTipTimer.current);
    hideTipTimer.current = setTimeout(() => setTipOpen(false), 450);
  };

  const setScale = (active: boolean) => {
    if (reduce) scale.jump(1);
    else animate(scale, active ? HOVER_SCALE : 1, { duration: 0.16 });
  };
  const beginDrag = (event: PointerEvent<HTMLSpanElement>) => {
    if (event.button !== 0 || pointerId.current !== null) return;
    pointerId.current = event.pointerId;
    clientX.jump(event.clientX);
    setScale(true);
    showValue();
  };
  const endDrag = (event: PointerEvent<HTMLSpanElement>) => {
    if (pointerId.current !== event.pointerId) return;
    pointerId.current = null;
    if (reduce) overflow.jump(0);
    else animate(overflow, 0, { type: 'spring', bounce: 0.5 });
    setScale(hovered.current);
    if (event.type === 'pointerup') hideValueSoon();
    else { clearTimeout(hideTipTimer.current); setTipOpen(false); }
  };

  return <WarmTooltipGroup lean={10}><div className="elastic-slider">
    <motion.div ref={surfaceRef} className="elastic-slider__surface" aria-hidden="true" style={{
      x: useTransform(() => overflow.get() * stretchSide.get() / 2),
      scaleX: useTransform(() => {
        const zoom = scale.get();
        const amount = overflow.get();
        const width = surfaceRef.current?.offsetWidth ?? 0;
        return width > 0 ? zoom + amount / width : zoom;
      }),
      scaleY: useTransform(() => scale.get() * (1 - overflow.get() / MAX_OVERFLOW * 0.08)),
    }} />
    <motion.div className="elastic-slider__wrapper"
      onHoverStart={() => { hovered.current = true; setScale(true); }}
      onHoverEnd={() => { hovered.current = false; if (pointerId.current === null) setScale(false); }}
      style={{ scale, opacity: useTransform(scale, [1, HOVER_SCALE], [0.7, 1]) }}>
      {startLabel && <motion.span className="elastic-slider__label" style={{ x: startX }}>{startLabel}</motion.span>}
      <Slider.Root ref={sliderRef} className="elastic-slider__root" dir="ltr"
        value={[value]} min={0} max={100} step={1} onValueChange={values => onChange(values[0])}
        onPointerDown={beginDrag}
        onPointerMove={event => { if (pointerId.current === event.pointerId) clientX.jump(event.clientX); }}
        onKeyDown={event => {
          if (['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End', 'PageUp', 'PageDown'].includes(event.key)) {
            showValue(); hideValueSoon();
          }
        }}
        onBlur={() => { if (pointerId.current === null) { clearTimeout(hideTipTimer.current); setTipOpen(false); } }}
        onPointerUp={endDrag} onPointerCancel={endDrag} onLostPointerCapture={endDrag}>
        <motion.div className="elastic-slider__track-wrapper" aria-hidden="true" style={{
          scaleX: useTransform(overflow, latest => {
            const width = sliderRef.current?.getBoundingClientRect().width ?? 0;
            return width > 0 ? 1 + latest / width : 1;
          }),
          scaleY: useTransform(overflow, [0, MAX_OVERFLOW], [1, 0.8]),
          transformOrigin: useTransform(clientX, latest => {
            const rect = sliderRef.current?.getBoundingClientRect();
            return rect ? latest < rect.left + rect.width / 2 ? 'right' : 'left' : 'center';
          }),
          height: useTransform(scale, [1, HOVER_SCALE], [6, 10]),
        }}>
          <div className="elastic-slider__track">
            <div className="elastic-slider__range" style={{ width: `${value}%` }} />
          </div>
          <div className="elastic-slider__value-anchor" style={{ left: `${value}%` }}>
            <AppTooltip content={`${value}%`} open={tipOpen} followAnchor side="top" gap={30}>
              <span className="elastic-slider__value-point" />
            </AppTooltip>
          </div>
        </motion.div>
        <Slider.Thumb className="elastic-slider__thumb" aria-label={ariaLabel} aria-valuetext={`${value}%`} />
      </Slider.Root>
      {endLabel && <motion.span className="elastic-slider__label" style={{ x: endX }}>{endLabel}</motion.span>}
    </motion.div>
  </div></WarmTooltipGroup>;
}

function decay(value: number, max: number): number {
  if (max === 0) return 0;
  const entry = value / max;
  const sigmoid = 2 * (1 / (1 + Math.exp(-entry)) - 0.5);
  return sigmoid * max;
}
