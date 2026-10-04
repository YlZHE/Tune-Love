"""Convert a 1 s separation ONNX into the DirectML-friendly file Tune Love downloads/uses.

    convert.py bytesep   --src bytesep_mobilenet_vocals_1s.onnx --out <dir>
    convert.py htdemucs  --src htdemucs_ft_vocals_1s.onnx       --out <dir>

Provenance of the --src files (made by scripts in artifacts/separation-bench/src, not stored in git):
  bytesep:  Zenodo 5804160 checkpoint .pth (MD5 197abd4c514fcc92bd22fb1fe77d5f3a) + Zenodo 5513378
            PQMF .mat files -> export_bytesep.py (torch.onnx.export, opset 17, 1 s window) -> 1 s ONNX
            -> convert.py.
  htdemucs: official Demucs htdemucs_ft checkpoint, vocals sub-model -> export_short.py (demucs-onnx by
            StemSplit, 1 s window) -> 1 s ONNX -> convert.py. It is re-exported from the checkpoint, not
            cut from StemSplitio's full-length Hugging Face ONNX (only a research reference).

Steps: rewrite ConvTranspose (+ Split for HTDemucs), check the rewrite counts, drop dead weights, save,
check the saved file against the source on CPU (SDR >= 100 dB), then record bytes and SHA-256 of the
output and of --src in <dir>/sha256.txt.
"""
import argparse
import hashlib
import json
import sys
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rewrite_graph as rg  # noqa: E402

MIN_SDR_DB = 100.0
# out file, apply Split->Slice, index of the vocals stem in the (squeezed) output,
# expected number of rewritten (ConvTranspose, Split) nodes
MODELS = {
    "bytesep": ("bytesep-mobilenet-1s.onnx", False, 0, (4, 0)),
    "htdemucs": ("htdemucs-ft-vocals-1s.onnx", True, 3, (2, 48)),
}


def sdr_db(ref, test):
    err = float(np.sum((ref.astype(np.float64) - test) ** 2))
    return float("inf") if err == 0 else 10 * np.log10(float(np.sum(ref.astype(np.float64) ** 2)) / err)


def cpu_session(path):
    so = ort.SessionOptions()
    so.intra_op_num_threads = 2  # keep the conversion from saturating the machine
    so.log_severity_level = 3
    return ort.InferenceSession(str(path), so, providers=["CPUExecutionProvider"])


def parity_sdr(src, dst):
    a, b = cpu_session(src), cpu_session(dst)
    inp = a.get_inputs()[0]
    x = (np.random.RandomState(1).randn(*inp.shape) * 0.1).astype(np.float32)
    return sdr_db(a.run(None, {inp.name: x})[0], b.run(None, {inp.name: x})[0])


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def record(out_dir, name, size, digest, src):
    """sha256.txt: per file a '# <file> converted from <src> (sha256, bytes)' comment line followed by
    '<sha256>  <bytes>  <file>'; both are replaced on rerun."""
    f = out_dir / "sha256.txt"
    old = f.read_text().splitlines() if f.exists() else []
    keep = [l for l in old if l.split()[-1:] != [name] and not l.startswith(f"# {name} ")]
    note = f"# {name} converted from {src.name} (sha256 {sha256_of(src)}, {src.stat().st_size} bytes)"
    entries = keep + [note, f"{digest}  {size}  {name}"]
    pairs = sorted((entries[i], entries[i + 1]) for i in range(0, len(entries), 2))
    f.write_text("\n".join(l for p in pairs for l in p) + "\n")


def convert(kind, src, out_dir):
    name, split, vocals_index, expected = MODELS[kind]
    out_dir.mkdir(parents=True, exist_ok=True)
    dst = out_dir / name
    if dst.resolve() == src.resolve():
        raise SystemExit(f"--out would overwrite --src ({src})")
    m = onnx.load(str(src))
    before = (rg.count_ops(m, "ConvTranspose"), rg.count_ops(m, "Split"))
    m = rg.rewrite_convtranspose(m)
    if split:
        m = rg.split_to_slice(m)
    m = rg.drop_unused_initializers(m)
    after = (rg.count_ops(m, "ConvTranspose"), rg.count_ops(m, "Split"))
    rewritten = (before[0] - after[0], before[1] - after[1])
    print(f"{name}: rewrote ConvTranspose {rewritten[0]}, Split {rewritten[1]}; "
          f"left ConvTranspose {after[0]}, Split {after[1]}", flush=True)
    if rewritten != expected:
        raise SystemExit(f"{name}: rewrote (ConvTranspose, Split) = {rewritten}, expected {expected}; not written")
    if rg.count_large_convtranspose(m) or (split and after[1]):
        raise SystemExit(f"{name}: a large ConvTranspose or a Split remains; not written")
    onnx.checker.check_model(m)
    onnx.save(m, str(dst))
    sdr = parity_sdr(src, dst)
    if sdr < MIN_SDR_DB:
        dst.unlink()
        raise SystemExit(f"{name}: CPU parity {sdr:.1f} dB < {MIN_SDR_DB} dB, not written")
    size, digest = dst.stat().st_size, sha256_of(dst)
    record(out_dir, name, size, digest, src)
    m = onnx.load(str(dst))
    dims = lambda v: [d.dim_value for d in v.type.tensor_type.shape.dim]
    outs = cpu_session(dst).get_outputs()
    return {"file": name, "bytes": size, "sha256": digest,
            "paritySdrDb": round(min(sdr, 999.0), 1),  # 999 = bit-identical
            "input": {"name": m.graph.input[0].name, "shape": dims(m.graph.input[0])},
            "outputShape": outs[0].shape, "vocalsIndex": vocals_index,
            "rewrittenConvTranspose": rewritten[0], "rewrittenSplit": rewritten[1],
            "srcSha256": sha256_of(src)}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("model", choices=sorted(MODELS))
    p.add_argument("--src", required=True, type=Path)
    p.add_argument("--out", required=True, type=Path)
    a = p.parse_args()
    print(json.dumps(convert(a.model, a.src, a.out), indent=1))


if __name__ == "__main__":
    main()
