// Builds devocal-engine.exe (the devocal/ workspace) and puts it where the app finds it.
//
//   npm run build:engine                 debug build
//   npm run build:engine -- --release    release build
//
// Copies the built exe to:
//   1. src-tauri/binaries/devocal-engine-x86_64-pc-windows-msvc.exe
//      The installer build reads it from there: bundle.externalBin in
//      src-tauri/tauri.release.conf.json (never in the base tauri.conf.json, so that
//      daily builds and CI `cargo test` do not need the engine).
//   2. src-tauri/target/<profile>/devocal-engine.exe
//      The app starts current_exe().with_file_name("devocal-engine.exe"). The base config
//      has no externalBin, so tauri-build does not copy anything for dev runs; this copy
//      lets `npm run tauri dev` (debug) and a local release exe find the engine.
//
// Both destinations are git-ignored (src-tauri/binaries/, src-tauri/target/).
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const TRIPLE = "x86_64-pc-windows-msvc";
const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const args = process.argv.slice(2);
const unknown = args.filter(a => a !== "--release");
if (unknown.length > 0) {
  console.error(`unknown argument(s): ${unknown.join(" ")}\nusage: node scripts/build-devocal-engine.mjs [--release]`);
  process.exit(2);
}
if (process.platform !== "win32" || process.arch !== "x64") {
  console.error(`devocal-engine is built for ${TRIPLE} only (this is ${process.platform}/${process.arch})`);
  process.exit(1);
}
const release = args.includes("--release");
const profile = release ? "release" : "debug";

// --message-format=json-render-diagnostics: compiler output stays readable on stderr and
// stdout carries the artifact records, which name the exe actually produced (this also
// follows CARGO_TARGET_DIR if it is set).
const cargoArgs = ["build", "-p", "devocal-engine", "--bin", "devocal-engine", "--message-format=json-render-diagnostics"];
if (release) cargoArgs.push("--release");
console.log(`> cargo ${cargoArgs.join(" ")}   (in devocal/)`);
const build = spawnSync("cargo", cargoArgs, {
  cwd: join(root, "devocal"),
  stdio: ["inherit", "pipe", "inherit"],
  encoding: "utf8",
  maxBuffer: 256 * 1024 * 1024,
});
if (build.error) {
  console.error(`failed to start cargo: ${build.error.message}`);
  process.exit(1);
}
if (build.status !== 0) {
  console.error(`cargo build failed (exit ${build.status})`);
  process.exit(build.status ?? 1);
}

let exe = null;
for (const line of build.stdout.split(/\r?\n/)) {
  if (!line.startsWith("{")) continue;
  const msg = JSON.parse(line);
  if (msg.reason === "compiler-artifact" && msg.target?.name === "devocal-engine"
      && msg.target.kind?.includes("bin") && msg.executable) {
    exe = msg.executable;
  }
}
if (!exe) {
  console.error("cargo did not report a devocal-engine executable");
  process.exit(1);
}

const destinations = [
  join(root, "src-tauri", "binaries", `devocal-engine-${TRIPLE}.exe`),
  join(root, "src-tauri", "target", profile, "devocal-engine.exe"),
];
for (const dest of destinations) {
  if (resolve(exe).toLowerCase() === dest.toLowerCase()) continue;
  mkdirSync(dirname(dest), { recursive: true });
  copyFileSync(exe, dest);
  console.log(`copied ${exe}\n    -> ${dest}`);
}
