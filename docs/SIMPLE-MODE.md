# Simple mode: a small tool set for small models

Status: spec, 2026-11-09. Branch `hekaton-simple-mode`.

## Why

The full `computer` tool is one tool with about 50 action names and more than 50 optional
parameters. Nothing is required and the action name is free text, so the model has to remember
exact spellings and which parameters go with which action. Large models cope. Small local models
guess, and a week of audit logs shows that even large models trip on names (`click`, `hover`).

Simple mode serves the same hands through a handful of small, strict tools that work from text,
not pictures. Nothing about the daemon or the safety layer changes.

## How it is started

```
hekaton mcp                          # full profile, exactly as today
hekaton mcp --profile simple         # text tools only
hekaton mcp --profile simple-vision  # text tools plus screenshot and click_at
```

`HEKATON_MCP_PROFILE` sets the same thing where a client cannot pass arguments; the flag wins.
An unknown profile name exits with status 2 and prints the usage line.

## The tools

`simple` lists these nine tools, in this order.

| Tool | Parameters (required in bold) | What it does | Becomes |
|---|---|---|---|
| `look` | `window`, `filter` | Front window title and app, open dialogs first, then a numbered list of things that can be pressed or typed into | `read` (browser tree woken, no screenshot) |
| `read_text` | `find` | The text of the window or page | `read_text` |
| `press` | **`target`**, `double` | Press item N from the newest list, or the control with that visible label | `press` when `target` is all digits, otherwise `click_text` |
| `type` | **`text`**, `into`, `clear`, `submit` | Type text. `into` (number or label) is pressed first, `clear` replaces what is there, `submit` presses Return after | a batch of `press`/`click_text`, `key`, `type`, `key` |
| `key` | **`keys`**, `repeat` | Press a key or combination, for example `Return` or `cmd+s` | `key` |
| `open` | **`target`** | Open an app by name, or a web address in the browser | `open_app` |
| `scroll` | **`direction`** (`up`, `down`, `left`, `right`), `amount` | Scroll the front window | `scroll` |
| `wait` | `seconds`, `for_text` | Wait a fixed time, or until new text appears on screen | `wait`, or `wait_for_text` with `new_only=true` |
| `approve` | **`code`** | Repeat the last refused step with the code the owner gave | the stored call plus `approval_code` |

`simple-vision` adds two more, after the nine:

| Tool | Parameters | What it does | Becomes |
|---|---|---|---|
| `screenshot` | none | One picture of the screen with its frame size stated | `screenshot` |
| `click_at` | **`x`**, **`y`**, `button` (`left`, `right`), `double` | Click a point in the newest screenshot | `left_click`, `right_click` or `double_click` |

The server always sends the same `shot_long`, so the click frame never changes between a
screenshot and a click.

## Behaviour

1. **Strict schemas.** Every tool has a `required` list, `additionalProperties: false`, enums
   where the values are a fixed list, and a one-line description on every parameter. No tool
   description is longer than 200 characters. The nine simple tools together, as compact JSON,
   stay under 3,200 characters.
2. **Translation only.** Each simple call is turned into an ordinary `computer` payload and sent
   down the same path as a full-profile call. The approval gate, `only_apps`, destructive
   refusals, the human-input pause, the audit log and the owner card all apply unchanged. There
   are no new daemon routes.
3. **Every acting reply carries a fresh list.** `press`, `type`, `key`, `open`, `scroll`, `wait`
   and `click_at` answer with one line saying what happened, then the current numbered list
   (at most 40 items and 2,500 characters; `look` allows 60 items and 4,000 characters). The
   numbers in a reply are therefore always current, and the model does not need a `look` between
   two actions. When more items exist than were shown, the reply says how many and suggests
   `look` with a `filter`. When the action fails, the reply is the error alone.
4. **No pictures in `simple`.** No reply contains an image. In `simple-vision` only `screenshot`
   returns one.
5. **Lenient values.** Numbers may arrive as numbers or as digit strings (`12` or `"12"`),
   booleans as booleans or `"true"`/`"false"`. An argument the tool does not know is ignored and
   named in a one-line note, not refused.
6. **Errors that say what to do next.** Every error ends with a line starting `Next:` that names
   the exact call to make. A stale number says to call `look` and press the new number. An
   ambiguous label lists the matches with their numbers. A missing required parameter shows an
   example call. An unknown tool name lists the tool names.
7. **`approve`.** The server remembers the last refused call of the session in memory.
   `approve` with the owner's code sends it again with `approval_code`. With nothing waiting, it
   says so. Clients that support the owner card keep getting the card.
8. **Aliases in the full tool.** In the full profile and in batch steps, the action `click` means
   `left_click` and `hover` means `mouse_move`. The full description and schema do not change.
9. **The default does not move.** With no profile, `tools/list` is byte for byte what it was.

## Acceptance criteria

| # | Criterion | How it is checked |
|---|---|---|
| AC1 | The full profile is unchanged | The existing definition and parity tests pass untouched |
| AC2 | `simple` lists the nine names in order, `simple-vision` eleven; an unknown profile exits 2 | Unit tests and a CLI test |
| AC3 | Schemas are strict and within the size budget, with no em-dash in any text | A test over every tool definition |
| AC4 | Every tool translates to the payloads in the table above, including `press` with `12`, `"12"` and `"Submit"`, `type` with `into`, `clear` and `submit`, and `wait` with `for_text` | Table-driven unit tests |
| AC5 | Acting replies carry the outcome line and the fresh list; a failed action carries the error and no list | Tests against the fake daemon used by the existing server tests |
| AC6 | `approve` resends the stored call with the code; with nothing stored it says so | Tests against the fake daemon |
| AC7 | Stale number, missing required parameter and unknown tool each end with a `Next:` line | Unit tests |
| AC8 | `click` and `hover` work as actions in the full tool, alone and as batch steps | Core unit tests |
| AC9 | `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass on macOS; the change has no platform-specific code | Run locally |
| AC10 | The built binary answers `initialize` and `tools/list` for each profile over stdio | A scripted stdio session; no input is sent to the desktop |

## Not in this change

- New daemon routes, or any browser-API (CDP/DOM) action.
- Any edit to the full tool's description or schema.
- Making `only_apps` accept app name, process name and bundle id alike. That is a separate fix.
- Replacing an installed app. The build is tested from the build folder only.

## Measuring it

`bench/model_ab.py` runs the same ten self-checking tasks with the same model against the full
profile and the simple profile and reports, per task: pass or fail, steps, tool errors, tokens
and seconds. See `bench/README-model-ab.md`.
