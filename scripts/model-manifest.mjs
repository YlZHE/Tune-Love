// Reads the single model manifest shared with the app (src-tauri/models.json).
import { readFileSync } from "node:fs";

export const MANIFEST_PATH = new URL("../src-tauri/models.json", import.meta.url);

export function loadManifest() {
  return JSON.parse(readFileSync(MANIFEST_PATH, "utf8"));
}

/** The manifest entry for one file of a model; `file` defaults to the model's first file. */
export function modelFile(id, file) {
  const model = loadManifest().models.find(m => m.id === id);
  if (!model) throw new Error(`unknown model ${id}`);
  const spec = file === undefined ? model.files[0] : model.files.find(f => f.file === file);
  if (!spec) throw new Error(`model ${id} has no file ${file}`);
  return spec;
}
