// Release-time check of every model download source (direct + mirrors).
// Hits the real network: run by hand before a release, never from tests or CI.
//
//   node scripts/check-model-mirrors.mjs [--id stemgenrt-hop128]
//
// For each `mirrorable` file and each source it requests the first 64 KiB and checks:
// status 206, Content-Range total == manifest bytes, content-type is not text/html, and the
// SHA-256 of those bytes equals the direct source's. If the direct source is unreachable the
// first passing mirror is used as the reference instead (noted in the output).
// Exit code 1 if any source fails.
import { createHash } from "node:crypto";
import { loadManifest } from "./model-manifest.mjs";

const TIMEOUT_MS = 15_000;
const RANGE_BYTES = 65_536;

/** Mirror address = prefix + original address (same rule as the app). */
const mirrorUrl = (prefix, origin) => prefix + origin;

/** Fetch the first 64 KiB of `url`. Resolves { ok, reason, hash }; never throws. */
async function probe(url, bytes) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), TIMEOUT_MS);
  try {
    const res = await fetch(url, {
      method: "GET",
      headers: { Range: `bytes=0-${RANGE_BYTES - 1}` },
      redirect: "follow",
      signal: ctl.signal,
    });
    const type = res.headers.get("content-type") ?? "";
    const range = res.headers.get("content-range") ?? "";
    // Read the body (it is at most 64 KiB if the server honours Range); abort if it does not.
    const buf = Buffer.from(await res.arrayBuffer());
    if (res.status !== 206) return { ok: false, reason: `status ${res.status} (expected 206)` };
    const total = /\/(\d+)\s*$/.exec(range)?.[1];
    if (Number(total) !== bytes) return { ok: false, reason: `Content-Range total ${total ?? "missing"} != ${bytes} (${range || "no header"})` };
    if (/text\/html/i.test(type)) return { ok: false, reason: `content-type ${type}` };
    if (buf.length !== RANGE_BYTES) return { ok: false, reason: `got ${buf.length} bytes, expected ${RANGE_BYTES}` };
    return { ok: true, reason: `206, ${type || "no content-type"}`, hash: createHash("sha256").update(buf).digest("hex") };
  } catch (e) {
    const timedOut = e?.name === "AbortError";
    return { ok: false, reason: timedOut ? `timeout after ${TIMEOUT_MS / 1000} s` : `network error: ${e?.cause?.code ?? e?.message ?? e}` };
  } finally {
    clearTimeout(timer);
  }
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--id") out.id = argv[++i];
    else throw new Error(`unknown argument ${argv[i]}`);
  }
  return out;
}

async function main() {
  const { id } = parseArgs(process.argv.slice(2));
  const manifest = loadManifest();
  const models = manifest.models.filter(m => id === undefined || m.id === id);
  if (models.length === 0) throw new Error(id === undefined ? "manifest has no models" : `unknown model ${id}`);

  let failures = 0;
  for (const model of models) {
    for (const file of model.files.filter(f => f.mirrorable)) {
      console.log(`\n${model.id}/${file.file}  (${file.bytes} bytes)`);
      const sources = [
        { label: "direct", url: file.origin },
        ...manifest.mirrors.map(prefix => ({ label: prefix, url: mirrorUrl(prefix, file.origin) })),
      ];
      const results = [];
      for (const s of sources) results.push({ ...s, ...(await probe(s.url, file.bytes)) });

      const direct = results[0];
      const reference = direct.ok ? direct : results.find(r => r.ok);
      for (const r of results) {
        let ok = r.ok;
        let reason = r.reason;
        if (ok && reference && r.hash !== reference.hash) {
          ok = false;
          reason = `first 64 KiB differ from ${reference === direct ? "direct" : reference.label}`;
        }
        if (ok && r !== reference && reference !== direct) reason += ` (compared with ${reference.label}, direct unreachable)`;
        if (ok && r === reference && reference !== direct) reason += " (reference: direct unreachable)";
        console.log(`${ok ? "PASS" : "FAIL"}  ${r.label}  ${reason}`);
        if (!ok) failures++;
      }
    }
  }
  console.log(failures === 0 ? "\nall sources passed" : `\n${failures} source(s) failed`);
  process.exitCode = failures === 0 ? 0 : 1;
}

main().catch(e => {
  console.error(e.message ?? e);
  process.exitCode = 1;
});
