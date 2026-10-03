import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { loadManifest, modelFile } from "./model-manifest.mjs";

describe("model manifest", () => {
  it("the StemgenRT licence text names the manifest's origin, size and SHA-256", () => {
    const f = modelFile("stemgenrt-hop128");
    const text = readFileSync(new URL("../licenses/StemgenRT-5.8.txt", import.meta.url), "utf8");
    expect(text).toContain(f.origin);
    expect(text).toContain(f.bytes.toLocaleString("en-US") + " bytes");
    expect(text).toContain(`SHA-256 ${f.sha256}`);
  });

  it("fetch-stemgenrt.mjs reads the manifest and hard-codes no hash or URL", () => {
    const src = readFileSync(new URL("./fetch-stemgenrt.mjs", import.meta.url), "utf8");
    expect(src).toContain("model-manifest.mjs");
    expect(src).not.toMatch(/[0-9a-f]{64}/);
    expect(src).not.toContain("githubusercontent");
  });

  it("the manifest lists the three measured mirrors", () => {
    expect(loadManifest().mirrors).toEqual(["https://ghfast.top/", "https://ghproxy.net/", "https://ghproxy.vip/"]);
  });

  it("modelFile defaults to the first file and rejects unknown ids or files", () => {
    expect(modelFile("stemgenrt-hop128").file).toBe("model.onnx");
    expect(modelFile("stemgenrt-hop128", "model.onnx").file).toBe("model.onnx");
    expect(() => modelFile("nope")).toThrow();
    expect(() => modelFile("stemgenrt-hop128", "nope.onnx")).toThrow();
  });
});
