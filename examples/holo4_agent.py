#!/usr/bin/env python3
"""Holo4 drives machands: H Company's computer-use model is the brain, machands is the hands.

Each step: machands takes a screenshot, Holo4 picks one tool call, machands runs it. Coordinates
come back from Holo4 on a 0..1000 grid and are scaled to the exact screenshot that was sent.
Risky steps still hit the machands safety gate (Allow / Deny), so the model cannot send, buy or
delete on its own.

Usage:
    python3 examples/holo4_agent.py "Open Calculator and work out 1234 * 56"
Key: HAI_API_KEY, else macOS Keychain item `hai-api-key`. No pip installs needed.
"""

import argparse
import base64
import json
import os
import re
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

API = "https://api.hcompany.ai/v1/chat/completions"
WINHANDS = os.environ.get(
    "WINHANDS_BIN", str(Path.home() / "Applications/winhands.app/Contents/MacOS/winhands")
)
# python.org builds ship without root certs; fall back to the macOS bundle
SSL = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if Path("/etc/ssl/cert.pem").exists() else None)
KEEP_SHOTS = 3  # H's guidance: keep the last 3 screenshots, evict older ones

SYSTEM = """You operate a macOS computer to finish the user's task. Every turn you get a fresh
screenshot. Call exactly one tool per turn. Keyboard shortcuts use cmd, not ctrl (cmd+space, cmd+a).
Prefer open_app to hunting for icons. After typing into a search or address field, press Return.
When the task is done, or cannot be done, call answer with the result in plain words."""


def tool(name, desc, props, required):
    return {
        "type": "function",
        "function": {
            "name": name,
            "description": desc,
            "parameters": {"type": "object", "properties": props, "required": required},
        },
    }


XY = {
    "element": {"type": "string", "description": "Detailed description of the target UI element"},
    "x": {"type": "integer", "description": "0..1000 across the screenshot"},
    "y": {"type": "integer", "description": "0..1000 down the screenshot"},
}
TOOLS = [
    tool("click", "Click at (x, y) coordinates.", XY, ["element", "x", "y"]),
    tool("double_click", "Double-click at (x, y) coordinates.", XY, ["element", "x", "y"]),
    tool("type", "Type text into the focused field.",
         {"text": {"type": "string"}}, ["text"]),
    tool("key", "Press a key or combo, e.g. Return, Escape, cmd+a, cmd+space.",
         {"keys": {"type": "string"}}, ["keys"]),
    tool("scroll", "Scroll at (x, y).",
         {**XY, "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
          "amount": {"type": "integer", "description": "Wheel ticks, default 3"}},
         ["x", "y", "direction"]),
    tool("open_app", "Open or focus a macOS app by name, e.g. Calculator, Notes, Safari.",
         {"name": {"type": "string"}}, ["name"]),
    tool("wait", "Wait for the screen to settle.",
         {"seconds": {"type": "number"}}, ["seconds"]),
    tool("answer", "Provide a final answer.",
         {"content": {"type": "string"}}, ["content"]),
]


def api_key():
    k = os.environ.get("HAI_API_KEY")
    if k:
        return k
    return subprocess.run(
        ["security", "find-generic-password", "-s", "hai-api-key", "-w"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()


def wh(payload):
    """One machands `computer` call. Returns (ok, stdout+stderr text)."""
    p = subprocess.run(
        [WINHANDS, "wh", "computer", "--json", "-"],
        input=json.dumps(payload), capture_output=True, text=True,
    )
    out = (p.stdout + p.stderr).strip()
    # a failed step inside a batch can still exit 0, so read the text too
    return p.returncode == 0 and "failed:" not in out and "error:" not in out, out


def screenshot():
    ok, out = wh({"action": "screenshot"})
    m = re.search(r"(\d+)x(\d+) image", out)
    path = out.strip().splitlines()[-1].strip()
    if not ok or not m or not Path(path).exists():
        sys.exit(f"screenshot failed: {out}")
    data = Path(path).read_bytes()
    return int(m.group(1)), int(m.group(2)), data


def clean_screen():
    """Hide every other app and close Finder windows, so the model only ever sees the task.
    Screenshots go to H's API: anything on screen leaves the machine."""
    subprocess.run(["osascript", "-e", 'tell application "Finder" to activate'], capture_output=True)
    time.sleep(0.5)
    wh({"action": "key", "text": "cmd+alt+h", "shot": False})
    subprocess.run(["osascript", "-e", 'tell application "Finder" to close every window'],
                   capture_output=True)
    time.sleep(0.5)
    ok, out = wh({"action": "list_windows", "shot": False})
    left = [ln for ln in out.splitlines() if ln.startswith("w")]
    if left:
        sys.exit("clean screen failed, still visible:\n" + "\n".join(left))


def chat(key, model, messages):
    body = {
        "model": model,
        "messages": messages,
        "tools": TOOLS,
        "tool_choice": "required",
        "temperature": 0.8,
        "chat_template_kwargs": {"enable_thinking": True},
    }
    if model == "holo4-27b":
        body["reasoning_effort"] = "medium"
    req = urllib.request.Request(
        API, data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=180, context=SSL) as r:
            return json.load(r)["choices"][0]["message"]
    except urllib.error.HTTPError as e:
        sys.exit(f"H API {e.code}: {e.read().decode(errors='replace')[:500]}")


def evict_old_shots(messages):
    seen = 0
    for m in reversed(messages):
        if m["role"] != "user" or not isinstance(m["content"], list):
            continue
        for part in m["content"]:
            if part.get("type") == "image_url":
                seen += 1
                if seen > KEEP_SHOTS:
                    part.clear()
                    part.update({"type": "text", "text": "[screenshot evicted]"})


def run_tool(name, a, w, h):
    px = lambda: [round(a["x"] * w / 1000), round(a["y"] * h / 1000)]
    if name == "click":
        return wh({"action": "left_click", "coordinate": px(), "shot": False})
    if name == "double_click":
        return wh({"action": "double_click", "coordinate": px(), "shot": False})
    if name == "type":
        return wh({"action": "type", "text": a["text"], "shot": False})
    if name == "key":
        return wh({"action": "key", "text": a["keys"], "shot": False})
    if name == "scroll":
        return wh({"action": "scroll", "coordinate": px(), "scroll_direction": a["direction"],
                   "scroll_amount": a.get("amount", 3), "shot": False})
    if name == "open_app":
        return wh({"action": "open_app", "text": a["name"], "shot": False})
    if name == "wait":
        time.sleep(min(float(a.get("seconds", 1)), 10))
        return True, "waited"
    return False, f"unknown tool {name}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("task")
    ap.add_argument("--model", default="holo4-35b-a3b")
    ap.add_argument("--max-steps", type=int, default=25)
    ap.add_argument("--log", default="holo4-run")
    ap.add_argument("--no-clean", action="store_true",
                    help="Skip hiding other apps first (screenshots then show whatever is open)")
    ap.add_argument("--rpm", type=float, default=0,
                    help="Requests per minute cap; defaults to 10 on the free holo3-1 model")
    args = ap.parse_args()

    key = api_key()
    if not args.no_clean:
        clean_screen()
    logdir = Path(args.log)
    logdir.mkdir(parents=True, exist_ok=True)
    messages = [{"role": "system", "content": SYSTEM},
                {"role": "user", "content": f"Task: {args.task}"}]
    t0 = time.time()
    rpm = args.rpm or (10 if args.model.startswith("holo3-1") else 0)
    gap = 60 / rpm + 0.2 if rpm else 0
    last_call = 0.0

    for step in range(1, args.max_steps + 1):
        w, h, img = screenshot()
        (logdir / f"step{step:02d}.jpg").write_bytes(img)
        b64 = base64.b64encode(img).decode()
        messages.append({"role": "user", "content": [
            {"type": "text", "text": "<observation>"},
            {"type": "image_url", "image_url": {"url": f"data:image/jpeg;base64,{b64}"}},
            {"type": "text", "text": "</observation>"},
        ]})
        evict_old_shots(messages)

        if gap and time.time() - last_call < gap:
            time.sleep(gap - (time.time() - last_call))
        t = last_call = time.time()
        msg = chat(key, args.model, messages)
        think_s = time.time() - t
        calls = msg.get("tool_calls") or []
        messages.append({"role": "assistant", "content": msg.get("content") or "",
                         "tool_calls": calls})
        if not calls:
            print(f"[{step}] no tool call: {msg.get('content')!r}")
            continue

        for c in calls:
            name = c["function"]["name"]
            a = json.loads(c["function"]["arguments"] or "{}")
            if name == "answer":
                total = time.time() - t0
                print(f"[{step}] ANSWER ({total:.0f} s, {step} steps): {a.get('content')}")
                (logdir / "answer.txt").write_text(a.get("content", ""))
                return
            t = time.time()
            ok, out = run_tool(name, a, w, h)
            act_s = time.time() - t
            short = out.splitlines()[0] if out else ""
            print(f"[{step}] {name} {json.dumps(a)} -> {'ok' if ok else 'FAIL'} "
                  f"(model {think_s:.1f} s, hands {act_s:.2f} s) {short if not ok else ''}")
            messages.append({"role": "tool", "tool_call_id": c["id"],
                             "content": "ok" if ok else f"failed: {out[:400]}"})
        time.sleep(0.3)

    print("stopped: max steps reached")


if __name__ == "__main__":
    main()
