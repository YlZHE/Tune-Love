import { describe, expect, test } from "vitest";
import { autotuneTarget, keyLabel, targetLabel, targetOptionLabels, titleLabel, sameTarget, type AutoTuneTarget } from "./keyDetection";

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
  const body = (overrides: Record<string, unknown> = {}) => ({
    key: 6, scale: "minor", candidate: null, uncoveredNotes: [], evidenceSeconds: 12.5, source: "analysis", ...overrides });
  const target = (overrides: Record<string, unknown> = {}, wrap: Record<string, unknown> = {}) =>
    detected({ pitchClass: 6, mode: "minor" }, { autotuneTarget: body(overrides), ...wrap });
  const parse = (value: unknown) =>
    autotuneTarget(value, identity.sourceId, identity.trackKey, identity.targetGeneration);

  test("returns the recommendation for the matching track regardless of key status", () => {
    expect(parse(target({}, { status: "analyzing", key: null }))).toEqual(body());
    expect(parse(target({ evidenceSeconds: 0, source: "cache" }))?.source).toBe("cache");
  });

  test("accepts Chromatic with or without a key, a candidate and uncovered notes", () => {
    const candidate = { key: 5, scale: "minor" };
    expect(parse(target({ key: null, scale: "chromatic" }))).toEqual(body({ key: null, scale: "chromatic" }));
    expect(parse(target({ key: 5, scale: "chromatic", candidate, uncoveredNotes: [1, 11] })))
      .toEqual(body({ key: 5, scale: "chromatic", candidate, uncoveredNotes: [1, 11] }));
  });

  test.each([
    [{ key: 12 }], [{ key: -1 }], [{ key: 1.5 }], [{ key: null }],
    [{ key: null, scale: "major" }],
    [{ scale: "dorian" }],
    [{ uncoveredNotes: [12] }], [{ uncoveredNotes: [-1] }], [{ uncoveredNotes: [1.5] }], [{ uncoveredNotes: "1" }],
    [{ uncoveredNotes: undefined }],
    [{ candidate: { key: 12, scale: "major" } }], [{ candidate: { key: 1, scale: "chromatic" } }],
    [{ candidate: { key: 1, scale: "dorian" } }], [{ candidate: undefined }],
    [{ evidenceSeconds: -1 }], [{ source: "cloud" }], [{ source: undefined }],
  ])("rejects %j", overrides => {
    expect(parse(target(overrides))).toBeNull();
  });

  test("rejects a missing target", () => {
    expect(parse(target({}, { autotuneTarget: null }))).toBeNull();
  });

  test.each([{ trackKey: "other" }, { targetGeneration: 5 }])("rejects other track %j", wrap => {
    expect(parse(target({}, wrap))).toBeNull();
  });
});

describe("target labels", () => {
  const base = { candidate: null, uncoveredNotes: [] as number[], evidenceSeconds: 1, source: "analysis" as const };
  const minor: AutoTuneTarget = { ...base, key: 6, scale: "minor" };

  test("titleLabel uses profile spellings", () => {
    expect(titleLabel({ ...base, key: 5, scale: "minor" })).toBe("F Minor");
    expect(titleLabel({ ...base, key: 8, scale: "chromatic" })).toBe("G# Chromatic");
    expect(titleLabel({ ...base, key: 8, scale: "major" })).toBe("G# Major");
    expect(titleLabel({ ...base, key: null, scale: "chromatic" })).toBe("Chromatic");
  });

  test("targetLabel and profile option names", () => {
    expect(targetLabel(minor)).toBe("F♯ 小调");
    expect(targetOptionLabels(minor)).toEqual({ key: "F#", scale: "Minor" });
    expect(targetOptionLabels({ ...base, key: 11, scale: "major" })).toEqual({ key: "B", scale: "Major" });
    expect(targetOptionLabels({ ...base, key: 8, scale: "chromatic" })).toEqual({ key: "G#", scale: "Chromatic" });
    expect(targetOptionLabels({ ...base, key: null, scale: "chromatic" })).toEqual({ key: null, scale: "Chromatic" });
  });
});

describe("sameTarget", () => {
  const t: AutoTuneTarget = { key: 5, scale: "chromatic", candidate: { key: 5, scale: "minor" },
    uncoveredNotes: [1], evidenceSeconds: 10, source: "analysis" };

  test("evidenceSeconds growth is the same target", () => {
    expect(sameTarget(t, { ...t, evidenceSeconds: 11 })).toBe(true);
  });

  test.each([
    { uncoveredNotes: [1, 6] }, { uncoveredNotes: [2] }, { source: "cache" as const },
    { candidate: { key: 5, scale: "major" as const } }, { candidate: null }, { key: null }, { scale: "minor" as const },
  ])("a change in %j is a new target", change => {
    expect(sameTarget(t, { ...t, ...change })).toBe(false);
  });
});
