#!/usr/bin/env python3
"""Fake stdio MCP server for tests: two tools, no desktop access."""
import json
import sys

TOOLS = [
    {"name": "echo", "description": "Echo text back. Text 'fail' returns an error.",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                     "required": ["text"], "additionalProperties": False}},
    {"name": "shot", "description": "Return a tiny image.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
]


def call(name, args):
    if name == "echo":
        t = str(args.get("text", ""))
        return {"isError": t == "fail", "content": [{"type": "text", "text": "echo: " + t}]}
    if name == "shot":
        return {"content": [{"type": "text", "text": "frame 1x1"},
                            {"type": "image", "mimeType": "image/png", "data": "AAAA"}]}
    return {"isError": True, "content": [{"type": "text", "text": "unknown"}]}


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        if "id" not in msg:
            continue
        m = msg.get("method")
        if m == "initialize":
            res = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                   "serverInfo": {"name": "fake", "version": "0"}}
        elif m == "tools/list":
            res = {"tools": TOOLS}
        elif m == "tools/call":
            p = msg.get("params", {})
            res = call(p.get("name"), p.get("arguments") or {})
        else:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"],
                                         "error": {"code": -32601, "message": "no"}}) + "\n")
            sys.stdout.flush()
            continue
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": res}) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
