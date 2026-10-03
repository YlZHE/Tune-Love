import { beforeEach, describe, expect, it } from "vitest";
import {
  MODEL_MANIFEST, STEMGENRT_ID, approxSize, consentKey, formatMiB, hasModelConsent, isValidMirrorPrefix,
  modelErrorText, modelTotalBytes, parseModelStatuses, pinnedCommit, progressPercent, progressText,
  readMirrorPrefix, rememberModelConsent, saveMirrorPrefix, sourceErrorHint, sourceLabel, type ModelStatus,
} from "./modelDownload";

function memoryStorage() {
  const map = new Map<string, string>();
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => { map.set(k, String(v)); },
    removeItem: (k: string) => { map.delete(k); },
    clear: () => map.clear(),
  };
}

const status = (patch: Partial<ModelStatus>): ModelStatus => ({
  id: "a", phase: "downloading", receivedBytes: 0, totalBytes: 0, source: null, path: null, error: null, sourceError: null, ...patch,
});

beforeEach(() => {
  (globalThis as { localStorage?: unknown }).localStorage = memoryStorage();
});

describe("manifest", () => {
  it("exposes the bundled manifest", () => {
    expect(STEMGENRT_ID).toBe("stemgenrt-hop128");
    expect(MODEL_MANIFEST.models[0].id).toBe(STEMGENRT_ID);
    expect(modelTotalBytes(MODEL_MANIFEST.models[0])).toBe(37529132);
  });
});

describe("parseModelStatuses", () => {
  it("parses a status array and drops unknown entries", () => {
    expect(parseModelStatuses(null)).toBeNull();
    expect(parseModelStatuses({ phase: "missing" })).toBeNull();
    expect(parseModelStatuses([
      { id: "a", phase: "downloading", receivedBytes: 5, totalBytes: 10, source: "origin", path: null, error: null, sourceError: null },
      { id: "b", phase: "exploded" },
      { phase: "missing" },
    ])).toEqual([{ id: "a", phase: "downloading", receivedBytes: 5, totalBytes: 10, source: "origin", path: null, error: null, sourceError: null }]);
  });
  it("preserves sourceError and maps missing or non-string text fields to null", () => {
    expect(parseModelStatuses([{ id: "a", phase: "downloading", receivedBytes: 1, totalBytes: 2, source: "origin", sourceError: "source_html" }]))
      .toEqual([{ id: "a", phase: "downloading", receivedBytes: 1, totalBytes: 2, source: "origin", path: null, error: null, sourceError: "source_html" }]);
    expect(parseModelStatuses([{ id: "a", phase: "failed", source: 3, path: {}, error: 7, sourceError: false }]))
      .toEqual([{ id: "a", phase: "failed", receivedBytes: 0, totalBytes: 0, source: null, path: null, error: null, sourceError: null }]);
  });
  it("takes 0 for numbers that are not finite", () => {
    expect(parseModelStatuses([{ id: "a", phase: "missing", receivedBytes: Infinity, totalBytes: "5" }]))
      .toEqual([status({ phase: "missing" })]);
    expect(parseModelStatuses([{ id: "a", phase: "missing", receivedBytes: NaN, totalBytes: null }]))
      .toEqual([status({ phase: "missing" })]);
  });
});

describe("sizes", () => {
  it("formats sizes in MiB the way the spec quotes them", () => {
    expect(formatMiB(37529132)).toBe("35.8");
    expect(approxSize(37529132)).toBe("约 36 MB");
  });
});

describe("sourceLabel", () => {
  it("labels sources", () => {
    expect(sourceLabel("origin")).toBe("直连");
    expect(sourceLabel("mirror:ghfast.top")).toBe("镜像 ghfast.top");
    expect(sourceLabel("custom")).toBe("自定义加速");
    expect(sourceLabel("import")).toBe("本地导入");
    expect(sourceLabel("env")).toBe("环境变量");
    expect(sourceLabel(null)).toBe("");
  });
});

describe("progress", () => {
  it("writes the progress line with source", () => {
    expect(progressText(status({ receivedBytes: 12_897_485, totalBytes: 37_529_132, source: "mirror:ghfast.top" })))
      .toBe("已下载 12.3 / 35.8 MB · 来源：镜像 ghfast.top");
  });
  it("leaves out the source when there is none", () => {
    expect(progressText(status({ receivedBytes: 12_897_485, totalBytes: 37_529_132 }))).toBe("已下载 12.3 / 35.8 MB");
  });
  it("computes an integer percent clamped to 0..100", () => {
    expect(progressPercent(status({ receivedBytes: 5, totalBytes: 10 }))).toBe(50);
    expect(progressPercent(status({ receivedBytes: 1, totalBytes: 3 }))).toBe(33);
    expect(progressPercent(status({ receivedBytes: 20, totalBytes: 10 }))).toBe(100);
    expect(progressPercent(status({ receivedBytes: -5, totalBytes: 10 }))).toBe(0);
    expect(progressPercent(status({ receivedBytes: 5, totalBytes: 0 }))).toBe(0);
  });
});

describe("modelErrorText", () => {
  it("maps every machine code to Chinese", () => {
    expect(modelErrorText("network_unreachable")).toBe("无法连接下载地址。请检查网络，或设置系统代理后重试。");
    expect(modelErrorText("timeout")).toBe("下载超时，请重试。");
    expect(modelErrorText("http_status:429")).toBe("下载服务繁忙（HTTP 429），请稍后再试。");
    expect(modelErrorText("http_status:503")).toBe("下载服务繁忙（HTTP 503），请稍后再试。");
    expect(modelErrorText("http_status:500")).toBe("下载服务繁忙（HTTP 500），请稍后再试。");
    expect(modelErrorText("http_status:404")).toBe("下载地址暂时无法访问（HTTP 404）。稍后再试，或从本地文件导入。");
    expect(modelErrorText("http_status:403")).toBe("下载地址暂时无法访问（HTTP 403）。稍后再试，或从本地文件导入。");
    expect(modelErrorText("source_html")).toBe("加速服务返回了网页，已换下一个来源");
    for (const c of ["size_mismatch", "sha256_mismatch"]) expect(modelErrorText(c)).toBe("下载的文件校验不通过，已删除。请重试。");
    expect(modelErrorText("import_mismatch")).toBe("所选文件与模型不符（大小或 SHA-256 不一致），未导入。");
    for (const c of ["disk_full", "write_failed"]) expect(modelErrorText(c)).toBe("无法写入模型文件夹（空间不足或没有权限）。");
    expect(modelErrorText("install_denied")).toBe("模型文件正被使用，请先关闭去人声再试。");
    expect(modelErrorText("delete_denied")).toBe("模型文件正被使用，请先关闭去人声再删除。");
    expect(modelErrorText("cancelled")).toBe("已取消下载。");
    expect(modelErrorText("already_running")).toBe("正在下载中。");
    expect(modelErrorText("all_sources_failed")).toBe("所有下载来源都失败了。可设置系统代理、填写加速前缀，或从本地文件导入。");
    expect(modelErrorText("invalid_prefix")).toBe("下载加速前缀无效：须以 https:// 开头、以 / 结尾。");
    for (const c of ["unknown_model", "import_unsupported"]) expect(modelErrorText(c)).toBe("这个模型暂不支持此操作。");
  });
  it("falls back to a generic message that carries the code", () => {
    expect(modelErrorText("internal_error")).toBe("下载失败（internal_error），请重试。");
    expect(modelErrorText("weird")).toBe("下载失败（weird），请重试。");
    expect(modelErrorText("http_status:abc")).toBe("下载失败（http_status:abc），请重试。");
  });
});

describe("isValidMirrorPrefix", () => {
  it("validates mirror prefixes like the backend", () => {
    for (const ok of ["https://ghfast.top/", "https://a.b/c/", "https://my-proxy.example:8443/", "https://a.b:1/x/"]) expect(isValidMirrorPrefix(ok)).toBe(true);
    const long = `https://a.b/${"x".repeat(200)}/`;
    for (const bad of [
      "", "http://a.b/", "https://a.b", "https:///", "https://a b/", " https://a.b/", "javascript:alert(1)/", "https://a.b/\n", long,
      "https://a@evil/", "https://user:pw@evil/", "https://a.b?x/", "https://a.b#x/", "https://a_b/",
      "https://a.b:/", "https://a.b:0/", "https://a.b:65536/", "https://a.b:80x/", "https://:443/",
      "https://a.b:1:2/", "https://[::1]/", "https://a.b\\c/", "https://a.b:+80/",
      "https://a.b/\u00a0x/", "https://a.b/\u3000x/", "https://a.b/\u0085x/", "https://a.b/\u007fx/",
    ]) expect(isValidMirrorPrefix(bad), JSON.stringify(bad)).toBe(false);
  });
  it("accepts a prefix of exactly 200 characters and rejects 201", () => {
    const base = "https://a.b/";
    expect(isValidMirrorPrefix(base + "x".repeat(200 - base.length - 1) + "/")).toBe(true);
    expect(isValidMirrorPrefix(base + "x".repeat(201 - base.length - 1) + "/")).toBe(false);
  });
  it("counts bytes, not characters, for the length limit like the backend", () => {
    expect(isValidMirrorPrefix("https://a.b/" + "é".repeat(100) + "/")).toBe(false);
  });
});

describe("consent", () => {
  it("remembers consent per model SHA-256 set", () => {
    const m = MODEL_MANIFEST.models[0];
    expect(hasModelConsent(m)).toBe(false);
    rememberModelConsent(m);
    expect(hasModelConsent(m)).toBe(true);
    expect(hasModelConsent({ ...m, files: [{ ...m.files[0], sha256: "0".repeat(64) }] })).toBe(false);
    expect(JSON.parse(localStorage.getItem("helper-model-consent-v1")!)).toEqual({ keys: [consentKey(m)] });
  });
  it("builds the key from the id and the file hashes", () => {
    const m = MODEL_MANIFEST.models[0];
    expect(consentKey(m)).toBe(`${m.id}:${m.files[0].sha256}`);
  });
  it("does not duplicate keys and ignores corrupt storage", () => {
    const m = MODEL_MANIFEST.models[0];
    rememberModelConsent(m); rememberModelConsent(m);
    expect(JSON.parse(localStorage.getItem("helper-model-consent-v1")!).keys).toHaveLength(1);
    localStorage.setItem("helper-model-consent-v1", "{not json");
    expect(hasModelConsent(m)).toBe(false);
    localStorage.setItem("helper-model-consent-v1", JSON.stringify({ keys: "x" }));
    expect(hasModelConsent(m)).toBe(false);
    rememberModelConsent(m);
    expect(hasModelConsent(m)).toBe(true);
  });
});

describe("mirror prefix storage", () => {
  it("round-trips the prefix", () => {
    expect(readMirrorPrefix()).toBe("");
    saveMirrorPrefix("https://my.proxy/");
    expect(readMirrorPrefix()).toBe("https://my.proxy/");
    expect(JSON.parse(localStorage.getItem("helper-model-mirror-v1")!)).toEqual({ prefix: "https://my.proxy/" });
  });
  it("returns an empty string for corrupt or non-string content", () => {
    localStorage.setItem("helper-model-mirror-v1", "{nope");
    expect(readMirrorPrefix()).toBe("");
    localStorage.setItem("helper-model-mirror-v1", JSON.stringify({ prefix: 5 }));
    expect(readMirrorPrefix()).toBe("");
  });
});

describe("pinnedCommit", () => {
  it("reads the pinned commit from the origin", () => {
    expect(pinnedCommit(MODEL_MANIFEST.models[0].files[0].origin)).toBe("61df8f4aa1555ef110308d01ea92b54ace770979");
  });
  it("returns null when no 40-hex segment exists", () => {
    expect(pinnedCommit("https://github.com/a/b/raw/main/m.onnx")).toBeNull();
    expect(pinnedCommit("https://github.com/a/b/raw/" + "g".repeat(40) + "/m.onnx")).toBeNull();
  });
});

describe("throwing localStorage", () => {
  it("survives getItem and setItem that throw", () => {
    const boom = () => { throw new Error("denied"); };
    (globalThis as { localStorage?: unknown }).localStorage = { getItem: boom, setItem: boom, removeItem: boom };
    const m = MODEL_MANIFEST.models[0];
    expect(hasModelConsent(m)).toBe(false);
    expect(readMirrorPrefix()).toBe("");
    expect(() => rememberModelConsent(m)).not.toThrow();
    expect(() => saveMirrorPrefix("https://a.b/")).not.toThrow();
  });
});

describe("sourceErrorHint", () => {
  it("while downloading, says a source failed and the next one is in use", () => {
    expect(sourceErrorHint("downloading", "timeout")).toBe("某个来源失败（下载超时，请重试），已自动换用下一个");
    expect(sourceErrorHint("downloading", "http_status:503")).toBe("某个来源失败（下载服务繁忙（HTTP 503），请稍后再试），已自动换用下一个");
    expect(sourceErrorHint("downloading", "network_unreachable")).toBe("某个来源失败（无法连接下载地址。请检查网络，或设置系统代理后重试），已自动换用下一个");
    // Its own text already says the next source was taken.
    expect(sourceErrorHint("downloading", "source_html")).toBe("某个来源失败（加速服务返回了网页），已自动换用下一个");
  });
  it("after the download failed, names the last source's failure", () => {
    expect(sourceErrorHint("failed", "timeout")).toBe("最后一个来源失败：下载超时，请重试。");
    expect(sourceErrorHint("failed", "size_mismatch")).toBe("最后一个来源失败：下载的文件校验不通过，已删除。请重试。");
    // No "switched to the next one" when there was none.
    expect(sourceErrorHint("failed", "source_html")).toBe("最后一个来源失败：加速服务返回了网页。");
  });
  it("shows nothing without a code or in other phases", () => {
    expect(sourceErrorHint("downloading", null)).toBeNull();
    expect(sourceErrorHint("failed", null)).toBeNull();
    for (const phase of ["missing", "verifying", "installed"] as const) expect(sourceErrorHint(phase, "timeout")).toBeNull();
  });
});
