// Generate packaging assets from Phosphor's MIT-licensed MusicNotes icon.
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MusicNotesIcon } from "@phosphor-icons/react";
import { mkdir, writeFile } from "node:fs/promises";
const icon = renderToStaticMarkup(React.createElement(MusicNotesIcon, {
  size: 1024, weight: "duotone", color: "#2da78b",
}));
await mkdir(new URL("../src-tauri/icons/", import.meta.url), { recursive: true });
await writeFile(new URL("../src-tauri/icons/source.svg", import.meta.url), icon);
