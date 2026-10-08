#!/usr/bin/env python3
"""Compare two winhands daemons (for example the Python build and the Rust build) on the same screen.

It starts nothing. Give it two base URLs and their tokens; it sends the same PASSIVE requests to both
(no clicks, no keys, no typing: health, screenshot, list_windows, cursor_position, list_files, read,
find_text) and diffs the answers:

* /health: the field names, and the type of each field;
* screenshot: the text shape, the number of images, each image's MIME type and pixel size;
* every other action: the text shape (digits and quoted strings are folded away, so a window that
  moved or a changed title does not count) and, as a note, whether the exact text was equal too.

Run it with both daemons up and the desktop left alone, so both see the same screen:

    python tests/parity/compare.py \\
        --a http://127.0.0.1:47811 --a-token-file "%LOCALAPPDATA%\\winhands\\token" --a-label python \\
        --b http://127.0.0.1:47812 --b-token-file D:\\rs-test\\winhands\\token       --b-label rust

The Rust daemon on a second port keeps its data in another folder: start it with
LOCALAPPDATA=<some folder> WINHANDS_PORT=47812 winhands daemon. Never point both at one data folder,
the second would rewrite the first one's token.

Exit code 0 when no shape differs, 1 when something does, 2 when a daemon cannot be reached.
Standard library only.
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import re
import struct
import sys
import urllib.error
import urllib.request

# Fields one build has and the other may not: the Rust daemon says which implementation and backend it
# is. Everything else in /health has to match.
HEALTH_EXTRAS = {"impl", "backend"}

# (label, computer payload). All passive. "shot": false keeps the text-only actions from adding a
# trailing screenshot; the screenshot request itself returns its picture.
def passive_requests(folder: str, find: str, read_window: str) -> list[tuple[str, dict]]:
    return [
        ("screenshot", {"action": "screenshot"}),
        ("list_windows", {"action": "list_windows", "shot": False}),
        ("cursor_position", {"action": "cursor_position", "shot": False}),
        ("list_files", {"action": "list_files", "text": folder, "shot": False}),
        ("read", {"action": "read", "text": read_window, "limit": 60, "shot": False}),
        ("find_text", {"action": "find_text", "text": find, "shot": False}),
    ]


# ---------------------------------------------------------------------------------------- HTTP

class Daemon:
    def __init__(self, label: str, base: str, token: str, timeout: float = 120.0):
        self.label = label
        self.base = base.rstrip("/")
        self.token = token
        self.timeout = timeout

    def request(self, method: str, path: str, payload: dict | None = None) -> tuple[int, dict | None, bytes]:
        data = None if payload is None else json.dumps(payload).encode("utf-8")
        req = urllib.request.Request(self.base + path, data=data, method=method)
        req.add_header("X-Winhands-Token", self.token)
        req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                status, body = resp.status, resp.read()
        except urllib.error.HTTPError as err:
            status, body = err.code, err.read()
        try:
            parsed = json.loads(body.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            parsed = None
        return status, parsed if isinstance(parsed, dict) else None, body


# ---------------------------------------------------------------------------------------- shapes

def shape(text: str) -> str:
    """The text with what changes from one moment to the next folded away: quoted strings and numbers."""
    text = re.sub(r'"[^"\n]*"', '"S"', text)
    text = re.sub(r"(?<!\w)'[^'\n]*'(?!\w)", "'S'", text)
    text = re.sub(r"\d+(?:\.\d+)?", "N", text)
    return re.sub(r"[ \t]+", " ", text).strip()


def shape_lines(text: str) -> list[str]:
    return [shape(line) for line in text.splitlines() if line.strip()]


def image_size(data: bytes) -> tuple[int, int] | None:
    """(width, height) of a PNG or JPEG, None when it is neither."""
    if data[:8] == b"\x89PNG\r\n\x1a\n" and len(data) >= 24:
        return struct.unpack(">II", data[16:24])
    if data[:2] == b"\xff\xd8":
        i = 2
        while i + 9 < len(data):
            if data[i] != 0xFF:
                i += 1
                continue
            marker = data[i + 1]
            if marker in (0xD8, 0x01) or 0xD0 <= marker <= 0xD7:
                i += 2
                continue
            length = struct.unpack(">H", data[i + 2:i + 4])[0]
            if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
                height, width = struct.unpack(">HH", data[i + 5:i + 9])
                return width, height
            i += 2 + length
    return None


def summarise(reply: dict | None, status: int) -> dict:
    """What a computer answer looks like, without its changing details."""
    if reply is None:
        return {"status": status, "ok": None, "text": "", "images": [], "error": None}
    text, images = "", []
    for block in reply.get("content") or []:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text":
            text += ("\n" if text else "") + str(block.get("text", ""))
        elif block.get("type") == "image":
            try:
                size = image_size(base64.b64decode(block.get("data", "")))
            except (ValueError, TypeError):
                size = None
            images.append({"mime": block.get("mimeType"), "size": size})
    if not text:
        text = str(reply.get("result", "") or "")
    return {"status": status, "ok": reply.get("ok"), "text": text, "images": images,
            "error": reply.get("error"), "failed_step": reply.get("failed_step")}


# ---------------------------------------------------------------------------------------- compare

def compare_health(a: dict | None, b: dict | None) -> list[str]:
    if a is None or b is None:
        return ["/health did not answer 200 with an object on " + ("a" if a is None else "b")]
    diffs = []
    ka, kb = set(a) - HEALTH_EXTRAS, set(b) - HEALTH_EXTRAS
    for key in sorted(ka - kb):
        diffs.append(f"/health field {key!r} only in a")
    for key in sorted(kb - ka):
        diffs.append(f"/health field {key!r} only in b")
    for key in sorted(ka & kb):
        ta, tb = type(a[key]).__name__, type(b[key]).__name__
        if ta != tb and not {ta, tb} <= {"int", "float"} and "NoneType" not in (ta, tb):
            diffs.append(f"/health field {key!r} is {ta} in a and {tb} in b")
    return diffs


def compare_answers(name: str, a: dict, b: dict) -> tuple[list[str], list[str]]:
    """(differences, notes) for one request's two summaries."""
    diffs, notes = [], []
    for field in ("status", "ok"):
        if a[field] != b[field]:
            diffs.append(f"{name}: {field} is {a[field]!r} in a and {b[field]!r} in b")
    if (a.get("error") is None) != (b.get("error") is None):
        diffs.append(f"{name}: error is {a.get('error')!r} in a and {b.get('error')!r} in b")
    if len(a["images"]) != len(b["images"]):
        diffs.append(f"{name}: {len(a['images'])} image(s) in a, {len(b['images'])} in b")
    for i, (ia, ib) in enumerate(zip(a["images"], b["images"])):
        if ia["size"] != ib["size"]:
            diffs.append(f"{name}: image {i + 1} is {ia['size']} in a and {ib['size']} in b")
        if ia["mime"] != ib["mime"]:
            diffs.append(f"{name}: image {i + 1} is {ia['mime']} in a and {ib['mime']} in b")
    sa, sb = shape_lines(a["text"]), shape_lines(b["text"])
    if sa != sb:
        only_a = [line for line in sa if line not in sb][:3]
        only_b = [line for line in sb if line not in sa][:3]
        diffs.append(f"{name}: text shape differs ({len(sa)} vs {len(sb)} lines); "
                     f"only in a: {only_a}; only in b: {only_b}")
    elif a["text"] != b["text"]:
        notes.append(f"{name}: same shape, different details (the screen moved between the two calls?)")
    return diffs, notes


def run(a: Daemon, b: Daemon, folder: str, find: str, read_window: str) -> dict:
    report = {"diffs": [], "notes": [], "checked": []}
    health = []
    for d in (a, b):
        status, body, _ = d.request("GET", "/health")
        health.append(body if status == 200 else None)
    report["diffs"] += compare_health(*health)
    report["checked"].append("health")
    for name, payload in passive_requests(folder, find, read_window):
        answers = []
        for d in (a, b):
            status, body, _ = d.request("POST", "/computer", payload)
            answers.append(summarise(body, status))
        diffs, notes = compare_answers(name, *answers)
        report["diffs"] += diffs
        report["notes"] += notes
        report["checked"].append(name)
    return report


# ---------------------------------------------------------------------------------------- command line

def read_token(literal: str | None, path: str | None) -> str:
    if literal:
        return literal.strip()
    if path:
        with open(os.path.expandvars(path), encoding="utf-8") as f:
            return f.read().strip()
    return ""


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    for side in ("a", "b"):
        p.add_argument(f"--{side}", required=True, help=f"base URL of daemon {side}, e.g. http://127.0.0.1:47811")
        p.add_argument(f"--{side}-token", help="the token itself")
        p.add_argument(f"--{side}-token-file", help="a file holding the token")
        p.add_argument(f"--{side}-label", default=side, help="a name for messages")
    p.add_argument("--folder", default=os.path.expanduser("~"), help="folder for list_files (default: home)")
    p.add_argument("--find", default="File", help="text for find_text")
    p.add_argument("--read-window", default="", help="window title for read (default: the front window)")
    p.add_argument("--json", action="store_true", help="print the report as JSON")
    args = p.parse_args(argv)
    a = Daemon(args.a_label, args.a, read_token(args.a_token, args.a_token_file))
    b = Daemon(args.b_label, args.b, read_token(args.b_token, args.b_token_file))
    try:
        report = run(a, b, args.folder, args.find, args.read_window)
    except (urllib.error.URLError, OSError) as exc:
        print(f"compare: cannot reach a daemon: {exc}", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"a = {a.label} {a.base}\nb = {b.label} {b.base}")
        print("checked: " + ", ".join(report["checked"]))
        for note in report["notes"]:
            print("note: " + note)
        for diff in report["diffs"]:
            print("DIFF: " + diff)
        print("parity: " + ("OK" if not report["diffs"] else f"{len(report['diffs'])} difference(s)"))
    return 1 if report["diffs"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
