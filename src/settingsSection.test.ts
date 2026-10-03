import { describe, expect, it } from "vitest";
import { nextSectionRequest, parseSettingsSection } from "./settingsSection";

describe("parseSettingsSection", () => {
  it("accepts only known sections", () => {
    expect(parseSettingsSection("devocal-model")).toBe("devocal-model");
    expect(parseSettingsSection("colors")).toBeNull();
    expect(parseSettingsSection(null)).toBeNull();
    expect(parseSettingsSection(1)).toBeNull();
  });
});

describe("nextSectionRequest", () => {
  it("counts every request to a known section", () => {
    expect(nextSectionRequest(null, "devocal-model")).toEqual({ section: "devocal-model", seq: 1 });
    expect(nextSectionRequest({ section: "devocal-model", seq: 1 }, "devocal-model")).toEqual({ section: "devocal-model", seq: 2 });
  });
  it("treats a null payload as a plain open", () => {
    expect(nextSectionRequest({ section: "devocal-model", seq: 2 }, null)).toEqual({ section: null, seq: 3 });
    expect(nextSectionRequest(null, null)).toEqual({ section: null, seq: 1 });
  });
  it("ignores unknown payloads", () => {
    const previous = { section: "devocal-model" as const, seq: 2 };
    expect(nextSectionRequest(previous, "colors")).toBe(previous);
    expect(nextSectionRequest(previous, undefined)).toBe(previous);
    expect(nextSectionRequest(previous, 3)).toBe(previous);
    expect(nextSectionRequest(null, "colors")).toBeNull();
  });
});
