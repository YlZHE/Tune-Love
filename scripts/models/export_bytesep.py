"""Export the ByteDance bytesep MobileNet-Subbandtime VOCALS model to a fixed-length ONNX and check
it against PyTorch (the --src of `convert.py bytesep`).

    python export_bytesep.py --ckpt bytesep_mobilenet_vocals.pth --filters-home <dir> --out-dir <dir>
        [--seconds 1 3] [--ort-python <python with onnxruntime>] [--dynamo]

Needs torch, onnx, torchlibrosa and the bytesep package (github.com/bytedance/music_source_separation,
Apache-2.0) installed in the running Python; the parity check runs `--ort-python` (default: this
Python) with numpy and onnxruntime. Inputs:
  --ckpt          Zenodo 5804160 checkpoint (MD5 197abd4c514fcc92bd22fb1fe77d5f3a).
  --filters-home  a directory holding bytesep_data/filters/f_4_64.mat (Zenodo 5513378). bytesep's
                  PQMF reads Path.home()/bytesep_data/filters, so HOME/USERPROFILE point here.
Writes <out-dir>/bytesep_mobilenet_vocals_<seconds>s.onnx and prints a JSON summary.

Empty `bytesep` / `bytesep.models` packages are registered so the pytorch-lightning training code
is never imported.
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys
import types
from pathlib import Path

ARGS = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ARGS.add_argument("--ckpt", type=Path, required=True)
ARGS.add_argument("--filters-home", type=Path, required=True)
ARGS.add_argument("--out-dir", type=Path, required=True)
ARGS.add_argument("--seconds", type=int, nargs="+", default=[1])
ARGS.add_argument("--ort-python", default=sys.executable)
ARGS.add_argument("--dynamo", action="store_true")
args = ARGS.parse_args()

HOME = args.filters_home.resolve()
# PQMF looks for filters at Path.home()/bytesep_data/filters; on Windows Path.home() reads
# USERPROFILE. Set before torch is imported.
os.environ["USERPROFILE"] = str(HOME)
os.environ["HOME"] = str(HOME)
assert Path.home() == HOME, (Path.home(), HOME)
assert (HOME / "bytesep_data/filters/f_4_64.mat").is_file(), "missing PQMF filters"
CKPT = args.ckpt.resolve()
OUT_DIR = args.out_dir.resolve()

import numpy as np  # noqa: E402
import torch  # noqa: E402
import torch.nn as nn  # noqa: E402

SITE = Path(torch.__file__).parents[1]
os.chdir(SITE)
_root = types.ModuleType("bytesep")
_root.__path__ = [str(SITE / "bytesep")]
sys.modules["bytesep"] = _root
_pkg = types.ModuleType("bytesep.models")
_pkg.__path__ = [str(SITE / "bytesep" / "models")]
sys.modules["bytesep.models"] = _pkg
from bytesep.models.mobilenet_subbandtime import MobileNet_Subbandtime  # noqa: E402
from torchlibrosa.stft import ISTFT  # noqa: E402

SR = 44_100
OPSET = 17


def load():
    m = MobileNet_Subbandtime(input_channels=2, output_channels=2, target_sources_num=1)
    m.load_state_dict(torch.load(CKPT, map_location="cpu")["model"])
    return m.eval()


def frames_for(n_samples):
    """Number of STFT frames the model sees for an N-sample input (PQMF pads 64 samples then
    decimates by 4; STFT is centered with n_fft=512, hop=110)."""
    sub = (n_samples + 64) // 4
    return (sub + 512 - 512) // 110 + 1


def use_onnx_istft(model, n_samples):
    """torchlibrosa's default ISTFT uses F.fold (aten::col2im), which only has an ONNX symbolic at
    opset 18. torchlibrosa ships an `onnx=True` variant (ConvTranspose2d overlap-add, Conv1d flip,
    precomputed window sum for a fixed frame count) which we swap in after loading weights. Its conv
    weights are deterministic constants, identical to the trained checkpoint's frozen ISTFT weights.
    """
    old = model.istft
    new = ISTFT(n_fft=old.n_fft, hop_length=old.hop_length, win_length=old.win_length, window=old.window,
                center=old.center, pad_mode=old.pad_mode, freeze_parameters=True,
                onnx=True, frames_num=frames_for(n_samples), device=None)
    assert torch.equal(new.conv_real.weight, old.conv_real.weight)
    assert torch.equal(new.conv_imag.weight, old.conv_imag.weight)
    assert torch.equal(new.ola_window, old.ola_window)
    model.istft = new.eval()
    return model


class Wrapper(nn.Module):
    def __init__(self, model):
        super().__init__()
        self.model = model

    def forward(self, waveform):  # (B, 2, N) -> (B, 2, N)
        return self.model({"waveform": waveform})["waveform"]


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def export(seconds, dynamo=False):
    n = SR * seconds
    out = OUT_DIR / f"bytesep_mobilenet_vocals_{seconds}s.onnx"
    base = load()
    # Reference output from the unmodified (fold-based) model.
    rng = np.random.RandomState(1234)
    x_np = (rng.randn(1, 2, n) * 0.1).astype(np.float32)
    x = torch.from_numpy(x_np)
    with torch.no_grad():
        ref = base({"waveform": x})["waveform"].numpy()
    use_onnx_istft(base, n)
    wrapped = Wrapper(base).eval()
    with torch.no_grad():
        ref_onnx_istft = wrapped(x).numpy()
    istft_swap_diff = float(np.abs(ref - ref_onnx_istft).max())

    torch.onnx.export(
        wrapped, (x,), str(out), opset_version=OPSET, dynamo=dynamo,
        input_names=["waveform"], output_names=["vocals"], do_constant_folding=True,
    )
    import onnx
    m = onnx.load(str(out))
    # The exporter leaves the output value_info dims unset; fill them with shape inference so the
    # declared output shape is explicit (1, 2, N).
    m = onnx.shape_inference.infer_shapes(m, strict_mode=True)
    # Inference stops at the dynamic Reshape in the ISTFT tail; N is fixed per file, so declare it.
    shp = m.graph.output[0].type.tensor_type.shape
    del shp.dim[:]
    for d in (1, 2, n):
        shp.dim.add().dim_value = d
    onnx.checker.check_model(m)
    onnx.save(m, str(out))
    io = {
        "inputs": [(i.name, [d.dim_value for d in i.type.tensor_type.shape.dim]) for i in m.graph.input],
        "outputs": [(o.name, [d.dim_value for d in o.type.tensor_type.shape.dim]) for o in m.graph.output],
        "opset": [(op.domain, op.version) for op in m.opset_import],
        "ops": sorted({nd.op_type for nd in m.graph.node}),
    }

    # Parity check with onnxruntime (CPU), possibly in another Python.
    scratch = OUT_DIR / f"_parity_{seconds}s.npz"
    np.savez(scratch, x=x_np, ref=ref)
    code = (
        "import sys, numpy as np, onnxruntime as ort\n"
        "d = np.load(sys.argv[2]); so = ort.SessionOptions(); so.intra_op_num_threads = 1\n"
        "s = ort.InferenceSession(sys.argv[1], so, providers=['CPUExecutionProvider'])\n"
        "y = s.run(None, {s.get_inputs()[0].name: d['x']})[0]\n"
        "print(__import__('json').dumps({'maxAbsDiff': float(np.abs(y - d['ref']).max()),"
        " 'refMaxAbs': float(np.abs(d['ref']).max()), 'outShape': list(y.shape)}))\n"
    )
    r = subprocess.run([args.ort_python, "-c", code, str(out), str(scratch)], capture_output=True, text=True)
    scratch.unlink(missing_ok=True)
    if r.returncode != 0:
        raise RuntimeError(r.stderr)
    parity = json.loads(r.stdout.strip().splitlines()[-1])
    return {"seconds": seconds, "samples": n, "file": out.name, "sha256": sha256(out),
            "bytes": out.stat().st_size, "exporter": "dynamo" if dynamo else "torchscript",
            "istftSwapMaxAbsDiff": istft_swap_diff, **io, **parity}


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    torch.set_num_threads(1)
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    results = [export(s, dynamo=args.dynamo) for s in args.seconds]
    print(json.dumps(results, indent=1))


if __name__ == "__main__":
    main()
