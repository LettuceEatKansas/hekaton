"""Gauntlet L1: Notepad Unicode text through Save As, compared byte for byte.

    python bench/l1_notepad.py --target new --runs 3
One batched call per run, input limited to notepad.exe, no screenshots returned.
"""
import argparse, json, os, subprocess, sys, time
from pathlib import Path

TEXT = ("Hekaton L1 \u00e9t\u00e9 na\u00efve caf\u00e9\tTAB\there\n"
        "emoji \U0001F600 \U0001F680 \u2764\ufe0f thumbs \U0001F44D\U0001F3FD\n"
        "hebrew \u05e9\u05dc\u05d5\u05dd \u05e2\u05d5\u05dc\u05dd and arabic \u0645\u0631\u062d\u0628\u0627\n"
        "cjk \u4f60\u597d\u4e16\u754c \u65e5\u672c\u8a9e \ud55c\uad6d\uc5b4\n"
        "math \u2211\u222b\u221a\u03c0 \u2264 \u2265 \u2260 quotes \u201csmart\u201d \u2018x\u2019\n")
TEXT = TEXT * 3

def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--target", required=True); ap.add_argument("--runs", type=int, default=3)
    a = ap.parse_args()
    out = Path(r"C:\Users\you\tools\hekaton-test-state\gauntlet")
    results = []
    for i in range(a.runs):
        path = out / f"l1-notepad-{a.target}-{i}.txt"
        if path.exists(): path.unlink()
        args = {"actions": [
            {"action": "open_app", "text": "notepad", "reuse": False},
            {"action": "type", "text": TEXT},
            {"action": "key", "text": "ctrl+shift+s"},
            {"action": "file_dialog", "text": str(path)},
            {"action": "wait_for_file", "text": str(path), "timeout": 5},
            {"action": "key", "text": "ctrl+w"},
        ], "only_apps": ["notepad.exe"], "shot": False, "profile": True}
        suite = out / "l1-suite.json"
        suite.write_text(json.dumps([{"name": f"l1-notepad", "args": args}]))
        r = subprocess.run([sys.executable, "bench/drive.py", "--target", a.target, "--suite", str(suite),
                            "--out", f"bench/runs/l1-{a.target}", "--timeout", "120", "--print-text"],
                           capture_output=True, text=True, encoding="utf-8")
        got = path.read_bytes() if path.exists() else b""
        want = TEXT.encode("utf-8")
        ok = got in (want, b"\xef\xbb\xbf" + want, want.replace(b"\n", b"\r\n"), b"\xef\xbb\xbf" + want.replace(b"\n", b"\r\n"))
        print(f"run {i}: file_match={ok} got={len(got)}B want={len(want)}B")
        if not ok and got:
            g = got.decode("utf-8", "replace"); 
            for j, (x, y) in enumerate(zip(g, TEXT)):
                if x != y: print(f"  first diff at char {j}: got {x!r} want {y!r}"); break
        print(r.stdout[-1500:])
        time.sleep(1)

main()
