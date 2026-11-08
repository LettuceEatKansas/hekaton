#!/usr/bin/env python3
"""Render a demo video: screen recording + live panel built from holo4_agent.py's events.jsonl.

Layout (1920x1080): the recording scaled to 1536x864 top left, a step log on the right, the prompt
and a timer along the bottom. The timer starts when the prompt is sent to the model and stops on
the model's final answer, so it shows real wall-clock time.

Usage:
    python3 examples/demo_overlay.py RAW.mp4 RUN_DIR/events.jsonl --video-start EPOCH -o out.mp4
`--video-start` is the wall-clock time of the recording's first frame (stop time minus duration).
"""

import argparse
import json
import re
import subprocess
import textwrap
from pathlib import Path

W, H = 1920, 1080
VW, VH = 1440, 810           # recording area
PX = VW + 24                 # panel text x
PW, PL = 190, 10           # prompt wrap width (chars) and max lines
TAIL = 4.0                   # seconds to hold the final frame state after the answer


def ts(sec):
    sec = max(sec, 0)
    h = int(sec // 3600)
    m = int(sec % 3600 // 60)
    s = sec % 60
    return f"{h}:{m:02d}:{s:05.2f}"


def esc(text):
    return text.replace("\\", "\\\\").replace("{", "(").replace("}", ")").replace("\n", " ")


def short_args(name, args):
    if name in ("click", "double_click"):
        return f'"{args.get("element", "")}"'
    if name == "scroll":
        return f'{args.get("direction", "")} x{args.get("amount", 3)}'
    if name == "type":
        extra = " + Return" if args.get("submit") else ""
        return f'"{args.get("text", "")}"{extra}'
    if name == "web_search":
        return f'"{args.get("query", "")}"'
    if name == "key":
        return args.get("keys", "")
    if name == "open_app":
        return args.get("name", "")
    if name == "note":
        return f'"{args.get("text", "")}"'
    if name == "wait":
        return f'{args.get("seconds", "")} s'
    return json.dumps(args)[:60]


HEADER = f"""[Script Info]
ScriptType: v4.00+
PlayResX: {W}
PlayResY: {H}
WrapStyle: 0

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Log,Menlo,18,&H00E6E6E6,&H00FFFFFF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,7,0,0,0,1
Style: Head,Helvetica Neue,24,&H00FFFFFF,&H00FFFFFF,&H00000000,&H00000000,1,0,0,0,100,100,0,0,1,0,0,7,0,0,0,1
Style: Prompt,Helvetica Neue,17,&H00F0F0F0,&H00FFFFFF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,7,0,0,0,1
Style: Timer,Menlo,72,&H00FFFFFF,&H00FFFFFF,&H00000000,&H00000000,1,0,0,0,100,100,0,0,1,0,0,9,0,0,0,1
Style: Small,Helvetica Neue,18,&H00A0A0A0,&H00FFFFFF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,7,0,0,0,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"""


def line(start, end, style, x, y, text, layer=0):
    return f"Dialogue: {layer},{ts(start)},{ts(end)},{style},,0,0,0,,{{\\pos({x},{y})}}{text}\n"


def build(events, vstart, duration, hands="hekaton"):
    rel = lambda t: t - vstart
    prompt = next(e for e in events if e["kind"] == "prompt")
    answer = next((e for e in events if e["kind"] == "answer"), None)
    t0 = rel(prompt["t"])
    t_end = rel(answer["t"]) if answer else duration
    end_all = min(duration, t_end + TAIL)
    model_name = prompt.get("model", "holo4")
    out = [HEADER]

    # bottom strip: prompt (typed out in the 2 s before it is sent), then held
    words = prompt["task"]
    wrapped = textwrap.wrap(words, PW)[:PL]
    full = "\\N".join(esc(x) for x in wrapped)
    out.append(line(0, end_all, "Small", 24, VH + 14, "PROMPT  →  " + esc(model_name)))
    type_from = max(0.0, t0 - 2.0)
    n = len(words)
    steps = 24
    for i in range(steps):
        a = type_from + (t0 - type_from) * i / steps
        b = type_from + (t0 - type_from) * (i + 1) / steps
        k = int(n * (i + 1) / steps)
        part = "\\N".join(esc(x) for x in textwrap.wrap(words[:k], PW)[:PL])
        out.append(line(a, b, "Prompt", 24, VH + 44, part))
    out.append(line(t0, end_all, "Prompt", 24, VH + 44, full))

    # timer: waits at 0.0 until the prompt is sent, runs, then freezes on the answer
    out.append(line(0, t0, "Timer", W - 30, VH + 30, "0.0 s"))
    t = t0
    while t < t_end:
        nxt = min(t + 0.1, t_end)
        out.append(line(t, nxt, "Timer", W - 30, VH + 30, f"{t - t0:.1f} s"))
        t = nxt
    if answer:
        out.append(line(t_end, end_all, "Timer", W - 30, VH + 30,
                        f"{{\\c&H0050E070&}}{answer['total']:.1f} s"))
    out.append(line(0, end_all, "Small", W - 300, VH + 120, "since the prompt was sent"))

    # right panel: step log, rebuilt at every event
    out.append(line(0, end_all, "Head", PX, 18, f"{esc(model_name)}  →  {esc(hands)}"))
    out.append(line(0, end_all, "Small", PX, 52, f"model decides · {esc(hands)} acts"))
    log = []          # finished lines
    states = []       # (time, text)

    def panel(extra=""):
        shown = log[-30:]
        body = "\\N".join(shown)
        return body + ("\\N" + extra if extra else "")

    states.append((0, "waiting for prompt"))
    for e in events:
        k = e["kind"]
        if k == "model":
            s = e["step"]
            states.append((rel(e["start"]), panel(f"{{\\c&H0040C0FF&}}#{s} thinking…")))
            names = ", ".join(c["name"] for c in e["calls"]) or "no action"
            log.append(f"{{\\c&H0040C0FF&}}#{s}{{\\c&H00E6E6E6&}} model {e['secs']:.1f} s → {esc(names)}")
            sents = re.split(r"(?<=[.!?])\s+", " ".join(e.get("thought", "").split()))
            thought = ""
            for sent in reversed([x for x in sents if x]):
                if thought and len(sent) + len(thought) > 80:
                    break
                thought = (sent + " " + thought).strip()
            thought = thought[-80:]
            for tl in textwrap.wrap(thought, 40)[-2:]:
                log.append(f"   {{\\c&H00909090&}}{esc(tl)}{{\\c&H00E6E6E6&}}")
            states.append((rel(e["start"] + e["secs"]), panel()))
        elif k == "action" and e["name"] == "note":
            log.append("   {\\c&H0050E070&}✎ note{\\c&H00E6E6E6&}")
            for tl in textwrap.wrap(e["args"].get("text", ""), 38)[:3]:
                log.append(f"     {{\\c&H0050E070&}}{esc(tl)}{{\\c&H00E6E6E6&}}")
            states.append((rel(e["start"] + e["secs"]), panel()))
        elif k == "action":
            mark = "✓" if e["ok"] else "✗"
            log.append(f"   {mark} {esc(e['name'])} {esc(short_args(e['name'], e['args']))[:30]}"
                       f"  {{\\c&H00A0A0A0&}}{e['secs']:.2f} s{{\\c&H00E6E6E6&}}")
            states.append((rel(e["start"] + e["secs"]), panel()))
        elif k == "answer":
            ans = "\\N".join(esc(x) for x in textwrap.wrap(e["content"], 40)[:6])
            log.append(f"{{\\c&H0050E070&}}DONE in {e['steps']} steps{{\\c&H00E6E6E6&}}")
            states.append((rel(e["t"]), panel("\\N" + ans)))
    states.sort(key=lambda x: x[0])
    for i, (a, txt) in enumerate(states):
        b = states[i + 1][0] if i + 1 < len(states) else end_all
        if b > a:
            out.append(line(a, b, "Log", PX, 90, txt))
    return "".join(out), end_all


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("raw")
    ap.add_argument("events")
    ap.add_argument("--video-start", type=float, required=True)
    ap.add_argument("-o", "--out", required=True)
    ap.add_argument("--lead", type=float, default=2.5,
                    help="seconds of recording kept before the prompt is sent; earlier frames are cut "
                         "(they show the screen before the run hid other apps)")
    ap.add_argument("--hands", default="hekaton", help="name of the hands layer shown in the panel")
    ap.add_argument("--blur", action="append", default=[],
                    help="x:y:w:h in recording pixels (1920x1080), optional @start-end seconds")
    args = ap.parse_args()

    events = [json.loads(x) for x in Path(args.events).read_text().splitlines() if x.strip()]
    dur = float(subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", args.raw],
        capture_output=True, text=True, check=True).stdout)
    t_prompt = next(e["t"] for e in events if e["kind"] == "prompt")
    cut = max(0.0, t_prompt - args.video_start - args.lead)
    ass, end_all = build(events, args.video_start + cut, dur - cut, args.hands)
    ass_path = Path(args.out).with_suffix(".ass")
    ass_path.write_text(ass)

    chain = "[0:v]"
    graph = []
    for i, b in enumerate(args.blur):
        box, _, when = b.partition("@")
        x, y, w, h = box.split(":")
        en = ""
        if when:
            a, z = when.split("-")
            en = f":enable='between(t,{a},{z})'"
        r = max(1, min(20, min(int(w), int(h)) // 4 - 1))   # chroma is half size, boxblur radius < half of it
        graph.append(f"{chain}split[m{i}][c{i}];[c{i}]crop={w}:{h}:{x}:{y},boxblur={r}:4[b{i}];"
                     f"[m{i}][b{i}]overlay={x}:{y}{en}[v{i}]")
        chain = f"[v{i}]"
    graph.append(f"{chain}scale={VW}:{VH},pad={W}:{H}:0:0:color=0x101010,"
                 f"drawbox=x=0:y={VH}:w={W}:h={H - VH}:color=0x050505:t=fill,"
                 f"drawbox=x={VW}:y=0:w=2:h={VH}:color=0x303030:t=fill,"
                 f"ass={ass_path}[out]")
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-ss", f"{cut:.3f}", "-i", args.raw,
                    "-filter_complex", ";".join(graph), "-map", "[out]", "-t", f"{end_all:.2f}",
                    "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
                    "-movflags", "+faststart", "-y", args.out], check=True)
    print(args.out)


if __name__ == "__main__":
    main()
