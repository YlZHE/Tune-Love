"""Generate StemgenRT reference fixtures for the Rust `stemgen` tests.

Run from `devocal/engine` with the separation-bench venv (onnxruntime 1.24.4):

    ../../artifacts/separation-bench/venv/Scripts/python tools/stemgen_reference.py [model.onnx]

The model path defaults to `../../artifacts/separation-bench/models/hop128.onnx`.
Input is synthetic (seeded noise + sines), never a song. Outputs, both little-endian
float32, interleaved stereo, same length (a whole number of 128-frame blocks):

- tests/fixtures/stemgen-input.f32: the blocks fed to the model, in order;
- tests/fixtures/stemgen-accompaniment.f32: the unclipped per-block streaming output,
  block k = input block k-1 (zeros for k = 0) minus vocals (stems[2]) of output k.

The loop mirrors `artifacts/separation-bench/src/quality.py::stemgenrt`.
"""

import sys
from pathlib import Path

import numpy as np
import onnxruntime as ort

SR = 44100
HOP = 128
SEED = 1234
SECONDS = 2.0

HERE = Path(__file__).resolve().parent
ENGINE = HERE.parent
FIXTURES = ENGINE / "tests" / "fixtures"
DEFAULT_MODEL = ENGINE.parent.parent / "artifacts" / "separation-bench" / "models" / "hop128.onnx"


def synthetic_mix():
    rng = np.random.default_rng(SEED)
    n = int(SR * SECONDS)
    t = np.arange(n) / SR
    left = 0.3 * np.sin(2 * np.pi * 220.0 * t) + 0.1 * np.sin(2 * np.pi * 659.25 * t)
    right = 0.3 * np.sin(2 * np.pi * 330.0 * t) + 0.1 * np.sin(2 * np.pi * 440.0 * t)
    mix = np.stack([left, right]) + 0.05 * rng.standard_normal((2, n))
    # Pad to whole blocks plus one zero block to flush the one-hop latency.
    mix = np.pad(mix, ((0, 0), (0, (-n) % HOP + HOP)))
    return mix.astype(np.float32)


def main():
    model = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_MODEL
    o = ort.SessionOptions()
    o.intra_op_num_threads = 1
    o.inter_op_num_threads = 1
    s = ort.InferenceSession(str(model), o, providers=["CPUExecutionProvider"])
    state_names = [i.name for i in s.get_inputs() if i.name != "audio_chunk"]
    states = {i.name: np.zeros(i.shape, np.float32) for i in s.get_inputs() if i.name != "audio_chunk"}

    mix = synthetic_mix()
    prev = np.zeros((2, HOP), np.float32)
    acc = []
    for k in range(mix.shape[1] // HOP):
        block = mix[:, k * HOP:(k + 1) * HOP]
        res = s.run(None, dict(states, audio_chunk=block[None]))
        vocals = res[0][0][2]  # output k belongs to input k-1
        acc.append(prev - vocals)
        prev = block
        states = {name: res[j + 1] for j, name in enumerate(state_names)}
    acc = np.concatenate(acc, axis=-1)

    FIXTURES.mkdir(parents=True, exist_ok=True)
    # (2, n) channel-first -> interleaved little-endian float32.
    mix.T.astype("<f4").tofile(FIXTURES / "stemgen-input.f32")
    acc.T.astype("<f4").tofile(FIXTURES / "stemgen-accompaniment.f32")
    print(f"onnxruntime {ort.__version__}; {mix.shape[1]} frames, {mix.shape[1] // HOP} blocks -> {FIXTURES}")


if __name__ == "__main__":
    main()
