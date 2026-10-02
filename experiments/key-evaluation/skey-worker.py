"""Local-only, CPU-only S-KEY worker; no file discovery or waveform persistence."""
import base64
import contextlib
import hashlib
import json
import os
from pathlib import Path
import sys
import time

START = time.perf_counter()
ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "artifacts/key-engine-evaluation/sources/skey-918b83d273568d5041569bb8068843d19a335726"
CHECKPOINT = SOURCE / "skey/models/skey.pt"
EXPECTED_HASH = "78dfd0ad4fa9434bf7cec70a25934b7c575bda9c80e994700140770ad3a5ead4"
os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TORCH_HOME"] = str(ROOT / "artifacts/key-engine-evaluation/torch-cache")
os.environ["OMP_NUM_THREADS"] = "1"
os.environ["MKL_NUM_THREADS"] = "1"
sys.path.insert(0, str(SOURCE))

with contextlib.redirect_stdout(sys.stderr):
    import numpy as np
    import torch
    import torchaudio
    from skey.key_detection import load_model_components, key_map

    if hashlib.sha256(CHECKPOINT.read_bytes()).hexdigest() != EXPECTED_HASH:
        raise ValueError("Pinned S-KEY checkpoint hash mismatch")
    safe = [(np._core.multiarray.scalar, "numpy.core.multiarray.scalar"), np.dtype, np.dtypes.Float64DType]
    with torch.serialization.safe_globals(safe):
        checkpoint = torch.load(CHECKPOINT, map_location="cpu", weights_only=True)
    model_rate = int(checkpoint["audio"]["sr"])
    if model_rate != 22050:
        raise ValueError("Unexpected checkpoint sample rate")
    torch.set_num_threads(1)
    torch.set_num_interop_threads(1)
    hcqt, chromanet, crop = load_model_components(checkpoint, torch.device("cpu"))
    resample = torchaudio.transforms.Resample(48000, model_rate)

def emit(value):
    print(json.dumps(value, allow_nan=False), flush=True)

emit({"type": "ready", "engine": "Deezer S-KEY CPU", "commit": "918b83d273568d5041569bb8068843d19a335726",
      "modelSha256": EXPECTED_HASH, "sampleRate": model_rate, "torch": torch.__version__,
      "threads": torch.get_num_threads(), "loadMs": (time.perf_counter()-START)*1000,
      "inputPolicy": "shared mono/gain; resample only, no extra upstream peak normalization"})

LIMIT = 2200000
while True:
    line = sys.stdin.buffer.readline(LIMIT + 1)
    if not line:
        break
    if len(line) > LIMIT or not line.endswith(b"\n"):
        raise ValueError("Oversized or truncated worker request")
    request = {}
    try:
        request = json.loads(line)
        if not isinstance(request, dict):
            raise ValueError("Request must be object")
        if (not isinstance(request.get("id"), str) or len(request["id"]) > 128 or
            request.get("sampleRate") != 48000 or request.get("channels") != 1 or
            not isinstance(request.get("pcm"), str)):
            raise ValueError("Invalid request metadata")
        raw = base64.b64decode(request["pcm"], validate=True)
        if len(raw) % 4 or not 6*48000*4 <= len(raw) <= 8*48000*4:
            raise ValueError("Invalid PCM size")
        pcm = np.frombuffer(raw, dtype="<f4").copy()
        if not np.isfinite(pcm).all():
            raise ValueError("Nonfinite PCM")
        begin = time.perf_counter()
        with torch.inference_mode():
            waveform = resample(torch.from_numpy(pcm).unsqueeze(0))
            feature = time.perf_counter()
            # Matches upstream infer_key; ChromaNet already applies softmax.
            cropped = crop(hcqt(waveform.unsqueeze(0)), torch.zeros(1))
            scores = chromanet(cropped).mean(dim=0)
            values = scores.tolist()
            winner = int(scores.argmax())
        end = time.perf_counter()
        if len(values) != 24 or not np.isfinite(values).all():
            raise ValueError("Invalid model output")
        emit({"id": request["id"], "raw": {"label": key_map[winner], "winnerIndex": winner, "scores": values},
              "preprocessMs": (feature-begin)*1000, "inferenceMs": (end-feature)*1000, "totalMs": (end-begin)*1000})
    except Exception as error:
        emit({"id": request.get("id") if isinstance(request, dict) else None, "error": str(error)})
