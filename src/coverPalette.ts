import { extractColors } from "extract-colors";
import { colord } from "colord";

export type Palette = [string, string, string];

export async function extractCoverPalette(source: string): Promise<Palette | null> {
  const colors = await extractColors(source, { pixels: 16_384 });
  const ranked = [...colors].sort((a, b) => b.area - a.area || a.hex.localeCompare(b.hex));
  if (!ranked.length) return null;
  // Dominant black/white backgrounds must not outrank a smaller colored subject.
  const colorful = ranked.filter(color => {
    const hsl = colord(color.hex).toHsl();
    return hsl.s >= 25 && hsl.l >= 18 && hsl.l <= 85;
  });
  const palette = [...new Set(colorful.map(color => color.hex.toLowerCase()))].slice(0, 3);
  if (palette.length) {
    // Fill missing slots with shades of the automatically chosen accent, not neutrals.
    const tones = manualPalette(palette[0]);
    for (const color of [tones[1], tones[2], tones[0]]) {
      if (palette.length < 3 && !palette.includes(color)) palette.push(color);
    }
    return [palette[0], palette[1], palette[2]];
  }
  // Monochrome artwork explicitly prefers white. With no white pixel available,
  // use a soft neutral white rather than choosing the dark background or manual hue.
  const lightest = [...ranked].sort((a, b) => colord(b.hex).toHsl().l - colord(a.hex).toHsl().l)[0];
  const white = colord(lightest.hex).toHsl().l >= 85 ? lightest.hex.toLowerCase() : "#eeeeee";
  const tones = manualPalette(white);
  return [white, tones[0], tones[1]];
}

export function manualPalette(color: string): Palette {
  const hsl = colord(color).toHsl();
  const lightness = Math.max(30, Math.min(62, hsl.l));
  return [0, 14, 28].map(offset => colord({ ...hsl, l: lightness + offset }).toHex()) as Palette;
}
