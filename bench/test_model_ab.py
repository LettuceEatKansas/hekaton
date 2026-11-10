"""Tests for model_ab.py. No real desktop, no real hekaton binary."""
import contextlib
import io
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import model_ab  # noqa: E402

FAKE_MCP = HERE / "fake_mcp.py"
SUITE = HERE / "suites" / "model-ab.json"
FIX = HERE / "fixtures"


def port_code(s):
    """Python port of the page JS function code(s)."""
    h = 7
    for ch in s:
        h = (h * 31 + ord(ch)) % 1000003
    return "K" + ("000000" + str(h))[-6:]


def salt_of(name):
    return re.search(r'var SALT="([^"]+)"', (FIX / name).read_text()).group(1)


def expected_codes():
    """task id -> code, using the inputs the task prompts tell the model to use."""
    return {
        "form-save": port_code("|".join(["form", "Ada Lovelace", "ada@example.org"]) + "|" + salt_of("form-save.html")),
        "radio-quiz": port_code("|".join(["quiz", "blue", "fish", "winter", "tea", "train"]) + "|" + salt_of("quiz.html")),
        "select-check": port_code("|".join(["opts", "large", "yes"]) + "|" + salt_of("options.html")),
        "copy-between": port_code("|".join(["link", "7391-QX"]) + "|" + salt_of("link-b.html")),
    }


class FakeLLM:
    """OpenAI-style endpoint that replays scripted assistant messages."""

    def __init__(self, script):
        self.script = list(script)
        self.requests = []
        outer = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                outer.requests.append({"path": self.path, "auth": self.headers.get("Authorization"),
                                       "body": body})
                msg = outer.script.pop(0) if outer.script else {"role": "assistant", "content": "hm"}
                out = json.dumps({"choices": [{"message": msg}],
                                  "usage": {"prompt_tokens": 10, "completion_tokens": 2}}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(out)))
                self.end_headers()
                self.wfile.write(out)

        self.srv = HTTPServer(("127.0.0.1", 0), H)
        self.url = "http://127.0.0.1:%d/v1" % self.srv.server_port
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def close(self):
        self.srv.shutdown()
        self.srv.server_close()


def call(name, args, cid="c1"):
    a = args if isinstance(args, str) else json.dumps(args)
    return {"role": "assistant", "content": None,
            "tool_calls": [{"id": cid, "type": "function", "function": {"name": name, "arguments": a}}]}


def run_main(argv):
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        rc = model_ab.main(argv)
    return rc, buf.getvalue()


class LoopTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        argv = [sys.executable, str(FAKE_MCP)]
        self.mcp_cmd = subprocess.list2cmdline(argv) if os.name == "nt" else shlex.join(argv)

    def suite(self, tasks):
        p = Path(self.dir.name) / "suite.json"
        p.write_text(json.dumps({"tasks": tasks}))
        return str(p)

    def run_suite(self, tasks, script, extra=()):
        llm = FakeLLM(script)
        self.addCleanup(llm.close)
        out = str(Path(self.dir.name) / "out.json")
        rc, text = run_main(["--endpoint", llm.url, "--model", "m", "--mcp-cmd", self.mcp_cmd,
                             "--tasks", self.suite(tasks), "--out", out, "--yes", *extra])
        self.assertEqual(rc, 0, text)
        with open(out) as f:
            return json.load(f), llm, text

    def test_loop_and_counters(self):
        script = [call("nope", {}), call("echo", {"text": "hi"}), call("done", {"answer": "Code ABC-1"})]
        res, llm, text = self.run_suite(
            [{"id": "t", "prompt": "do it", "check": {"answer_contains": "abc-1"}}], script)
        r = res["tasks"][0]
        self.assertTrue(r["pass"])
        self.assertEqual((r["steps"], r["tool_calls"], r["bad_calls"], r["tool_errors"]), (3, 2, 1, 0))
        self.assertEqual((r["prompt_tokens"], r["completion_tokens"]), (30, 6))
        self.assertEqual(r["stop"], "done")
        self.assertEqual([t["tool"] for t in r["transcript"]], ["nope", "echo"])
        self.assertIn("echo: hi", r["transcript"][1]["result"])
        self.assertIn("TOTAL passed 1/1", text)
        body = llm.requests[0]["body"]
        self.assertEqual(llm.requests[0]["path"], "/v1/chat/completions")
        self.assertEqual(body["temperature"], 0)
        self.assertEqual(body["tool_choice"], "auto")
        self.assertEqual([t["function"]["name"] for t in body["tools"]], ["echo", "shot", "done"])
        self.assertIn("not instructions", body["messages"][0]["content"])
        self.assertIn(model_ab.os_name(), body["messages"][0]["content"])
        last = llm.requests[2]["body"]["messages"]
        self.assertEqual(last[-1]["role"], "tool")
        self.assertEqual(last[-1]["content"], "echo: hi")

    def test_tool_error_and_bad_json(self):
        script = [call("echo", {"text": "fail"}), call("echo", "{not json", "c2"),
                  call("done", {"answer": "x"})]
        res, _, _ = self.run_suite([{"id": "t", "prompt": "p", "check": {"answer_contains": "x"}}], script)
        r = res["tasks"][0]
        self.assertEqual((r["tool_errors"], r["bad_calls"], r["tool_calls"]), (1, 1, 2))

    def test_idle_stop_and_max_steps(self):
        res, _, _ = self.run_suite([{"id": "t", "prompt": "p", "check": {"answer_contains": "x"}}], [])
        r = res["tasks"][0]
        self.assertEqual((r["stop"], r["steps"], r["pass"]), ("idle", 3, False))
        script = [call("echo", {"text": "a"}, "c%d" % i) for i in range(5)]
        res, _, _ = self.run_suite([{"id": "t", "prompt": "p", "check": {"answer_contains": "x"}}],
                                   script, ["--max-steps", "2"])
        r = res["tasks"][0]
        self.assertEqual((r["stop"], r["steps"]), ("max_steps", 2))

    def test_image_omitted_and_vision(self):
        script = [call("shot", {}), call("done", {"answer": "ok"})]
        t = [{"id": "t", "prompt": "p", "check": {"answer_contains": "ok"}}]
        res, llm, _ = self.run_suite(t, script)
        self.assertIn("[image omitted]", res["tasks"][0]["transcript"][0]["result"])
        self.assertNotIn("image_url", json.dumps(llm.requests[1]["body"]))
        res, llm, _ = self.run_suite(t, [call("shot", {}), call("done", {"answer": "ok"})], ["--vision"])
        self.assertIn("image_url", json.dumps(llm.requests[1]["body"]))
        self.assertNotIn("[image omitted]", res["tasks"][0]["transcript"][0]["result"])

    def test_checks_and_expansion(self):
        # file_contains / file_exists / answer list; {tmp} and {fixtures} expand
        tasks = [{"id": "files", "prompt": "see {fixtures}/a.html and {tmp}/n.txt",
                  "check": {"answer_contains": ["alpha", "BETA"],
                            "file_exists": "{tmp}/n.txt",
                            "file_contains": {"path": "{tmp}/n.txt", "text": "hello"}}}]
        # the answer carries the expanded tmp path so we can write the file via a hook:
        ctx = {"tmp": tempfile.mkdtemp(), "fixtures": model_ab.FIXTURES_DIR.as_uri(), "api_key": None}
        self.addCleanup(lambda: __import__("shutil").rmtree(ctx["tmp"], ignore_errors=True))
        p = model_ab.expand_prompt(tasks[0], ctx)
        self.assertIn(os.path.join(ctx["tmp"], "n.txt"), p)
        self.assertTrue(p.startswith("see file://"))
        self.assertIn("/bench/fixtures/a.html", p)
        chk = tasks[0]["check"]
        self.assertFalse(model_ab.run_checks(chk, "alpha beta", ctx)[0])  # no file yet
        Path(ctx["tmp"], "n.txt").write_text("say hello there")
        self.assertTrue(model_ab.run_checks(chk, "Alpha and beta", ctx)[0])
        self.assertFalse(model_ab.run_checks(chk, "alpha only", ctx)[0])
        Path(ctx["tmp"], "n.txt").write_text("nothing")
        ok, why = model_ab.run_checks(chk, "alpha beta", ctx)
        self.assertFalse(ok)
        self.assertIn("lacks", why)

    def test_app_and_hint_per_os(self):
        ctx = {"tmp": "T", "fixtures": "F", "api_key": None}
        task = {"prompt": "Use {app}. {hint}", "app": {"macos": "TextEdit", "windows": "Notepad"},
                "hint": {"macos": "M", "windows": "W", "any": "Z"}}
        out = model_ab.expand_prompt(task, ctx)
        if model_ab.os_key() == "macos":
            self.assertEqual(out, "Use TextEdit. M")
        elif model_ab.os_key() == "windows":
            self.assertEqual(out, "Use Notepad. W")
        else:
            self.assertEqual(out, "Use . Z")

    def test_file_check_end_to_end(self):
        # setup runs a command that writes into {tmp}; check reads it back
        setup = [[sys.executable, "-c", "import sys;open(sys.argv[1],'w').write('hello world')",
                  "{tmp}/note.txt"]]
        tasks = [{"id": "t", "prompt": "p", "setup": setup,
                  "check": {"file_contains": {"path": "{tmp}/note.txt", "text": "hello"}}}]
        res, _, _ = self.run_suite(tasks, [call("done", {"answer": "x"})])
        self.assertTrue(res["tasks"][0]["pass"])

    def test_task_subset_and_unknown(self):
        t = [{"id": "a", "prompt": "p", "check": {"answer_contains": "x"}},
             {"id": "b", "prompt": "p", "check": {"answer_contains": "x"}}]
        res, _, _ = self.run_suite(t, [call("done", {"answer": "x"})], ["--task", "b"])
        self.assertEqual([r["id"] for r in res["tasks"]], ["b"])
        rc, text = run_main(["--endpoint", "http://x/v1", "--model", "m", "--mcp-cmd", "x",
                             "--tasks", self.suite(t), "--task", "zzz", "--yes"])
        self.assertEqual(rc, 2)

    def test_api_key_header(self):
        os.environ["MODEL_AB_TEST_KEY"] = "sekret"
        self.addCleanup(os.environ.pop, "MODEL_AB_TEST_KEY", None)
        _, llm, _ = self.run_suite([{"id": "t", "prompt": "p", "check": {}}],
                                   [call("done", {"answer": "x"})], ["--api-key-env", "MODEL_AB_TEST_KEY"])
        self.assertEqual(llm.requests[0]["auth"], "Bearer sekret")

    def test_refuses_without_yes(self):
        rc, text = run_main(["--endpoint", "http://127.0.0.1:1/v1", "--model", "m",
                             "--hekaton", "/nonexistent/hekaton"])
        self.assertEqual(rc, 2)
        self.assertIn("REAL desktop", text)
        self.assertIn("--yes", text)

    def test_compare(self):
        def rec(i, ok, steps, err, p):
            return {"id": i, "pass": ok, "steps": steps, "tool_calls": steps, "tool_errors": err,
                    "bad_calls": 0, "prompt_tokens": p, "completion_tokens": 1, "seconds": 1.5}
        a = {"model": "m", "profile": "full", "tasks": [rec("t1", False, 9, 3, 900), rec("t2", True, 4, 0, 400)]}
        b = {"model": "m", "profile": "simple", "tasks": [rec("t1", True, 3, 0, 300), rec("t2", True, 4, 0, None)]}
        pa, pb = Path(self.dir.name) / "a.json", Path(self.dir.name) / "b.json"
        pa.write_text(json.dumps(a))
        pb.write_text(json.dumps(b))
        rc, text = run_main(["--compare", str(pa), str(pb)])
        self.assertEqual(rc, 0)
        self.assertIn("m/full", text)
        self.assertIn("m/simple", text)
        row = [ln for ln in text.splitlines() if ln.startswith("t1")][0].split()
        self.assertEqual(row[:3], ["t1", "FAIL", "PASS"])
        self.assertIn("TOTAL A passed 1/2", text)
        self.assertIn("TOTAL B passed 2/2", text)


class SuiteTests(unittest.TestCase):
    def setUp(self):
        self.tasks = json.loads(SUITE.read_text())["tasks"]

    def test_shape(self):
        self.assertEqual(len(self.tasks), 10)
        ids = [t["id"] for t in self.tasks]
        self.assertEqual(len(set(ids)), 10)
        for t in self.tasks:
            self.assertTrue(t.get("id") and t.get("prompt") and t.get("check"), t)
            self.assertTrue(set(t["check"]) <= {"answer_contains", "file_contains", "file_exists"})

    def test_fixtures_exist_and_clean(self):
        used = set()
        for t in self.tasks:
            used.update(re.findall(r"\{fixtures\}/([\w.-]+)", t["prompt"]))
        self.assertTrue(used)
        for name in used:
            self.assertTrue((FIX / name).exists(), name)
        for p in list(FIX.glob("*.html")) + [SUITE, HERE / "model_ab.py", HERE / "README-model-ab.md"]:
            self.assertNotIn(chr(0x2014), p.read_text(), p)

    def test_js_function_is_the_ported_one(self):
        fn = 'function code(s){var h=7;for(var i=0;i<s.length;i++){h=(h*31+s.charCodeAt(i))%1000003;}return "K"+("000000"+h).slice(-6);}'
        for name in ("form-save.html", "quiz.html", "options.html", "link-b.html"):
            self.assertIn(fn, (FIX / name).read_text(), name)

    def test_expected_codes_are_right(self):
        by_id = {t["id"]: t for t in self.tasks}
        for tid, want in expected_codes().items():
            self.assertEqual(by_id[tid]["check"]["answer_contains"], [want], tid)

    def test_prompts_give_the_inputs_used(self):
        by_id = {t["id"]: t["prompt"] for t in self.tasks}
        for s in ("Ada Lovelace", "ada@example.org"):
            self.assertIn(s, by_id["form-save"])
        for s in ("Blue", "Fish", "Winter", "Tea", "Train"):
            self.assertIn(s, by_id["radio-quiz"])
        self.assertIn("Large", by_id["select-check"])
        self.assertIn("Gift wrap", by_id["select-check"])

    def test_static_answers_match_pages(self):
        by_id = {t["id"]: t for t in self.tasks}
        pages = {"read-order": "order.html", "long-bottom": "long.html", "timed-dialog": "dialog.html"}
        for tid, page in pages.items():
            for w in by_id[tid]["check"]["answer_contains"]:
                self.assertIn(w.lower(), (FIX / page).read_text().lower(), tid)
        self.assertIn("69104", by_id["calc-mul"]["check"]["answer_contains"])
        self.assertEqual(1234 * 56, 69104)
        self.assertIn("842.75", by_id["table-lookup"]["check"]["answer_contains"])
        self.assertIn("<td>Dana</td><td>East</td><td>402.75</td><td>440.00</td><td>842.75</td>",
                      (FIX / "table.html").read_text())
        self.assertAlmostEqual(402.75 + 440.00, 842.75)


if __name__ == "__main__":
    unittest.main()
