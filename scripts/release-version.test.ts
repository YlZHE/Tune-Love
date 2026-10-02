import { describe, expect, it } from "vitest";
import { checkVersions, readCargoPackageVersion } from "./release-version.mjs";

const same = { tauri: "0.1.0", pkg: "0.1.0", cargo: "0.1.0" };

describe("checkVersions", () => {
  it("accepts a tag equal to all three versions", () => {
    expect(checkVersions("v0.1.0", same)).toEqual({ ok: true, version: "0.1.0" });
  });

  it("rejects malformed tags", () => {
    for (const tag of ["v0.1", "0.1.0"]) {
      const r = checkVersions(tag, same);
      expect(r.ok).toBe(false);
      expect((r as { error: string }).error).toContain("tag");
    }
  });

  it("rejects prerelease tags", () => {
    expect(checkVersions("v0.1.0-beta.1", same).ok).toBe(false);
  });

  it("names the mismatching file and its value", () => {
    const r = checkVersions("v0.1.0", { ...same, cargo: "0.1.1" });
    expect(r.ok).toBe(false);
    const error = (r as { error: string }).error;
    expect(error).toContain("Cargo.toml");
    expect(error).toContain("0.1.1");
  });

  it("names tauri.conf.json and package.json mismatches", () => {
    const t = checkVersions("v0.1.0", { ...same, tauri: "0.2.0" });
    expect((t as { error: string }).error).toContain("tauri.conf.json");
    const p = checkVersions("v0.1.0", { ...same, pkg: "0.3.0" });
    expect((p as { error: string }).error).toContain("package.json");
  });
});

describe("readCargoPackageVersion", () => {
  it("reads only the [package] version, not dependency versions", () => {
    const toml = [
      "[dependencies]",
      'serde = { version = "9.9.9" }',
      "",
      "[package]",
      'name = "x"',
      'version = "0.1.0"',
      "",
      "[dependencies.foo]",
      'version = "7.7.7"',
    ].join("\n");
    expect(readCargoPackageVersion(toml)).toBe("0.1.0");
  });

  it("handles CRLF and returns null when absent", () => {
    expect(readCargoPackageVersion('[package]\r\nversion = "1.2.3"\r\n')).toBe("1.2.3");
    expect(readCargoPackageVersion("[package]\nname = 'x'\n")).toBeNull();
  });
});
