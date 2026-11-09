# Model A/B benchmark

Tells you whether one model drives the desktop-control MCP server better through the `full`
profile or the `simple` profile (or `simple-vision`). Ten small tasks, each checked by the
script itself. Python 3.9+, standard library only.

**This moves the real mouse and keyboard.** It opens browser pages, Calculator and a text
editor on the machine you run it on. Do not touch the mouse or keyboard while it runs, close
anything you care about first, and expect it to take over the screen. Without `--yes` the
script only prints this warning and exits.

## Requirements

- A built `hekaton` binary that supports `--profile` (see `docs/SIMPLE-MODE.md`).
- An OpenAI-compatible chat endpoint with tool calling for the model you want to test.
- macOS or Windows, signed in, screen unlocked, with a default web browser. The server must
  already have the permissions it needs (accessibility, screen recording on macOS).
- Pages are plain `file://` HTML in `bench/fixtures`. No network, no login.

## Run one profile

```
python3 bench/model_ab.py \
  --endpoint http://127.0.0.1:8080/v1 --model my-model \
  --profile simple --hekaton /path/to/hekaton \
  --tasks bench/suites/model-ab.json --out simple.json --yes
```

Options: `--api-key-env VAR` (bearer key read from that env var), `--max-steps N` (default 25),
`--task ID` (repeatable, run a subset), `--vision` (send screenshots to the model as
`image_url` parts; otherwise images become the text `[image omitted]`; use it with
`simple-vision`, and only with a vision model), `--mcp-cmd "..."` (replace the whole server
command).

### Endpoints

| Server | Start | `--endpoint` |
|---|---|---|
| llama-server (llama.cpp) | `llama-server -m model.gguf --jinja --port 8080` | `http://127.0.0.1:8080/v1` |
| Ollama | `ollama serve` (model pulled first) | `http://127.0.0.1:11434/v1` |
| LM Studio | Developer tab, start server | `http://127.0.0.1:1234/v1` |

llama-server needs `--jinja` for tool calls. `--model` is the name the server knows (Ollama
tag, LM Studio model id; llama-server ignores it). For a hosted endpoint put the key in an env
var and pass `--api-key-env`.

## Both profiles back to back, then compare

macOS:

```
python3 bench/model_ab.py --endpoint http://127.0.0.1:8080/v1 --model my-model \
  --profile full   --hekaton ./target/release/hekaton --out full.json   --yes
python3 bench/model_ab.py --endpoint http://127.0.0.1:8080/v1 --model my-model \
  --profile simple --hekaton ./target/release/hekaton --out simple.json --yes
python3 bench/model_ab.py --compare full.json simple.json
```

Windows (PowerShell, same flags, backtick continuation, `.exe` path):

```
python bench\model_ab.py --endpoint http://127.0.0.1:1234/v1 --model my-model `
  --profile full --hekaton .\target\release\hekaton.exe --out full.json --yes
python bench\model_ab.py --endpoint http://127.0.0.1:1234/v1 --model my-model `
  --profile simple --hekaton .\target\release\hekaton.exe --out simple.json --yes
python bench\model_ab.py --compare full.json simple.json
```

Keep the model, endpoint settings and machine state the same between the two runs. Temperature
is 0, but local models can still vary a little, so repeat a run before trusting a small gap.

## What is recorded

Per task: pass, steps (model turns), tool_calls, tool_errors (results flagged `isError`),
bad_calls (unknown tool name or invalid JSON arguments), prompt and completion tokens (summed
from the endpoint's `usage`, `null` if it sends none), seconds, the final answer, and a compact
transcript (tool, args, first 300 characters of each result). The run stops on `done`, on
`--max-steps`, or after 3 turns in a row with no tool call.

## The ten tasks

| id | What | Check |
|---|---|---|
| read-order | read an order number from a page | answer |
| form-save | fill name and email, press Save, report the code | answer |
| radio-quiz | five radio questions, submit, report the code | answer |
| long-bottom | scroll a long page, report the last word | answer |
| timed-dialog | press OK on a dialog that appears after 3 s, report revealed text | answer |
| select-check | choose a select option, tick a checkbox, submit, report the code | answer |
| calc-mul | Calculator: 1234 * 56 | answer |
| editor-save | type a line in TextEdit or Notepad, save as `{tmp}/note.txt` | file_contains |
| copy-between | read a value on page A, type it into page B, report the code | answer |
| table-lookup | report the total in Dana's row | answer |

Confirmation codes are computed in the page's JavaScript from the inputs plus a fixed salt, so
they cannot be guessed. The expected values live in `bench/suites/model-ab.json`; the tests
recompute them with a Python port of the page code.

Checks available to a task: `answer_contains` (case-insensitive, a list means all must appear),
`file_contains` (`{path, text}`), `file_exists`. `{tmp}` is a fresh temp folder per run (removed
at the end) and `{fixtures}` is the `file://` URL of `bench/fixtures`. Prompts may use `{app}`
and `{hint}` with per-OS values.

## Tests

```
python3 -m unittest bench/test_model_ab.py -v
```

Uses a fake MCP server and a fake chat endpoint. It never touches the desktop or a real
`hekaton`.
