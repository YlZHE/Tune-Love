"""Minimal LRC parser: returns sorted (start, end, text) lines; blank lines only end the previous line."""
import re

TAG = re.compile(r"\[(\d+):(\d+(?:\.\d+)?)\]")


def parse(text):
    stamps = []
    for raw in text.splitlines():
        tags = TAG.findall(raw)
        if not tags:
            continue
        body = TAG.sub("", raw).strip()
        for minutes, seconds in tags:
            stamps.append((int(minutes) * 60 + float(seconds), body))
    stamps.sort(key=lambda s: s[0])
    lines = []
    for i, (start, body) in enumerate(stamps):
        if not body:
            continue
        end = stamps[i + 1][0] if i + 1 < len(stamps) else None
        lines.append((start, end, body))
    return lines


def first_line(lines):
    """(start, end) of the first lyric line; end falls back to start + 5 s."""
    start, end, _ = lines[0]
    return start, end if end is not None else start + 5.0
