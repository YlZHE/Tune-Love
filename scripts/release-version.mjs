// Release gate: a `v<major>.<minor>.<patch>` tag must equal the app version in
// tauri.conf.json, package.json and the [package] section of Cargo.toml.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const TAG_RE = /^v(\d+)\.(\d+)\.(\d+)$/;

/** Reads `version` from the `[package]` table of a Cargo.toml source string. */
export function readCargoPackageVersion(toml) {
  let inPackage = false;
  for (const line of toml.split(/\r?\n/)) {
    const section = line.match(/^\s*\[([^\]]+)\]\s*(?:#.*)?$/);
    if (section) {
      inPackage = section[1].trim() === "package";
      continue;
    }
    if (inPackage) {
      const m = line.match(/^\s*version\s*=\s*"([^"]*)"/);
      if (m) return m[1];
    }
  }
  return null;
}

export function checkVersions(tag, versions) {
  if (!TAG_RE.test(tag)) {
    return {
      ok: false,
      error: `tag "${tag}" is not of the form vMAJOR.MINOR.PATCH (e.g. v0.1.0); pre-release suffixes are not accepted`,
    };
  }
  const version = tag.slice(1);
  const files = [
    ["src-tauri/tauri.conf.json", versions.tauri],
    ["package.json", versions.pkg],
    ["src-tauri/Cargo.toml", versions.cargo],
  ];
  for (const [file, actual] of files) {
    if (actual !== version) {
      return {
        ok: false,
        error: `${file} version is ${JSON.stringify(actual ?? null)} but tag ${tag} requires "${version}"`,
      };
    }
  }
  return { ok: true, version };
}

function main() {
  const tag = process.argv[2];
  if (!tag) {
    console.error("usage: node scripts/release-version.mjs <tag>");
    return 1;
  }
  const root = join(dirname(fileURLToPath(import.meta.url)), "..");
  try {
    const versions = {
      tauri: JSON.parse(readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf8")).version,
      pkg: JSON.parse(readFileSync(join(root, "package.json"), "utf8")).version,
      cargo: readCargoPackageVersion(readFileSync(join(root, "src-tauri", "Cargo.toml"), "utf8")),
    };
    const result = checkVersions(tag, versions);
    if (!result.ok) {
      console.error(result.error);
      return 1;
    }
    console.log(result.version);
    return 0;
  } catch (e) {
    console.error(`release version check failed: ${e instanceof Error ? e.message : e}`);
    return 1;
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  process.exitCode = main();
}
