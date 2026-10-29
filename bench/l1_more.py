"""Gauntlet L1, part 2: Calculator, Explorer and window operations, checked against real state.

    python bench/l1_more.py --target new [--only calc,explorer,window]

Every window it touches is one it opened itself (open_app reuse:false, or the folder action on a
fresh test folder) and is addressed by the w-ref that call returned, never by title, so the owner's
own windows are never selected. Pass or fail comes from the disk and from list_windows, not from
the tool's own success text.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import drive  # noqa: E402

ROOT = Path(r"C:\Users\ikill\tools\hekaton-test-state\gauntlet\l1-more")


class Client:
    def __init__(self, target: str):
        self.port, state = drive.TARGETS[target]
        self.token = (state / "token").read_text().strip()
        self.log: list[dict] = []

    def __call__(self, args: dict, timeout: float = 60) -> tuple[bool, str, dict]:
        args = {"shot": False, "profile": True, **args}
        status, data, wall = drive.call(self.port, self.token, args, timeout)
        res = json.loads(data)
        text = res.get("result") or res.get("error") or ""
        self.log.append({"args": args, "ok": res.get("ok"), "wall_ms": round(wall, 1), "text": text})
        return bool(res.get("ok")), text, res


def wref(text: str) -> str | None:
    m = re.search(r"\b(w\d+)\b", text)
    return m.group(1) if m else None


def window_line(c: Client, ref: str) -> str | None:
    _, text, _ = c({"action": "list_windows"})
    for line in text.splitlines():
        if line.startswith(ref + " "):
            return line
    return None


def rect_of(line: str | None) -> tuple[int, ...] | None:
    if not line:
        return None
    m = re.search(r"(-?\d+),(-?\d+),(\d+),(\d+)\s*$", line)
    return tuple(int(v) for v in m.groups()) if m else None


def calc(c: Client) -> dict:
    """12345 * 6789 = 83,810,205, typed as keys, answer read back from the display element."""
    ok, text, _ = c({"actions": [
        {"action": "open_app", "text": "calculator", "reuse": False},
        {"action": "key", "text": "escape"},
        {"action": "type", "text": "12345*6789="},
        {"action": "read", "text": "Calculator"},
    ]})
    ref = wref(text)
    shown = "83,810,205" in text or "83810205" in text
    if ref:
        c({"action": "window", "op": "close", "window": ref})
    return {"ok": ok and shown, "call_ok": ok, "answer_seen": shown, "detail": text[-400:]}


def explorer(c: Client) -> dict:
    """Open a fresh folder, make a subfolder with ctrl+shift+n, rename a file with F2."""
    base = ROOT / f"explorer-{int(time.time())}"
    base.mkdir(parents=True)
    (base / "before.txt").write_text("hekaton")
    ok1, text, _ = c({"action": "folder", "path": str(base), "list": 10})
    ref = wref(text)
    steps = [
        {"action": "key", "text": "ctrl+shift+n"},
        {"action": "wait", "duration": 0.4},
        {"action": "type", "text": "made-by-hekaton\n"},
        {"action": "wait", "duration": 0.3},
        {"action": "click_text", "text": "before.txt"},
        {"action": "key", "text": "f2"},
        {"action": "wait", "duration": 0.3},
        {"action": "key", "text": "ctrl+a"},
        {"action": "type", "text": "after.txt\n"},
        # wait_for_file is refused here by design: under an allowlist it only watches Temp and
        # files this call saved. The disk check below is the proof.
        {"action": "wait", "duration": 0.4},
    ]
    ok2, text2, _ = c({"actions": steps, "only_apps": ["explorer.exe"]})
    made = (base / "made-by-hekaton").is_dir()
    renamed = (base / "after.txt").is_file() and not (base / "before.txt").exists()
    if ref:
        c({"action": "window", "op": "close", "window": ref})
    return {"ok": made and renamed, "call_ok": ok1 and ok2, "folder_made": made, "renamed": renamed,
            "detail": (text + "\n" + text2)[-500:]}


def window_ops(c: Client) -> dict:
    """Move, minimize, restore, maximize and close a Notepad window this test opened."""
    ok, text, _ = c({"action": "open_app", "text": "notepad", "reuse": False})
    ref = wref(text)
    if not ref:
        return {"ok": False, "detail": text}
    want = (300, 200, 900, 600)
    checks = {}
    c({"action": "window", "op": "move", "window": ref, "rect": list(want)})
    got = rect_of(window_line(c, ref))
    checks["moved"] = bool(got) and all(abs(a - b) <= 16 for a, b in zip(got, want))
    c({"action": "window", "op": "minimize", "window": ref})
    checks["minimized"] = "minimized" in (window_line(c, ref) or "")
    c({"action": "window", "op": "restore", "window": ref})
    line = window_line(c, ref) or ""
    checks["restored"] = bool(line) and "minimized" not in line
    c({"action": "window", "op": "maximize", "window": ref})
    big = rect_of(window_line(c, ref))
    checks["maximized"] = bool(big) and big[2] > want[2] + 400 and big[3] > want[3] + 300
    c({"action": "window", "op": "close", "window": ref})
    time.sleep(0.5)
    checks["closed"] = window_line(c, ref) is None
    return {"ok": all(checks.values()), **checks, "detail": line}


CASES = {"calc": calc, "explorer": explorer, "window": window_ops}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", required=True, choices=["old", "new"])
    ap.add_argument("--only", default=",".join(CASES))
    a = ap.parse_args()
    ROOT.mkdir(parents=True, exist_ok=True)
    c = Client(a.target)
    failed = 0
    for name in a.only.split(","):
        t = time.perf_counter()
        try:
            r = CASES[name](c)
        except Exception as e:  # report and keep going
            r = {"ok": False, "detail": f"{type(e).__name__}: {e}"}
        r["wall_s"] = round(time.perf_counter() - t, 2)
        failed += not r["ok"]
        print(f"{name}: {json.dumps(r, ensure_ascii=False)}")
    out = Path("bench/runs") / f"l1-more-{a.target}"
    out.mkdir(parents=True, exist_ok=True)
    (out / "calls.jsonl").write_text("\n".join(json.dumps(x, ensure_ascii=False) for x in c.log), encoding="utf-8")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
