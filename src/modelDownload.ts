import manifestJson from "../src-tauri/models.json";

// Frontend half of the in-app model download: the manifest (the same file the Rust side
// compiles in), status parsing, Chinese copy, size formats and the two persisted choices
// (download consent and the user's acceleration prefix).

export interface ModelFile { file: string; bytes: number; sha256: string; origin: string; mirrorable: boolean }
export interface ModelInfo {
  id: string;
  name: string;
  tier: "realtime" | "quality";
  files: ModelFile[];
  sampleRate: number;
  latencyMs: number;
  runtime: "cpu" | "cuda";
  license: { code: string; weights: string; trainingData: string[] };
  source: string;
}

export const MODEL_MANIFEST = manifestJson as unknown as { version: number; mirrors: string[]; models: ModelInfo[] };
export const STEMGENRT_ID = "stemgenrt-hop128";

export type ModelPhase = "missing" | "downloading" | "verifying" | "installed" | "failed";
export interface ModelStatus {
  id: string;
  phase: ModelPhase;
  receivedBytes: number;
  totalBytes: number;
  // Label of the source in use: origin, mirror:<host>, custom, import or env.
  source: string | null;
  path: string | null;
  // Machine code of the last failure of the whole run.
  error: string | null;
  // Machine code of the most recent per-source failure in the current or last run.
  sourceError: string | null;
}

const PHASES: readonly ModelPhase[] = ["missing", "downloading", "verifying", "installed", "failed"];

const finiteOrZero = (value: unknown) => typeof value === "number" && Number.isFinite(value) ? value : 0;
const stringOrNull = (value: unknown) => typeof value === "string" ? value : null;

// IPC replies are untyped: anything that is not an array is rejected, and entries without
// a known id and phase are dropped rather than guessed.
export function parseModelStatuses(value: unknown): ModelStatus[] | null {
  if (!Array.isArray(value)) return null;
  const out: ModelStatus[] = [];
  for (const item of value) {
    if (!item || typeof item !== "object") continue;
    const raw = item as Record<string, unknown>;
    if (typeof raw.id !== "string" || typeof raw.phase !== "string" || !PHASES.includes(raw.phase as ModelPhase)) continue;
    out.push({
      id: raw.id,
      phase: raw.phase as ModelPhase,
      receivedBytes: finiteOrZero(raw.receivedBytes),
      totalBytes: finiteOrZero(raw.totalBytes),
      source: stringOrNull(raw.source),
      path: stringOrNull(raw.path),
      error: stringOrNull(raw.error),
      sourceError: stringOrNull(raw.sourceError),
    });
  }
  return out;
}

export const modelTotalBytes = (m: ModelInfo) => m.files.reduce((sum, f) => sum + f.bytes, 0);

// Sizes are computed in MiB but written "MB", the way the spec quotes them.
export const formatMiB = (bytes: number) => (bytes / 1048576).toFixed(1);
export const approxSize = (bytes: number) => `约 ${Math.round(bytes / 1048576)} MB`;

export function sourceLabel(source: string | null): string {
  if (!source) return "";
  if (source === "origin") return "直连";
  if (source === "custom") return "自定义加速";
  if (source === "import") return "本地导入";
  if (source === "env") return "环境变量";
  if (source.startsWith("mirror:")) return `镜像 ${source.slice("mirror:".length)}`;
  return source;
}

export function progressText(s: ModelStatus): string {
  const base = `已下载 ${formatMiB(s.receivedBytes)} / ${formatMiB(s.totalBytes)} MB`;
  const label = sourceLabel(s.source);
  return label ? `${base} · 来源：${label}` : base;
}

export function progressPercent(s: ModelStatus): number {
  if (!(s.totalBytes > 0)) return 0;
  return Math.min(100, Math.max(0, Math.floor((s.receivedBytes / s.totalBytes) * 100)));
}

export function modelErrorText(code: string): string {
  const http = /^http_status:(\d{1,3})$/.exec(code);
  if (http) {
    const status = Number(http[1]);
    return status === 429 || (status >= 500 && status <= 599)
      ? `下载服务繁忙（HTTP ${status}），请稍后再试。`
      : `下载地址暂时无法访问（HTTP ${status}）。稍后再试，或从本地文件导入。`;
  }
  switch (code) {
    case "network_unreachable": return "无法连接下载地址。请检查网络，或设置系统代理后重试。";
    case "timeout": return "下载超时，请重试。";
    case "source_html": return "加速服务返回了网页，已换下一个来源";
    case "size_mismatch":
    case "sha256_mismatch": return "下载的文件校验不通过，已删除。请重试。";
    case "import_mismatch": return "所选文件与模型不符（大小或 SHA-256 不一致），未导入。";
    case "disk_full":
    case "write_failed": return "无法写入模型文件夹（空间不足或没有权限）。";
    case "install_denied": return "模型文件正被使用，请先关闭去人声再试。";
    case "delete_denied": return "模型文件正被使用，请先关闭去人声再删除。";
    case "cancelled": return "已取消下载。";
    case "already_running": return "正在下载中。";
    case "all_sources_failed": return "所有下载来源都失败了。可设置系统代理、填写加速前缀，或从本地文件导入。";
    case "invalid_prefix": return "下载加速前缀无效：须以 https:// 开头、以 / 结尾。";
    case "unknown_model":
    case "import_unsupported": return "这个模型暂不支持此操作。";
    default: return `下载失败（${code}），请重试。`;
  }
}

// The hint line for the most recent per-source failure (`sourceError`): while downloading the
// download has moved on to the next source; once failed it was the last source. Null when
// there is nothing to show.
export function sourceErrorHint(phase: ModelPhase, code: string | null): string | null {
  if (!code) return null;
  const html = code === "source_html";
  if (phase === "downloading") {
    const reason = html ? "加速服务返回了网页" : modelErrorText(code).replace(/。$/, "");
    return `某个来源失败（${reason}），已自动换用下一个`;
  }
  // source_html's own text says the next source was taken, which is not so for the last one.
  if (phase === "failed") return `最后一个来源失败：${html ? "加速服务返回了网页。" : modelErrorText(code)}`;
  return null;
}

// Mirrors `valid_prefix` in src-tauri/src/devocal/model/manifest.rs: https:// + host
// ([A-Za-z0-9.-]+, optional :port 1-65535), ends with "/", no whitespace or control
// characters, at most 200 bytes. The host rule keeps userinfo, queries and fragments out.
const MAX_PREFIX_BYTES = 200;
// Unicode White_Space (what Rust's char::is_whitespace uses) or a Cc control character.
const WHITESPACE_CODES = new Set([0x1680, 0x2028, 0x2029, 0x202f, 0x205f, 0x3000]);
function isWhitespaceOrControl(code: number): boolean {
  return code <= 0x20 || (code >= 0x7f && code <= 0xa0) || (code >= 0x2000 && code <= 0x200a) || WHITESPACE_CODES.has(code);
}

function validAuthority(authority: string): boolean {
  const colon = authority.indexOf(":");
  const host = colon < 0 ? authority : authority.slice(0, colon);
  const port = colon < 0 ? null : authority.slice(colon + 1);
  if (!/^[A-Za-z0-9.-]+$/.test(host)) return false;
  if (port === null) return true;
  return /^[0-9]+$/.test(port) && Number(port) >= 1 && Number(port) <= 65535;
}

export function isValidMirrorPrefix(s: string): boolean {
  const scheme = "https://";
  if (!s.startsWith(scheme)) return false;
  return new TextEncoder().encode(s).length <= MAX_PREFIX_BYTES
    && s.endsWith("/")
    && ![...s].some(c => isWhitespaceOrControl(c.codePointAt(0)!))
    && validAuthority(s.slice(scheme.length).split("/")[0]);
}

// The commit the origin URL is pinned to: its first 40-hex path segment.
export function pinnedCommit(origin: string): string | null {
  return origin.split("/").find(segment => /^[0-9a-fA-F]{40}$/.test(segment)) ?? null;
}

export const consentKey = (m: ModelInfo) => `${m.id}:${m.files.map(f => f.sha256).join(",")}`;

const CONSENT_STORAGE_KEY = "helper-model-consent-v1";
const MIRROR_STORAGE_KEY = "helper-model-mirror-v1";

function readConsentKeys(): string[] {
  try {
    const keys = JSON.parse(localStorage.getItem(CONSENT_STORAGE_KEY) ?? "null")?.keys;
    return Array.isArray(keys) ? keys.filter((k): k is string => typeof k === "string") : [];
  } catch { return []; }
}

export function hasModelConsent(m: ModelInfo): boolean {
  return readConsentKeys().includes(consentKey(m));
}

export function rememberModelConsent(m: ModelInfo): void {
  try {
    const keys = readConsentKeys();
    const key = consentKey(m);
    if (!keys.includes(key)) keys.push(key);
    localStorage.setItem(CONSENT_STORAGE_KEY, JSON.stringify({ keys }));
  } catch { /* storage unavailable: the box is shown again next time */ }
}

export function readMirrorPrefix(): string {
  try {
    const prefix = JSON.parse(localStorage.getItem(MIRROR_STORAGE_KEY) ?? "null")?.prefix;
    return typeof prefix === "string" ? prefix : "";
  } catch { return ""; }
}

export function saveMirrorPrefix(prefix: string): void {
  try { localStorage.setItem(MIRROR_STORAGE_KEY, JSON.stringify({ prefix })); }
  catch { /* storage unavailable: the prefix is not remembered */ }
}
