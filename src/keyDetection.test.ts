import { describe, expect, test } from "vitest";
import { autotuneTarget, keyLabel, targetLabel, targetOptionLabels } from "./keyDetection";

const identity = { sourceId: "player", trackKey: "song", targetGeneration: 4 } as const;

function detected(key: unknown, overrides: Record<string, unknown> = {}) {
  return {
    sourceId: identity.sourceId,
    trackKey: identity.trackKey,
    targetGeneration: identity.targetGeneration,
    status: "detected",
    key,
    updatedAtMs: 100,
    ...overrides,
  };
}

describe("keyLabel", () => {
  test.each([
    [0, "major", "C 大调"], [0, "minor", "C 小调"],
    [1, "major", "C♯ 大调"], [1, "minor", "C♯ 小调"],
    [2, "major", "D 大调"], [2, "minor", "D 小调"],
    [3, "major", "D♯ 大调"], [3, "minor", "D♯ 小调"],
    [4, "major", "E 大调"], [4, "minor", "E 小调"],
    [5, "major", "F 大调"], [5, "minor", "F 小调"],
    [6, "major", "F♯ 大调"], [6, "minor", "F♯ 小调"],
    [7, "major", "G 大调"], [7, "minor", "G 小调"],
    [8, "major", "G♯ 大调"], [8, "minor", "G♯ 小调"],
    [9, "major", "A 大调"], [9, "minor", "A 小调"],
    [10, "major", "A♯ 大调"], [10, "minor", "A♯ 小调"],
    [11, "major", "B 大调"], [11, "minor", "B 小调"],
  ] as const)("maps pitch class %i %s to %s", (pitchClass, mode, expected) => {
    expect(keyLabel(detected({ pitchClass, mode }), identity.sourceId,
      identity.trackKey, identity.targetGeneration)).toBe(expected);
  });

  test.each(["idle", "analyzing", "unavailable", "ready", "DETECTED", null, undefined])(
    "rejects non-detected status %s", status => {
      expect(keyLabel(detected({ pitchClass: 9, mode: "minor" }, { status }),
        identity.sourceId, identity.trackKey, identity.targetGeneration)).toBeNull();
    },
  );

  test.each([-1, 12, 1.5, "9", null, undefined, Number.NaN, Number.POSITIVE_INFINITY])(
    "rejects malformed pitch class %s", pitchClass => {
      expect(keyLabel(detected({ pitchClass, mode: "minor" }), identity.sourceId,
        identity.trackKey, identity.targetGeneration)).toBeNull();
    },
  );

  test.each(["Major", "MINOR", "dorian", "", null, undefined, 1])(
    "rejects malformed mode %s", mode => {
      expect(keyLabel(detected({ pitchClass: 9, mode }), identity.sourceId,
        identity.trackKey, identity.targetGeneration)).toBeNull();
    },
  );

  test.each([
    [null, identity.trackKey, identity.targetGeneration],
    ["", identity.trackKey, identity.targetGeneration],
    [identity.sourceId, "", identity.targetGeneration],
    [identity.sourceId, identity.trackKey, undefined],
    [identity.sourceId, identity.trackKey, -1],
    [identity.sourceId, identity.trackKey, 1.5],
  ] as const)("rejects invalid current identity", (sourceId, trackKey, targetGeneration) => {
    expect(keyLabel(detected({ pitchClass: 9, mode: "minor" }), sourceId,
      trackKey, targetGeneration)).toBeNull();
  });

  test.each([
    { sourceId: "other" },
    { sourceId: null },
    { trackKey: "older-song" },
    { trackKey: null },
    { targetGeneration: 3 },
    { targetGeneration: -1 },
    { targetGeneration: 4.5 },
  ])("rejects stale or malformed ownership $sourceId $trackKey $targetGeneration", overrides => {
    expect(keyLabel(detected({ pitchClass: 9, mode: "minor" }, overrides), identity.sourceId,
      identity.trackKey, identity.targetGeneration)).toBeNull();
  });

  test.each([0, -1, 1.5, "100", null, undefined, Number.NaN, Number.POSITIVE_INFINITY])(
    "rejects invalid detection timestamp %s", updatedAtMs => {
      expect(keyLabel(detected({ pitchClass: 9, mode: "minor" }, { updatedAtMs }),
        identity.sourceId, identity.trackKey, identity.targetGeneration)).toBeNull();
    },
  );

  test.each([null, undefined, [], "detected", 4, { status: "detected" },
    detected(null), detected([]), detected("A minor")])("rejects malformed snapshots", value => {
      expect(keyLabel(value, identity.sourceId, identity.trackKey, identity.targetGeneration)).toBeNull();
    });
});

describe("autotuneTarget", () => {
  const target = (overrides: Record<string, unknown> = {}) =>
    detected({ pitchClass: 6, mode: "minor" }, { autotuneTarget: { key: 6, scale: "minor", evidenceSeconds: 12.5, source: "analysis" }, ...overrides });

  test("returns the recommendation for the matching track regardless of key status", () => {
    expect(autotuneTarget(target({ status: "analyzing", key: null }), identity.sourceId, identity.trackKey,
      identity.targetGeneration)).toEqual({ key: 6, scale: "minor", evidenceSeconds: 12.5, source: "analysis" });
    expect(autotuneTarget(target({ autotuneTarget: { key: 6, scale: "minor", evidenceSeconds: 0, source: "cache" } }),
      identity.sourceId, identity.trackKey, identity.targetGeneration)?.source).toBe("cache");
  });

  test.each([
    [{ autotuneTarget: null }], [{ autotuneTarget: { key: 12, scale: "minor", evidenceSeconds: 1, source: "analysis" } }],
    [{ autotuneTarget: { key: 1, scale: "dorian", evidenceSeconds: 1, source: "analysis" } }],
    [{ autotuneTarget: { key: 1, scale: "chromatic", evidenceSeconds: 1, source: "analysis" } }],
    [{ autotuneTarget: { key: 1, scale: "major", evidenceSeconds: -1 } }],
    [{ autotuneTarget: { key: 1.5, scale: "major", evidenceSeconds: 1, source: "analysis" } }],
    [{ autotuneTarget: { key: 1, scale: "major", evidenceSeconds: 1 } }],
    [{ autotuneTarget: { key: 1, scale: "major", evidenceSeconds: 1, source: "cloud" } }],
    [{ trackKey: "other" }], [{ targetGeneration: 5 }],
  ])("rejects %j", overrides => {
    expect(autotuneTarget(target(overrides), identity.sourceId, identity.trackKey, identity.targetGeneration)).toBeNull();
  });

  test("labels and profile option names", () => {
    expect(targetLabel({ key: 6, scale: "minor", evidenceSeconds: 1, source: "analysis" })).toBe("F♯ 小调");
    expect(targetOptionLabels({ key: 6, scale: "minor", evidenceSeconds: 1, source: "analysis" })).toEqual({ key: "F#", scale: "Minor" });
    expect(targetOptionLabels({ key: 11, scale: "major", evidenceSeconds: 1, source: "analysis" })).toEqual({ key: "B", scale: "Major" });
  });
});
