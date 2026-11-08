"""Gauntlet L2: build a small Excel table with formulas through the GUI, save it, check the file.

    python bench/l2_excel.py --target new [--runs 1]

One batched call per run, input limited to excel.exe, no screenshots returned. The saved .xlsx is
unzipped and its cells and formulas are compared, so a wrong cell or a lost formula fails the run
whatever the tool's own text said.
"""

from __future__ import annotations

import argparse
import re
import sys
import time
import zipfile
import xml.etree.ElementTree as ET
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from l1_more import Client  # noqa: E402

OUT = Path(r"C:\Users\you\tools\hekaton-test-state\gauntlet\l2-excel")
TABLE = "Item\tQty\tPrice\napple\t3\t1.25\npear\t5\t0.8\nplum\t7\t2.5\n"
WANT = {
    "A1": "Item", "B1": "Qty", "C1": "Price",
    "A2": "apple", "B2": "3", "C2": "1.25",
    "A3": "pear", "B3": "5", "C3": "0.8",
    "A4": "plum", "B4": "7", "C4": "2.5",
    "A5": "Total", "B5": "15", "C5": "25.25",
}
WANT_FORMULAS = {"B5": "SUM(B2:B4)", "C5": "SUMPRODUCT(B2:B4,C2:C4)"}
NS = {"m": "http://schemas.openxmlformats.org/spreadsheetml/2006/main"}


def cells(path: Path) -> tuple[dict[str, str], dict[str, str]]:
    with zipfile.ZipFile(path) as z:
        shared = []
        if "xl/sharedStrings.xml" in z.namelist():
            for si in ET.fromstring(z.read("xl/sharedStrings.xml")).findall("m:si", NS):
                shared.append("".join(t.text or "" for t in si.iter(f"{{{NS['m']}}}t")))
        sheet = ET.fromstring(z.read("xl/worksheets/sheet1.xml"))
    values, formulas = {}, {}
    for c in sheet.iter(f"{{{NS['m']}}}c"):
        ref, kind = c.get("r"), c.get("t")
        v, f = c.find("m:v", NS), c.find("m:f", NS)
        if f is not None and f.text:
            formulas[ref] = f.text
        if v is None:
            is_ = c.find("m:is", NS)
            values[ref] = "".join(t.text or "" for t in is_.iter(f"{{{NS['m']}}}t")) if is_ is not None else ""
        elif kind == "s":
            values[ref] = shared[int(v.text)]
        else:
            text = v.text or ""
            values[ref] = re.sub(r"\.0+$", "", text) if re.fullmatch(r"-?\d+\.0+", text) else text
    return values, formulas


def run(c: Client, path: Path) -> dict:
    if path.exists():
        path.unlink()
    args = {"actions": [
        {"action": "open_app", "text": "excel", "reuse": False},
        # The start screen shows Blank workbook selected, but an Enter sent while it is still
        # loading is dropped (2026-10-28, 1 run in 3). Clicking the tile waits for it to exist.
        {"action": "click_text", "text": "Blank workbook"},
        {"action": "wait_for_text", "text": "Sheet1", "timeout": 8},  # the grid's sheet tab
        {"action": "type", "text": TABLE},
        {"action": "key", "text": "ctrl+g"},
        {"action": "type", "text": "A5\n"},
        {"action": "type", "text": "Total\t=SUM(B2:B4)\t=SUMPRODUCT(B2:B4,C2:C4)\n"},
        {"action": "key", "text": "f12"},
        {"action": "file_dialog", "text": str(path)},
        {"action": "wait_for_file", "text": str(path), "timeout": 8},
        {"action": "key", "text": "alt+f4"},  # saved, so Excel closes without a prompt
    ], "only_apps": ["excel.exe"]}
    t = time.perf_counter()
    ok, text, res = c(args, timeout=120)
    wall = time.perf_counter() - t
    result = {"call_ok": ok, "wall_s": round(wall, 2)}
    if not path.exists():
        return {**result, "ok": False, "why": "no file", "detail": text[-600:]}
    time.sleep(0.5)
    values, formulas = cells(path)
    bad = {k: (values.get(k), v) for k, v in WANT.items() if values.get(k) != v}
    bad_f = {k: (formulas.get(k), v) for k, v in WANT_FORMULAS.items() if formulas.get(k) != v}
    return {**result, "ok": not bad and not bad_f, "wrong_cells": bad, "wrong_formulas": bad_f,
            "detail": text[-600:]}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", required=True, choices=["old", "new"])
    ap.add_argument("--runs", type=int, default=1)
    a = ap.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    c = Client(a.target)
    failed = 0
    for i in range(a.runs):
        r = run(c, OUT / f"l2-{a.target}-{i}.xlsx")
        failed += not r["ok"]
        print(f"run {i}: {r}")
        time.sleep(1.5)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
