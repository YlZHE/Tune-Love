"""Convert a 1 s separation ONNX into the DirectML-friendly file Tune Love downloads/uses.

    convert.py bytesep   --src bytesep_mobilenet_vocals_1s.onnx --out <dir>
    convert.py htdemucs  --src htdemucs_ft_vocals_1s.onnx       --out <dir>

bytesep's source is the 1 s export made by artifacts/separation-bench/src/export_bytesep.py (from the
official .pth and PQMF filters). HTDemucs's source is the StemSplitio htdemucs_ft_vocals ONNX cut to 1 s.
Steps: rewrite ConvTranspose (+ Split for HTDemucs), drop dead weights, save, check the saved file
against the source on CPU (SDR >= 100 dB), then record bytes and SHA-256 in <dir>/sha256.txt.
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
# out file, apply Split->Slice, index of the vocals stem in the (squeezed) output
MODELS = {
    "bytesep": ("bytesep-mobilenet-1s.onnx", False, 0),
    "htdemucs": ("htdemucs-ft-vocals-1s.onnx", True, 3),
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


def record(out_dir, name, size, digest):
    """sha256.txt lines: '<sha256>  <bytes>  <file>'; one line per file, replaced on rerun."""
    f = out_dir / "sha256.txt"
    lines = [l for l in (f.read_text().splitlines() if f.exists() else []) if l.split()[-1:] != [name]]
    lines.append(f"{digest}  {size}  {name}")
    f.write_text("\n".join(sorted(lines, key=lambda l: l.split()[-1])) + "\n")


def convert(kind, src, out_dir):
    name, split, vocals_index = MODELS[kind]
    out_dir.mkdir(parents=True, exist_ok=True)
    dst = out_dir / name
    m = rg.rewrite_convtranspose(onnx.load(str(src)))
    if split:
        m = rg.split_to_slice(m)
    m = rg.drop_unused_initializers(m)
    onnx.checker.check_model(m)
    onnx.save(m, str(dst))
    sdr = parity_sdr(src, dst)
    if sdr < MIN_SDR_DB:
        dst.unlink()
        raise SystemExit(f"{name}: CPU parity {sdr:.1f} dB < {MIN_SDR_DB} dB, not written")
    size, digest = dst.stat().st_size, sha256_of(dst)
    record(out_dir, name, size, digest)
    m = onnx.load(str(dst))
    dims = lambda v: [d.dim_value for d in v.type.tensor_type.shape.dim]
    outs = cpu_session(dst).get_outputs()
    return {"file": name, "bytes": size, "sha256": digest,
            "paritySdrDb": round(min(sdr, 999.0), 1),  # 999 = bit-identical
            "input": {"name": m.graph.input[0].name, "shape": dims(m.graph.input[0])},
            "outputShape": outs[0].shape, "vocalsIndex": vocals_index}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("model", choices=sorted(MODELS))
    p.add_argument("--src", required=True, type=Path)
    p.add_argument("--out", required=True, type=Path)
    a = p.parse_args()
    print(json.dumps(convert(a.model, a.src, a.out), indent=1))


if __name__ == "__main__":
    main()
