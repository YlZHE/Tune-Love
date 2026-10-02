# StemgenRT reference fixtures

The `*.f32` files here are generated locally and git-ignored (they are derived from the
model weights, which are never committed). Input is synthetic: 2 s of seeded (1234)
stereo noise plus sines, padded to whole 128-frame blocks plus one zero block.

Generate them from `devocal/engine` with the separation-bench venv
(onnxruntime 1.24.4 at generation time):

```sh
../../artifacts/separation-bench/venv/Scripts/python tools/stemgen_reference.py
```

An optional first argument overrides the model path (default
`../../artifacts/separation-bench/models/hop128.onnx`, SHA-256
`77164d6a581fafb2a31f53fd8ffde44c07cf618472952a4cdba14e68dda3b8b9`).

Files (little-endian `f32`, interleaved stereo, equal length):

- `stemgen-input.f32` — the blocks fed to the model;
- `stemgen-accompaniment.f32` — the unclipped per-block streaming output: block k is
  input block k−1 (zeros for k = 0) minus the vocals stem of output k.

Then run the ignored tests (Git Bash):

```sh
STEMGENRT_ONNX="<abs path to hop128.onnx>" cargo test -p devocal-engine stemgen -- --ignored
```
