"""Side-by-side driver for the Hekaton scoreboard.

Sends `computer` calls to a running daemon over HTTP, records wall time, response size, text
characters, image count and an input-token estimate, saves every returned image to disk, and prints
median and p95 per call name. Images never reach the calling agent's context.

    python bench/drive.py --target new --call '{"action":"list_windows"}' --repeat 5
    python bench/drive.py --target old --suite bench/suites/baseline.json --out bench/runs/old

Targets:
    old  installed daemon, port 47811, state %LOCALAPPDATA%\\winhands
    new  side-by-side daemon, port 47911, state HEKATON_TEST_STATE (default
         C:\\Users\\ikill\\tools\\hekaton-test-state\\winhands)

Token estimate: text chars / 4, plus per image ceil(w*h/750) after the Anthropic API downscale
(long edge <= 1568 px and <= 1.15 MP). It is an estimate, labelled as such in every report.
"""

from __future__ import annotations

import argparse
import base64
import json
import math
import os
import statistics
import struct
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

TARGETS = {
    "old": (47811, Path(os.environ.get("LOCALAPPDATA", "")) / "winhands"),
    "new": (
        47911,
        Path(os.environ.get("HEKATON_TEST_STATE", r"C:\Users\ikill\tools\hekaton-test-state\winhands")),
    ),
}


def image_size(raw: bytes) -> tuple[int, int]:
    if raw[:8] == b"\x89PNG\r\n\x1a\n":
        w, h = struct.unpack(">II", raw[16:24])
        return w, h
    if raw[:2] == b"\xff\xd8":
        i = 2
        while i < len(raw):
            if raw[i] != 0xFF:
                i += 1
                continue
            marker = raw[i + 1]
            if marker in (0xC0, 0xC1, 0xC2):
                h, w = struct.unpack(">HH", raw[i + 5 : i + 9])
                return w, h
            seg = struct.unpack(">H", raw[i + 2 : i + 4])[0]
            i += 2 + seg
    if raw[:4] == b"RIFF" and raw[8:12] == b"WEBP":
        chunk = raw[12:16]
        if chunk == b"VP8X":
            w = 1 + int.from_bytes(raw[24:27], "little")
            h = 1 + int.from_bytes(raw[27:30], "little")
            return w, h
    return 0, 0


def image_tokens(w: int, h: int) -> int:
    if not w or not h:
        return 0
    scale = min(1.0, 1568 / max(w, h), math.sqrt(1_150_000 / (w * h)))
    return math.ceil((w * scale) * (h * scale) / 750)


def call(port: int, token: str, args: dict, timeout: float) -> tuple[int, bytes, float]:
    body = json.dumps(args).encode()
    deadline = int((time.time() + timeout) * 1000)
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/computer",
        data=body,
        method="POST",
        headers={
            "X-Winhands-Token": token,
            "X-Hands-Deadline-Unix-Ms": str(deadline),
            "Content-Type": "application/json",
        },
    )
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            data, status = r.read(), r.status
    except urllib.error.HTTPError as e:
        data, status = e.read(), e.code
    return status, data, (time.perf_counter() - t0) * 1000


def pct(xs: list[float], p: float) -> float:
    xs = sorted(xs)
    if not xs:
        return 0.0
    k = (len(xs) - 1) * p
    lo, hi = math.floor(k), math.ceil(k)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", choices=TARGETS, required=True)
    ap.add_argument("--call", help="one JSON args object")
    ap.add_argument("--name", default="call")
    ap.add_argument("--suite", help="JSON list of {name, args, repeat}")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--timeout", type=float, default=60)
    ap.add_argument("--out", default=None, help="folder for results.jsonl and images")
    ap.add_argument("--print-text", action="store_true", help="print each result text (truncated)")
    a = ap.parse_args()

    port, state = TARGETS[a.target]
    token = (state / "token").read_text().strip()
    out = Path(a.out or f"bench/runs/{a.target}-{time.strftime('%Y%m%d-%H%M%S')}")
    (out / "img").mkdir(parents=True, exist_ok=True)

    if a.suite:
        cases = json.loads(Path(a.suite).read_text())
    else:
        cases = [{"name": a.name, "args": json.loads(a.call), "repeat": a.repeat}]

    rows = []
    with open(out / "results.jsonl", "a", encoding="utf-8") as log:
        for case in cases:
            for i in range(int(case.get("repeat", 1))):
                status, data, wall = call(port, token, case["args"], a.timeout)
                try:
                    res = json.loads(data)
                except ValueError:
                    res = {"raw": data[:200].decode(errors="replace")}
                text = res.get("result") or res.get("error") or ""
                imgs = [c for c in res.get("content", []) if c.get("type") == "image"]
                itok = 0
                for j, im in enumerate(imgs):
                    raw = base64.b64decode(im["data"])
                    w, h = image_size(raw)
                    itok += image_tokens(w, h)
                    ext = {"image/png": "png", "image/jpeg": "jpg", "image/webp": "webp"}.get(
                        im.get("mimeType", ""), "bin"
                    )
                    (out / "img" / f"{case['name']}-{i}-{j}.{ext}").write_bytes(raw)
                row = {
                    "name": case["name"],
                    "i": i,
                    "status": status,
                    "ok": res.get("ok"),
                    "wall_ms": round(wall, 1),
                    "resp_bytes": len(data),
                    "text_chars": len(text),
                    "text": text,
                    "images": len(imgs),
                    "est_tokens": math.ceil(len(text) / 4) + itok,
                    "metrics": res.get("metrics"),
                    "failed_step": res.get("failed_step"),
                    "error": res.get("error"),
                }
                rows.append(row)
                log.write(json.dumps(row) + "\n")
                if a.print_text:
                    print(f"--- {case['name']}#{i} {status}: {text[:600]}")
                if case.get("pause_ms"):
                    time.sleep(case["pause_ms"] / 1000)

    print(f"target={a.target} port={port} out={out}")
    print(f"{'name':28} {'n':>3} {'ok':>3} {'med_ms':>8} {'p95_ms':>8} {'chars':>7} {'imgs':>4} {'tok~':>6}")
    for name in dict.fromkeys(r["name"] for r in rows):
        rs = [r for r in rows if r["name"] == name]
        walls = [r["wall_ms"] for r in rs]
        print(
            f"{name:28} {len(rs):>3} {sum(1 for r in rs if r['ok']):>3} {statistics.median(walls):>8.1f} "
            f"{pct(walls, 0.95):>8.1f} {statistics.median(r['text_chars'] for r in rs):>7.0f} "
            f"{statistics.median(r['images'] for r in rs):>4.0f} {statistics.median(r['est_tokens'] for r in rs):>6.0f}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
