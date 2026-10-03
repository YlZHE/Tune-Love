// Downloads the StemgenRT-5.8 weights (unmodified) from the authors' pinned commit and
// verifies size and SHA-256 before anything can load them. The weights are not part of
// this repository; see licenses/StemgenRT-5.8.txt for source, licence status and training data.
//
// Usage: node scripts/fetch-stemgenrt.mjs --accept [destination.onnx]
// Default destination is where the app looks for it:
//   %LOCALAPPDATA%\io.github.ylzhe.tunelove\models\stemgenrt-hop128\model.onnx
import { createHash } from "node:crypto";
import { createReadStream, createWriteStream, existsSync } from "node:fs";
import { mkdir, rename, rm, stat } from "node:fs/promises";
import { dirname, join } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { modelFile } from "./model-manifest.mjs";

// URL, size and hash come from src-tauri/models.json, the same manifest the app uses.
const MODEL = modelFile("stemgenrt-hop128");

const args = process.argv.slice(2);
const accepted = args.includes("--accept");
const target = args.find(a => !a.startsWith("--"))
  ?? join(process.env.LOCALAPPDATA ?? ".", "io.github.ylzhe.tunelove", "models", "stemgenrt-hop128", MODEL.file);

async function sha256(path) {
  const hash = createHash("sha256");
  await pipeline(createReadStream(path), hash);
  return hash.digest("hex");
}

async function verified(path) {
  if (!existsSync(path)) return false;
  return (await stat(path)).size === MODEL.bytes && await sha256(path) === MODEL.sha256;
}

console.log(`StemgenRT-5.8 model weights
  source:  ${MODEL.origin}
  size:    ${MODEL.bytes} bytes
  sha256:  ${MODEL.sha256}
  licence: repository MIT; weights licence not separately stated by the authors
  data:    trained on MUSDB18-HQ (educational use only) and MoisesDB (CC BY-NC-SA 4.0)
  target:  ${target}`);

if (await verified(target)) {
  console.log("Already present and verified.");
  process.exit(0);
}
if (!accepted) {
  console.log("\nRe-run with --accept to download for free, non-commercial use under the terms above.");
  process.exit(1);
}

await mkdir(dirname(target), { recursive: true });
const temp = `${target}.download-${process.pid}`;
try {
  const response = await fetch(MODEL.origin);
  if (!response.ok || !response.body) throw new Error(`HTTP ${response.status}`);
  await pipeline(Readable.fromWeb(response.body), createWriteStream(temp));
  const size = (await stat(temp)).size;
  if (size !== MODEL.bytes) throw new Error(`size ${size} != ${MODEL.bytes}`);
  const digest = await sha256(temp);
  if (digest !== MODEL.sha256) throw new Error(`sha256 ${digest} != ${MODEL.sha256}`);
  await rename(temp, target);
  console.log("Downloaded and verified.");
} catch (error) {
  await rm(temp, { force: true });
  console.error(`Download failed: ${error.message}`);
  process.exit(1);
}
