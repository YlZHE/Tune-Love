import { describe, expect, it } from "vitest";
import { colord } from "colord";
import { manualPalette } from "./coverPalette";
import { parseColorPreferences } from "./colorPreferences";

describe("manual color stays simple and visible even at the extremes", () => {
  it.each(["#000000", "#ffffff", "#2255cc", "#cc3344"])("derives three ordered tones from %s", color => {
    const palette = manualPalette(color);
    expect(new Set(palette).size).toBe(3);
    const tones = palette.map(c => colord(c).toHsl());
    expect(tones[0].l).toBeGreaterThanOrEqual(30);
    expect(tones[2].l).toBeLessThanOrEqual(90);
    expect(tones[0].l).toBeLessThan(tones[1].l);
    expect(tones[1].l).toBeLessThan(tones[2].l);
    for (const tone of tones) expect(Math.abs(tone.h - colord(color).toHsl().h)).toBeLessThanOrEqual(1);
  });
  it("loads existing saved manual colors without migration or loss", () => {
    expect(parseColorPreferences('{"mode":"cover","manualColor":"#2255CC"}'))
      .toEqual({ mode: "cover", manualColor: "#2255cc" });
  });
  it.each([0, 1, 2, -1, 3, null])("ignores the obsolete cover slot %j without losing saved preferences", coverIndex => {
    expect(parseColorPreferences(JSON.stringify({ mode: "cover", manualColor: "#2255cc", coverIndex })))
      .toEqual({ mode: "cover", manualColor: "#2255cc" });
  });
});
