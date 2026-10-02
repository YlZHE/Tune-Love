import { useEffect, useMemo, useState, type CSSProperties } from "react";
import { extractCoverPalette, manualPalette, type Palette } from "./coverPalette";
import { colord, extend } from "colord";
import a11yPlugin from "colord/plugins/a11y";
import { useColorPreferences } from "./colorPreferences";

extend([a11yPlugin]);

export function useAppColors(artwork: string | null) {
  const preferences = useColorPreferences();
  const source = preferences.mode === "cover" ? artwork : null;
  const [extracted, setExtracted] = useState<{ source: string; palette: Palette | null } | null>(null);
  useEffect(() => {
    if (!source) return;
    let cancelled = false;
    void extractCoverPalette(source)
      .then(palette => { if (!cancelled) setExtracted({ source, palette }); })
      .catch(() => { if (!cancelled) setExtracted({ source, palette: null }); });
    return () => { cancelled = true; };
  }, [source]);

  // Keep the committed palette while the next image loads, avoiding a flash of
  // the manual color. Cancelled/late results cannot commit; missing covers reset.
  const coverPalette = source ? extracted?.palette ?? null : null;
  const coverColor = coverPalette?.[0] ?? null;
  const primaryColor = coverColor ?? preferences.manualColor;
  const palette = useMemo(() => coverPalette ?? manualPalette(preferences.manualColor), [coverPalette, preferences.manualColor]);
  const style = useMemo(() => {
    let readable = colord(primaryColor);
    // Keep chosen/extracted hue, but lift very dark colors for small text and icons.
    for (let i = 0; i < 20 && readable.contrast("#1d2430") < 4.5; i++) readable = readable.lighten(0.04);
    return {
      "--theme-color": primaryColor,
      "--accent": readable.toHex(),
      "--accent-contrast": "#0d111c",
      "--strand-0": palette[0],
      "--strand-1": palette[1],
      "--strand-2": palette[2],
    } as CSSProperties;
  }, [primaryColor, palette]);
  return { preferences, primaryColor, style, palette, coverColor, coverPalette,
    extracting: !!source && extracted?.source !== source };
}
