import { describe, expect, it } from "vitest";
import { validAudioLevel, smoothAudioLevel, type AudioIdentity, type AudioLevel } from "./audioLevel";

const identity: AudioIdentity = { sourceId: "player.exe", trackKey: '["player.exe","Song","Artist","Album"]', playing: true, visible: true };
const sample: AudioLevel = { sourceId: identity.sourceId, trackKey: identity.trackKey, status: "capturing", rms: 0.25, peak: 0.8, level: 0.7, bands: [0.7, 0.3, 0.1], updatedAtMs: 10_000 };

describe("only fresh audio from the displayed playing song reaches Strands", () => {
  it("accepts a current captured level without inventing or rescaling it", () => {
    expect(validAudioLevel(sample, identity, 10_100)).toBe(0.7);
    expect(validAudioLevel({ ...sample, level: 0 }, identity, 10_100)).toBe(0);
  });
  it.each(["idle", "resolving", "unavailable"])("silences the %s capture state", status => {
    expect(validAudioLevel({ ...sample, status }, identity, 10_100)).toBe(0);
  });
  it.each([
    { sourceId: "other.exe" }, { trackKey: "previous-song" }, { sourceId: null },
    { updatedAtMs: 9_699 }, { updatedAtMs: 10_151 }, { updatedAtMs: NaN }, { updatedAtMs: Infinity },
    { level: NaN }, { level: Infinity }, { level: -0.01 }, { level: 1.01 }, { level: "0.7" },
    { rms: NaN }, { rms: -0.01 }, { rms: 1.01 }, { peak: Infinity }, { peak: -0.01 }, { peak: 1.01 },
  ])("rejects an invalid or mismatched frame %j", changes => {
    expect(validAudioLevel({ ...sample, ...changes }, identity, 10_100)).toBe(0);
  });
  it.each([null, undefined, {}, "audio"])("safely rejects malformed IPC data %j", value => {
    expect(validAudioLevel(value, identity, 10_100)).toBe(0);
  });
  it("allows the documented age boundary and small clock jitter", () => {
    expect(validAudioLevel(sample, identity, 10_400)).toBe(0.7);
    expect(validAudioLevel(sample, identity, 9_950)).toBe(0.7);
  });
  it("silences paused, hidden and unidentified playback", () => {
    expect(validAudioLevel(sample, { ...identity, playing: false }, 10_100)).toBe(0);
    expect(validAudioLevel(sample, { ...identity, visible: false }, 10_100)).toBe(0);
    expect(validAudioLevel(sample, { ...identity, sourceId: null }, 10_100)).toBe(0);
  });
});

describe("audio motion follows energy and becomes completely still", () => {
  it("catches short beats and drops below half intensity within 80ms", () => {
    const attack = smoothAudioLevel(0, 1, 20);
    const release = smoothAudioLevel(1, 0, 80);
    expect(attack).toBeGreaterThan(0.7);
    expect(attack).toBeLessThan(1);
    expect(release).toBeGreaterThan(0);
    expect(release).toBeLessThan(0.5);
  });
  it("decays to exact zero so rendering can sleep", () => {
    let value = 1;
    for (let time = 0; time < 2_000; time += 16) value = smoothAudioLevel(value, 0, 16);
    expect(value).toBe(0);
  });
  it("releases enough between short beats to make the next movement distinct", () => {
    const released = smoothAudioLevel(0.95, 0.15, 64);
    expect(released).toBeGreaterThan(0.15);
    expect(released).toBeLessThan(0.3);
    const nextBeat = smoothAudioLevel(released, 0.95, 32);
    expect(nextBeat - released).toBeGreaterThan(0.5);
  });
  it("is independent of frame rate before the silence threshold", () => {
    const direct = smoothAudioLevel(0, 1, 32);
    const split = smoothAudioLevel(smoothAudioLevel(0, 1, 16), 1, 16);
    expect(direct).toBeCloseTo(split, 8);
  });
  it("does not create energy at rest or without elapsed time", () => {
    expect(smoothAudioLevel(0, 0, 16)).toBe(0);
    expect(smoothAudioLevel(0.4, 0.8, 0)).toBe(0.4);
  });
});
