import { describe, expect, it } from "vitest";
import { reactiveShards } from "./backgroundAudio";
import { parseBackgroundPreference } from "./backgroundPreferences";

describe("audio-reactive background", () => {
  it("stops transport on silence rather than fabricating activity", () => {
    expect(reactiveShards([0, 0, 0])).toMatchObject({ active: false, speed: 0, depth: 1 });
  });
  it("maps bass to at most five percent depth without changing the treble highlights", () => {
    expect(reactiveShards([1, 0, 0])).toMatchObject({ depth: 1.05, glow: 0.7, brightness: 0.9 });
    expect(reactiveShards([0.5, 0, 0]).depth).toBeCloseTo(1.025);
  });
  it("maps mids to transport and turbulence without a bass pulse", () => {
    const result = reactiveShards([0, 1, 0]);
    expect(result.speed).toBeCloseTo(1.1);
    expect(result.turbulence).toBeCloseTo(1.3);
    expect(result.depth).toBe(1);
  });
  it("maps highs to bounded highlights without expanding the background", () => {
    const result = reactiveShards([0, 0, 1]);
    expect(result.glow).toBeCloseTo(1.5);
    expect(result.bloom).toBeCloseTo(1.3);
    expect(result.brightness).toBeCloseTo(1.06);
    expect(result.depth).toBe(1);
  });
  it("keeps active transport within ten percent of its baseline", () => {
    for (const bands of [[1, 0, 0], [0, 0.08, 0], [0, 0.5, 0], [0, 1, 0], [0, 0, 1], [1, 1, 1]]) {
      const speed = reactiveShards(bands).speed;
      expect(speed).toBeGreaterThanOrEqual(0.9);
      expect(speed).toBeLessThanOrEqual(1.1);
    }
    expect(reactiveShards([1, 0, 0]).speed).toBeCloseTo(0.9);
    expect(reactiveShards([0, 0.5, 0]).speed).toBeCloseTo(1);
  });
  it("fades transport to zero with dying audio instead of snapping from cruising speed", () => {
    expect(reactiveShards([0, 0.04, 0]).speed).toBeCloseTo(0.454);
    expect(reactiveShards([0, 0.001, 0]).speed).toBeLessThan(0.012);
    expect(reactiveShards([0, 0, 0]).speed).toBe(0);
  });
  it("keeps invalid or out-of-range values out of renderer parameters", () => {
    expect(reactiveShards([NaN, -1, Infinity])).toMatchObject({ active: false, speed: 0 });
    expect(reactiveShards([50, 50, 50])).toEqual(reactiveShards([1, 1, 1]));
  });
  it.each([null, "bad json", "null", "{}", '{"enabled":"true"}', '{"enabled":false}'])
    ("keeps the new feature off for absent or malformed preferences: %s", raw => {
      expect(parseBackgroundPreference(raw)).toBe(false);
    });
  it("restores an explicitly enabled background", () => {
    expect(parseBackgroundPreference('{"enabled":true}')).toBe(true);
  });
});
