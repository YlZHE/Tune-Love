export function reactiveShards(bands: readonly number[]) {
  const [low, mid, high] = [0, 1, 2].map(index => {
    const value = bands[index];
    return Number.isFinite(value) ? Math.max(0, Math.min(1, value)) : 0;
  });
  const activity = Math.max(low, mid, high);
  // Cruise gently while sound is present; only the quiet tail fades to rest.
  const motionEnvelope = Math.min(1, activity / 0.08);
  return {
    active: activity > 0,
    depth: 1 + low * 0.05,
    speed: motionEnvelope * (0.9 + mid * 0.2),
    turbulence: 0.6 + mid * 0.7,
    glow: 0.7 + high * 0.8,
    bloom: 0.65 + high * 0.65,
    brightness: 0.9 + high * 0.16,
  };
}
