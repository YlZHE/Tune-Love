"""Cache features for every .mp3 in a folder (read-only on the source folder)."""
import argparse
import sys
import time
import traceback
from pathlib import Path

from features import load_song

parser = argparse.ArgumentParser()
parser.add_argument("--songs", required=True)
parser.add_argument("--cache", required=True)
args = parser.parse_args()
sys.stdout.reconfigure(encoding="utf-8")
for path in sorted(Path(args.songs).glob("*.mp3")):
    clock = time.time()
    try:
        digest, data = load_song(path, args.cache)
    except Exception:
        print(f"FAILED\t{path.name}\t{traceback.format_exc().splitlines()[-1]}", flush=True)
        continue
    print(f"{path.name}\t{digest[:16]}\t{float(data['duration']):.1f}s\t{time.time() - clock:.1f}s",
          flush=True)
