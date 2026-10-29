"""Gauntlet L2, big ledger: 100,000 rows into Excel through the excel power channel, checked from the file.

    python bench/l2_ledger.py --target new [--rows 100000] [--runs 1]

The daemon must run with power_channels containing "excel". Rows are generated from a fixed seed, so
the expected totals are computed here in Python. A summary sheet holds SUMIFS, SUMPRODUCT and XLOOKUP
formulas; after the save the .xlsx is parsed with the standard library and every summary value,
every formula and a random sample of data cells are compared. The tool's own text is not trusted.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import sys
import time
import zipfile
import xml.etree.ElementTree as ET
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from l1_more import Client  # noqa: E402

OUT = Path(r"C:\Users\ikill\tools\hekaton-test-state\gauntlet\l2-ledger")
NS = {"m": "http://schemas.openxmlformats.org/spreadsheetml/2006/main"}
M = f"{{{NS['m']}}}"
REGIONS = ["North", "South", "East", "West"]
PRODUCTS = [f"SKU-{i:02d}" for i in range(1, 13)]
HEADER = ["Day", "Region", "Product", "Qty", "Price"]
# HTTP bodies are capped at 2 MiB, so the rows go over in blocks well under that.
CHUNK = 12_000


def ledger(rows: int) -> list[list]:
    rnd = random.Random(20261028)
    return [[1 + rnd.randrange(365), rnd.choice(REGIONS), rnd.choice(PRODUCTS),
             rnd.randint(1, 50), round(rnd.uniform(0.5, 99.5), 2)] for _ in range(rows)]


def summary(rows: int) -> list[list]:
    """The Summary sheet: one row per region, a total row and a lookup row."""
    last = rows + 1
    block = [["Region", "Qty", "Revenue"]]
    for i, region in enumerate(REGIONS, start=2):
        block.append([region,
                      f'=SUMIFS(Sheet1!D2:D{last},Sheet1!B2:B{last},A{i})',
                      f'=SUMPRODUCT((Sheet1!B2:B{last}=A{i})*Sheet1!D2:D{last}*Sheet1!E2:E{last})'])
    n = len(REGIONS) + 2
    block.append(["Total", f"=SUM(B2:B{n - 1})", f"=SUM(C2:C{n - 1})"])
    block.append(["First SKU-07 price", f'=XLOOKUP("SKU-07",Sheet1!C2:C{last},Sheet1!E2:E{last})', ""])
    return block


def expected(data: list[list]) -> dict[str, float]:
    want: dict[str, float] = {}
    for i, region in enumerate(REGIONS, start=2):
        mine = [r for r in data if r[1] == region]
        want[f"B{i}"] = sum(r[3] for r in mine)
        want[f"C{i}"] = sum(r[3] * r[4] for r in mine)
    n = len(REGIONS) + 2
    want[f"B{n}"] = sum(r[3] for r in data)
    want[f"C{n}"] = sum(r[3] * r[4] for r in data)
    want[f"B{n + 1}"] = next(r[4] for r in data if r[2] == "SKU-07")
    return want


def sheet_cells(z: zipfile.ZipFile, name: str, shared: list[str]) -> tuple[dict, dict]:
    values, formulas = {}, {}
    with z.open(name) as f:
        for _, el in ET.iterparse(f):
            if el.tag != M + "c":
                continue
            ref, kind = el.get("r"), el.get("t")
            v, fm = el.find(M + "v"), el.find(M + "f")
            if fm is not None and fm.text:
                formulas[ref] = fm.text
            if v is not None:
                values[ref] = shared[int(v.text)] if kind == "s" else (v.text or "")
            el.clear()
    return values, formulas


def read_book(path: Path) -> dict[str, tuple[dict, dict]]:
    with zipfile.ZipFile(path) as z:
        shared = []
        if "xl/sharedStrings.xml" in z.namelist():
            for si in ET.fromstring(z.read("xl/sharedStrings.xml")).findall("m:si", NS):
                shared.append("".join(t.text or "" for t in si.iter(M + "t")))
        return {n: sheet_cells(z, n, shared) for n in z.namelist()
                if re.fullmatch(r"xl/worksheets/sheet\d+\.xml", n)}


def col(i: int) -> str:
    return "ABCDE"[i]


def run(c: Client, path: Path, rows: int) -> dict:
    if path.exists():
        path.unlink()
    data = ledger(rows)
    calls = []

    def call(args: dict, timeout: float = 120) -> str:
        t = time.perf_counter()
        ok, text, _ = c({"action": "excel", **args}, timeout=timeout)
        calls.append({"op": args["op"], "ok": ok, "ms": round((time.perf_counter() - t) * 1000)})
        if not ok:
            raise RuntimeError(f"{args['op']} failed: {text[-400:]}")
        return text

    t0 = time.perf_counter()
    try:
        text = call({"op": "new"})
        m = re.search(r"\b(x\d+)\b", text)
        if not m:
            raise RuntimeError(f"no workbook handle in {text[-200:]!r}")
        book = m.group(1)
        call({"op": "write", "workbook": book, "range": "A1", "values": [HEADER]})
        for start in range(0, rows, CHUNK):
            block = data[start:start + CHUNK]
            call({"op": "write", "workbook": book, "range": f"A{start + 2}", "values": block})
        call({"op": "add_sheet", "workbook": book, "name": "Summary"})
        call({"op": "write", "workbook": book, "sheet": "Summary", "range": "A1",
              "values": summary(rows)})
        call({"op": "calc", "workbook": book})
        call({"op": "save_as", "workbook": book, "path": str(path)}, timeout=180)
        call({"op": "close", "workbook": book})
    except RuntimeError as e:
        return {"ok": False, "why": str(e), "calls": calls}
    wall = time.perf_counter() - t0

    if not path.exists():
        return {"ok": False, "why": "no file", "calls": calls}
    sheets = read_book(path)
    by_rows = sorted(sheets.values(), key=lambda s: len(s[0]), reverse=True)
    data_vals, summ = by_rows[0][0], by_rows[1]
    problems = []
    rnd = random.Random(7)
    for r in rnd.sample(range(rows), min(300, rows)):
        for ci, want in enumerate(data[r]):
            got = data_vals.get(f"{col(ci)}{r + 2}")
            if isinstance(want, str):
                bad = got != want
            else:
                bad = got is None or abs(float(got) - want) > 1e-9
            if bad:
                problems.append((f"{col(ci)}{r + 2}", got, want))
    if f"E{rows + 1}" not in data_vals or f"A{rows + 2}" in data_vals:
        problems.append(("row count", None, rows))
    for ref, want in expected(data).items():
        got = summ[0].get(ref)
        if got is None or abs(float(got) - want) > 1e-6 * max(1.0, abs(want)):
            problems.append((ref, got, want))
    for ref in ["B2", "C2", "B6", "C6", "B7"]:
        if ref not in summ[1]:
            problems.append((ref, "no formula", "formula"))
    return {"ok": not problems, "wall_s": round(wall, 2), "problems": problems[:10],
            "calls": len(calls), "write_ms": sum(x["ms"] for x in calls if x["op"] == "write"),
            "save_ms": next(x["ms"] for x in calls if x["op"] == "save_as"),
            "file_mb": round(path.stat().st_size / 1e6, 2)}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", required=True, choices=["old", "new"])
    ap.add_argument("--rows", type=int, default=100_000)
    ap.add_argument("--runs", type=int, default=1)
    a = ap.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    c = Client(a.target)
    failed = 0
    for i in range(a.runs):
        r = run(c, OUT / f"ledger-{a.target}-{i}.xlsx", a.rows)
        failed += not r["ok"]
        print(json.dumps({"run": i, **r}))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
