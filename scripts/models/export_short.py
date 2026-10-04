"""Export the htdemucs_ft vocals specialist with a SHORT internal segment (the --src of
`convert.py htdemucs`).

    python export_short.py <out-dir> 1

Needs StemSplit's demucs-onnx (github.com/StemSplit/demucs-onnx) and Demucs (MIT) installed in the
running Python; demucs-onnx downloads the official htdemucs_ft checkpoint. Writes
<out-dir>/htdemucs_ft_vocals_<seconds>s.onnx for each length given (refuses to overwrite).

demucs_onnx's segment_seconds only changes the dummy input; HTDemucs.forward still pads
to model.segment (7.8 s). Setting model.segment on the sub-model makes the graph truly short.
"""
import copy
import sys
from pathlib import Path

from demucs_onnx.export.exporter import (_export_one, _load_checkpoint, _onnx_check,
                                         _verify_onnx_parity)
from demucs_onnx.export.patch import patch_htdemucs_for_onnx

SR = 44_100
VOCALS = 3

out_dir = Path(sys.argv[1])
seconds = [float(s) for s in sys.argv[2:]]
bag, subs, sources = _load_checkpoint("htdemucs_ft", verbose=False)
for seg in seconds:
    original = copy.deepcopy(subs[VOCALS]).eval().to("cpu")
    original.segment = seg
    n = int(seg * SR)
    path = out_dir / f"htdemucs_ft_vocals_{seg:g}s.onnx"
    if path.exists():
        raise SystemExit(f"refusing to overwrite {path}")
    _export_one(patch_htdemucs_for_onnx(copy.deepcopy(original)), path, n_samples=n, opset=17, verbose=False)
    _onnx_check(path, verbose=False)
    _verify_onnx_parity(original, path, n_samples=n, bag_index=VOCALS, stem="vocals",
                        tolerance=1e-3, verbose=True)
    print("exported", path, flush=True)
