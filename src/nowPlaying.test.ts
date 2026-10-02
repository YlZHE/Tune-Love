import { describe, expect, it } from "vitest";
import { clampProgress, formatClock, isPlaying, projectedPosition, type NowPlaying } from "./nowPlaying";

const track: NowPlaying = {
  title: "Example", artist: "Artist", album: "Album", source: "Player",
  sourceId: "Player.exe", sourceIconDataUrl: null,
  artworkDataUrl: null, playbackStatus: "playing", playbackRate: 1,
  positionSeconds: 40, durationSeconds: 180, updatedAtMs: 10000,
};

describe("progress between system updates", () => {
  it("advances playback at the player's rate", () => {
    expect(projectedPosition({ ...track, playbackRate: 1.5 }, 12000)).toBe(43);
  });
  it("freezes paused playback even if the timeline timestamp is old", () => {
    expect(projectedPosition({ ...track, playbackStatus: "paused" }, 20000)).toBe(40);
  });
  it("immediately follows seeking backwards", () => {
    expect(projectedPosition({ ...track, positionSeconds: 5, updatedAtMs: 12000 }, 13000)).toBe(6);
  });
  it("never goes beyond a known duration", () => {
    expect(projectedPosition({ ...track, positionSeconds: 179 }, 13000)).toBe(180);
  });
  it("stops advancing after a disconnected backend", () => {
    expect(projectedPosition(track, 120000)).toBe(48);
  });
  it("does not invent progress when duration is unavailable", () => {
    expect(projectedPosition({ ...track, durationSeconds: 0 }, 12000)).toBe(0);
  });
});

describe("now playing display helpers", () => {
  it("formats a timeline position as a clock", () => {
    expect(formatClock(0)).toBe("0:00");
    expect(formatClock(74.9)).toBe("1:14");
    expect(formatClock(Number.NaN)).toBe("0:00");
  });

  it("clamps progress to a safe range", () => {
    expect(clampProgress(30, 120)).toBe(0.25);
    expect(clampProgress(-5, 120)).toBe(0);
    expect(clampProgress(180, 120)).toBe(1);
    expect(clampProgress(1, 0)).toBe(0);
  });

  it("recognizes only the playing state as active playback", () => {
    expect(isPlaying("playing")).toBe(true);
    expect(isPlaying("paused")).toBe(false);
    expect(isPlaying("unknown")).toBe(false);
  });
});
