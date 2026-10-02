"""Decode, separate (demucs htdemucs), chroma (librosa) and vocal F0 (torchcrepe), cached per song."""
import hashlib
import json
import subprocess
from pathlib import Path

import os

import numpy as np

DEVICE = os.environ.get("SCALE_MATCH_DEVICE", "cpu")  # offline evaluation only

SEP_RATE = 44_100
CHROMA_RATE = 22_050
CHROMA_HOP = 2048            # ~92.9 ms
F0_RATE = 16_000
F0_HOP = 160                 # 10 ms


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def decode(path, rate, channels):
    cmd = ["ffmpeg", "-v", "error", "-i", str(path), "-f", "f32le", "-ac", str(channels),
           "-ar", str(rate), "pipe:1"]
    raw = subprocess.run(cmd, check=True, capture_output=True).stdout
    return np.frombuffer(raw, dtype="<f4").reshape(-1, channels).T.copy()


def separate(stereo):
    import torch
    from demucs.apply import apply_model
    from demucs.pretrained import get_model

    model = get_model("htdemucs")
    model.eval()
    wav = torch.from_numpy(stereo)
    ref = wav.mean(0)
    mean, std = ref.mean(), ref.std() + 1e-8
    with torch.no_grad():
        out = apply_model(model, ((wav - mean) / std)[None], device=DEVICE, split=True,
                          overlap=0.25, progress=False)[0]
    out = out.cpu() * std + mean
    vocals = out[model.sources.index("vocals")].numpy()
    accompaniment = (out.sum(0) - out[model.sources.index("vocals")]).numpy()
    return vocals, accompaniment


def resample(x, src, dst):
    import librosa
    return librosa.resample(x, orig_sr=src, target_sr=dst)


def chroma(mono, rate):
    import librosa
    y = resample(mono, rate, CHROMA_RATE)
    c = librosa.feature.chroma_cqt(y=y, sr=CHROMA_RATE, hop_length=CHROMA_HOP, norm=None)
    rms = librosa.feature.rms(y=y, frame_length=CHROMA_HOP * 2, hop_length=CHROMA_HOP)[0]
    n = min(c.shape[1], len(rms))
    times = np.arange(n) * CHROMA_HOP / CHROMA_RATE
    return times, c[:, :n].T.astype(np.float32), rms[:n].astype(np.float32)


def vocal_f0(mono, rate):
    import torch
    import torchcrepe
    y = resample(mono, rate, F0_RATE)
    audio = torch.from_numpy(y.astype(np.float32))[None]
    pitch, periodicity = torchcrepe.predict(audio, F0_RATE, F0_HOP, fmin=65.0, fmax=1000.0,
                                            model="full", batch_size=1024, device=DEVICE,
                                            return_periodicity=True)
    periodicity = torchcrepe.filter.median(periodicity, 3)
    pitch = torchcrepe.filter.mean(pitch, 3)
    pitch, periodicity = pitch[0].cpu().numpy(), periodicity[0].cpu().numpy()
    frames = len(pitch)
    rms = np.array([np.sqrt(np.mean(y[i * F0_HOP:(i + 1) * F0_HOP] ** 2) + 1e-12)
                    for i in range(frames)])
    times = np.arange(frames) * F0_HOP / F0_RATE
    return times, pitch.astype(np.float32), periodicity.astype(np.float32), rms.astype(np.float32)


def load_song(path, cache_root):
    """All features for one song; heavy steps cached under cache_root/<sha256>."""
    digest = sha256(path)
    cache = Path(cache_root) / digest[:16]
    cache.mkdir(parents=True, exist_ok=True)
    feats = cache / "features.npz"
    if feats.exists():
        return digest, dict(np.load(feats))
    stereo = decode(path, SEP_RATE, 2)
    vocals, accompaniment = separate(stereo)
    mix_t, mix_c, mix_rms = chroma(stereo.mean(0), SEP_RATE)
    acc_t, acc_c, acc_rms = chroma(accompaniment.mean(0), SEP_RATE)
    f0_t, f0, period, f0_rms = vocal_f0(vocals.mean(0), SEP_RATE)
    data = dict(duration=np.float32(stereo.shape[1] / SEP_RATE),
                mix_t=mix_t, mix_c=mix_c, mix_rms=mix_rms,
                acc_t=acc_t, acc_c=acc_c, acc_rms=acc_rms,
                f0_t=f0_t, f0=f0, period=period, f0_rms=f0_rms)
    np.savez_compressed(feats, **data)
    import soundfile as sf
    # Short stems for manual spot checks only (first 60 s), 16-bit to save space.
    sf.write(cache / "vocals-60s.wav", vocals[:, :SEP_RATE * 60].T, SEP_RATE, subtype="PCM_16")
    (cache / "source.json").write_text(json.dumps({"sha256": digest, "file": Path(path).name},
                                                  ensure_ascii=False), encoding="utf-8")
    return digest, data
