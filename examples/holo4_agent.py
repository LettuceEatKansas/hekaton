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
screenshot. You may call several tools in one turn when the screen will not change in a way you
need to see between them (for example: click a field, type, press Return).
Keyboard shortcuts use cmd, not ctrl. Useful ones: cmd+n new document or window, cmd+shift+n
private browser window, cmd+l browser address bar, cmd+s save, Tab next spreadsheet cell, Return
next row. Never click the Dock: open apps with open_app.
Do one search at a time: after each search, read the answer on the page and save it with note
before the next search. Prefer web_search and shortcuts to hunting for icons or menus. To fill a spreadsheet, click the first cell once, then type each value with Tab between columns.
A new TextEdit document is an empty page (black in dark mode): click in it and type.
In a save dialog, type the file name, press cmd+d to pick the Desktop, then Return.
Before you answer, look at the screen and check the result is really there.
If a CAPTCHA or login wall appears, stop and answer that you were blocked.
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
    tool("type", "Type text into the focused field or cell. Use \\n for new lines: write a whole "
         "multi-line document in one call.",
         {"text": {"type": "string"},
          "clear_first": {"type": "boolean", "description": "Select all and replace what is there"},
          "submit": {"type": "boolean", "description": "Press Return after typing"}},
         ["text"]),
    tool("web_search", "Search the web in the front browser window (focuses the address bar, "
         "types the query, presses Return).",
         {"query": {"type": "string"}}, ["query"]),
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
    tool("note", "Save a fact you found (an address, a number) so you still have it after the "
         "screenshot that showed it is gone. Your notes come back with every new screenshot.",
         {"text": {"type": "string"}}, ["text"]),
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


def visible_outsiders(allowed):
    """Windows on screen from apps that are not part of the task."""
    _, out = wh({"action": "list_windows", "shot": False})
    bad = []
    for ln in out.splitlines():
        m = re.match(r'w\d+ ".*" (\S+) ', ln)
        if m and m.group(1) not in allowed:
            bad.append(ln)
    return bad


class Events:
    """Wall-clock event log the demo overlay is rendered from (one JSON object per line)."""

    def __init__(self, path):
        self.f = open(path, "w")

    def __call__(self, kind, **kw):
        self.f.write(json.dumps({"t": time.time(), "kind": kind, **kw}) + "\n")
        self.f.flush()


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
    # the Dock holds every app, including private ones; a misclick there opened Outlook
    if name in ("click", "double_click") and a["y"] >= 930:
        return False, "blocked: clicking the Dock is not allowed. Use open_app to open an app."
    if name == "click":
        return wh({"action": "left_click", "coordinate": px(), "shot": False})
    if name == "double_click":
        return wh({"action": "double_click", "coordinate": px(), "shot": False})
    if name == "type":
        steps = []
        if a.get("clear_first"):
            steps.append({"action": "key", "text": "cmd+a"})
        steps.append({"action": "type", "text": a["text"]})
        if a.get("submit"):
            steps.append({"action": "key", "text": "Return"})
        return wh({"actions": steps, "shot": False})
    if name == "web_search":
        return wh({"actions": [{"action": "key", "text": "cmd+l"},
                               {"action": "type", "text": a["query"]},
                               {"action": "key", "text": "Return"}], "shot": False})
    if name == "key":
        return wh({"action": "key", "text": a["keys"], "shot": False})
    if name == "scroll":
        return wh({"action": "scroll", "coordinate": px(), "scroll_direction": a["direction"],
                   "scroll_amount": a.get("amount", 3), "shot": False})
    if name == "open_app":
        ok, out = wh({"action": "open_app", "text": a["name"], "shot": False})
        ref = re.search(r"\bw(\d+)\b", out)
        if ok and ref:
            # small windows confused the model (an empty dark TextEdit page looked like nothing).
            # Not op=maximize: on macOS that is native full screen, a separate Space, and the next
            # app opened onto an empty desktop. Fill the screen below the menu bar instead.
            wh({"action": "window", "op": "move", "window": f"w{ref.group(1)}",
                "rect": [0, 30, 1920, 996], "shot": False})
        return ok, out
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
    ap.add_argument("--apps", default="",
                    help="Comma list of bundle ids the task may show; any other visible app stops the run")
    ap.add_argument("--no-clean", action="store_true",
                    help="Skip hiding other apps first (screenshots then show whatever is open)")
    ap.add_argument("--rpm", type=float, default=0,
                    help="Requests per minute cap; defaults to 10 on the free holo3-1 model")
    args = ap.parse_args()

    key = api_key()
    if not args.no_clean:
        clean_screen()
    allowed = {a.strip() for a in args.apps.split(",") if a.strip()}
    allowed |= {"com.apple.finder", "com.apple.Spotlight", "com.apple.dock"}
    logdir = Path(args.log)
    logdir.mkdir(parents=True, exist_ok=True)
    ev = Events(logdir / "events.jsonl")
    messages = [{"role": "system", "content": SYSTEM},
                {"role": "user", "content": f"Task: {args.task}"}]
    rpm = args.rpm or (10 if args.model.startswith("holo3-1") else 0)
    gap = 60 / rpm + 0.2 if rpm else 0
    last_call = 0.0
    notes = []
    ev("prompt", task=args.task, model=args.model)
    t0 = time.time()

    for step in range(1, args.max_steps + 1):
        if gap and time.time() - last_call < gap:
            w0 = time.time()
            time.sleep(gap - (time.time() - last_call))
            ev("ratewait", start=w0)
        if args.apps:
            bad = visible_outsiders(allowed)
            if bad:
                ev("abort", reason="outside app visible")
                sys.exit("stopped: a window outside the task is visible, screenshot not sent:\n"
                         + "\n".join(bad))
        w, h, img = screenshot()
        ev("shot", step=step)
        (logdir / f"step{step:02d}.jpg").write_bytes(img)
        b64 = base64.b64encode(img).decode()
        messages.append({"role": "user", "content": [
            {"type": "text", "text": "<observation>"},
            {"type": "image_url", "image_url": {"url": f"data:image/jpeg;base64,{b64}"}},
            {"type": "text", "text": "</observation>"},
        ] + ([{"type": "text", "text": "Your notes so far:\n" + "\n".join(notes)}] if notes else [])})
        evict_old_shots(messages)

        t = last_call = time.time()
        msg = chat(key, args.model, messages)
        think_s = time.time() - t
        calls = msg.get("tool_calls") or []
        thought = (msg.get("reasoning") or msg.get("reasoning_content") or "").strip()
        ev("model", step=step, start=t, secs=think_s, thought=thought[-300:],
           calls=[{"name": c["function"]["name"], "args": c["function"]["arguments"]} for c in calls])
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
                ev("answer", content=a.get("content", ""), total=total, steps=step)
                print(f"[{step}] ANSWER ({total:.1f} s, {step} steps): {a.get('content')}")
                (logdir / "answer.txt").write_text(a.get("content", ""))
                return
            t = time.time()
            if name == "note":
                notes.append(a.get("text", ""))
                ok, out = True, "saved"
            else:
                ok, out = run_tool(name, a, w, h)
            act_s = time.time() - t
            ev("action", step=step, start=t, secs=act_s, name=name, args=a, ok=ok)
            short = out.splitlines()[0] if out else ""
            print(f"[{step}] {name} {json.dumps(a)} -> {'ok' if ok else 'FAIL'} "
                  f"(model {think_s:.1f} s, hands {act_s:.2f} s) {short if not ok else ''}")
            messages.append({"role": "tool", "tool_call_id": c["id"],
                             "content": (out.splitlines()[0] if name == "open_app" else "ok")
                             if ok else f"failed: {out[:400]}"})
        time.sleep(0.3)

    print("stopped: max steps reached")


if __name__ == "__main__":
    main()
