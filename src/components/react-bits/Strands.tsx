'use client';

import { Renderer, Program, Mesh, Triangle } from 'ogl';
import { useEffect, useImperativeHandle, useRef, useState, type Ref } from 'react';
import { colord } from 'colord';
import { smoothAudioLevel } from '../../audioLevel';

import './Strands.css';

const MAX_STRANDS = 3;
// Retain the existing horizontal footprint and compact vertical viewport.
const HORIZONTAL_STRETCH = 2.05;
const VERTICAL_SCALE = 0.8;

const VERT = `#version 300 es
in vec2 position;
void main() {
  gl_Position = vec4(position, 0.0, 1.0);
}
`;

const FRAG = `#version 300 es
precision highp float;

uniform vec2 uResolution;
uniform vec3 uColors[3];
uniform vec3 uBands;
uniform float uWaviness;
uniform float uThickness;
uniform float uGlow;
uniform float uTaper;
uniform float uSpread;
uniform float uOpacity;
uniform float uScale;
uniform float uSaturation;

out vec4 fragColor;

const float PI = 3.14159265;

void main() {
  vec2 uv = (gl_FragCoord.xy - 0.5 * uResolution) / uResolution.y;
  uv /= max(uScale, 0.0001);

  float env = pow(max(cos(uv.x * PI * 1.3), 0.0), uTaper);

  vec3 col = vec3(0.0);

  for (int i = 0; i < ${MAX_STRANDS}; i++) {
    float fi = float(i);
    float level = uBands[i];
    float e = 0.06 + level * 0.8 * 0.94;
    float ph = fi * 1.7 * uSpread;
    float freq = (2.0 + fi * 0.35) * uWaviness;
    // Fixed identities and phases: only measured band energy changes the shape.
    float w = sin(uv.x * freq + ph) * 0.60
            + sin(uv.x * freq * 1.1 + ph * 1.7) * 0.40;
    float amp = (0.008 + level * 0.016) * env;
    // Positive shader y is upward: low below, mid fixed at zero, high above.
    // The outer curves move with their own energy; mid changes only intensity.
    // Keep the ordering independent of energy, without any clock/rotation.
    float y = i == 1 ? 0.0 : (fi - 1.0) * (0.022 + 0.064 * level) * env + w * amp;

    float d = abs(uv.y - y);
    float thick = (0.012 + 0.012 * e) * (0.35 + env) * uThickness;
    float g = thick / (d + thick * 0.45);
    g = g * g;

    // Lift only very dark emitted colors; leave the stored/extracted palette intact.
    // Additive lift stays continuous even when a transition starts at pure black.
    vec3 baseColor = uColors[i];
    float brightest = max(max(baseColor.r, baseColor.g), baseColor.b);
    vec3 visibleColor = baseColor + vec3(max(0.0, 0.28 - brightest));
    col += visibleColor * g * env * (0.45 + 0.7 * e);
  }

  col = 1.0 - exp(-col * uGlow);

  float gray = dot(col, vec3(0.2126, 0.7152, 0.0722));
  col = max(mix(vec3(gray), col, uSaturation), 0.0);

  float lum = max(max(col.r, col.g), col.b);
  float alpha = clamp(lum, 0.0, 1.0) * uOpacity;

  fragColor = vec4(col * uOpacity, alpha);
}
`;


// React Bits Strands — https://reactbits.dev/r/Strands-TS-CSS.json
// Copyright (c) 2026 David Haz. See licenses/React-Bits.txt.
// Adapted from the official direct (glass=false) shader: three independently
// measured audio bands, fixed per-line colors and no autonomous time/phase motion.
export interface StrandsHandle { wake(): void; clear(): void }
interface StrandsProps {
  ref?: Ref<StrandsHandle>;
  audioBands: () => [number, number, number];
  reducedMotion: boolean;
}

export default function Strands({ ref, audioBands, reducedMotion }: StrandsProps) {
  const container = useRef<HTMLDivElement>(null);
  const input = useRef(audioBands);
  input.current = audioBands;
  const controls = useRef<StrandsHandle>({ wake() {}, clear() {} });
  const [failed, setFailed] = useState(false);
  useImperativeHandle(ref, () => ({
    wake: () => controls.current.wake(),
    clear: () => controls.current.clear(),
  }), []);

  useEffect(() => {
    const element = container.current;
    if (!element || reducedMotion || failed) return;
    const canvas = document.createElement('canvas');
    // Check WebGL2 before OGL, which otherwise attempts a WebGL1 fallback.
    const context = canvas.getContext('webgl2', { alpha: true, premultipliedAlpha: true, antialias: true });
    if (!context) { setFailed(true); return; }
    let geometry: Triangle | undefined;
    let program: Program | undefined;
    let resizeObserver: ResizeObserver | undefined;
    let colorObserver: MutationObserver | undefined;
    let raf = 0;
    let disposed = false;
    const levels = [0, 0, 0];
    let previousTime = 0;
    let colorUntil = 0;
    const colorText = ['', '', ''];
    const palette = Array.from({ length: 3 }, () => [1, 1, 1]);

    const stop = () => { cancelAnimationFrame(raf); raf = 0; previousTime = 0; };
    const lost = (event: Event) => {
      event.preventDefault();
      stop();
      if (!disposed) setFailed(true);
    };
    canvas.addEventListener('webglcontextlost', lost);
    const release = () => {
      disposed = true;
      stop();
      controls.current = { wake() {}, clear() {} };
      resizeObserver?.disconnect();
      colorObserver?.disconnect();
      canvas.removeEventListener('webglcontextlost', lost);
      geometry?.remove();
      program?.remove();
      canvas.remove();
      context.getExtension('WEBGL_lose_context')?.loseContext();
    };

    try {
      const renderer = new Renderer({
        canvas, webgl: 2, dpr: Math.min(window.devicePixelRatio || 1, 2),
        alpha: true, premultipliedAlpha: true, antialias: true, depth: false,
      });
      const gl = renderer.gl;
      gl.clearColor(0, 0, 0, 0);
      geometry = new Triangle(gl);
      // Keep every owned attribute so Geometry.remove also deletes its buffer.
      program = new Program(gl, {
        vertex: VERT, fragment: FRAG, transparent: true, depthTest: false, depthWrite: false,
        uniforms: {
          uResolution: { value: [46, 22] },
          uColors: { value: palette }, uBands: { value: levels },
          uWaviness: { value: 1 },
          uThickness: { value: 0.7 * VERTICAL_SCALE / HORIZONTAL_STRETCH }, uGlow: { value: 2.6 },
          uTaper: { value: 2.8 }, uSpread: { value: 1 }, uOpacity: { value: 0.85 },
          uScale: { value: 1.5 * HORIZONTAL_STRETCH }, uSaturation: { value: 1 },
        },
      });
      program.setBlendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
      if (!gl.getProgramParameter(program.program, gl.LINK_STATUS)) throw new Error('Strands shader unavailable');
      const mesh = new Mesh(gl, { geometry, program });
      const uniforms = program.uniforms;
      element.appendChild(canvas);

      const draw = (time: number) => {
        raf = 0;
        if (disposed || document.hidden) return;
        try {
          const target = input.current();
          const elapsed = previousTime ? Math.min(time - previousTime, 64) : 16;
          previousTime = time;
          const style = getComputedStyle(element);
          for (let i = 0; i < 3; i++) {
            levels[i] = smoothAudioLevel(levels[i], target[i], elapsed);
            // Read each interpolated CSS color, never the target hex or the selected accent.
            const nextColor = style.getPropertyValue(`--strand-${i}`).trim();
            if (nextColor === colorText[i]) continue;
            colorText[i] = nextColor;
            const color = colord(nextColor).toRgb();
            palette[i][0] = color.r / 255; palette[i][1] = color.g / 255; palette[i][2] = color.b / 255;
          }
          renderer.render({ scene: mesh });
          if (levels.some(v => v > 0) || target.some(v => v > 0) || time < colorUntil) raf = requestAnimationFrame(draw);
          else previousTime = 0;
        } catch {
          stop();
          if (!disposed) setFailed(true);
        }
      };
      const wake = () => {
        if (!disposed && !document.hidden && !raf) raf = requestAnimationFrame(draw);
      };
      controls.current = {
        wake,
        clear() { levels.fill(0); stop(); wake(); },
      };
      const resize = () => {
        const width = Math.max(element.clientWidth, 1);
        const height = Math.max(element.clientHeight, 1);
        renderer.setSize(width, height);
        uniforms.uResolution.value = [gl.drawingBufferWidth, gl.drawingBufferHeight];
        wake();
      };
      resizeObserver = new ResizeObserver(resize);
      resizeObserver.observe(element);
      // A palette change must wake a sleeping glyph for the existing .45 s CSS
      // transition; after its last interpolated frame it returns to sleep.
      colorObserver = new MutationObserver(() => {
        colorUntil = performance.now() + 500;
        wake();
      });
      const colorOwner = element.closest('.music-window');
      if (colorOwner) colorObserver.observe(colorOwner, { attributes: true, attributeFilter: ['style', 'class'] });
      resize();
    } catch {
      release();
      setFailed(true);
      return;
    }
    return release;
  }, [reducedMotion, failed]);

  return <div ref={container} className="strands-container">
    {(reducedMotion || failed) && <svg className="strands-fallback" viewBox="0 0 30 18" preserveAspectRatio="none" aria-hidden="true">
      <g fill="none" strokeWidth="1.2" strokeLinecap="round" opacity=".8">
        <path d="M3 12.5Q15 15.5 27 12.5" style={{ stroke: 'color-mix(in srgb, var(--strand-0) 72%, white)' }} />
        <path d="M2 9H28" style={{ stroke: 'color-mix(in srgb, var(--strand-1) 72%, white)' }} />
        <path d="M3 5.5Q15 2.5 27 5.5" style={{ stroke: 'color-mix(in srgb, var(--strand-2) 72%, white)' }} />
      </g>
    </svg>}
  </div>;
}

