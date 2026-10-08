"""Tests for compare.py against two throwaway mock servers. No winhands daemon is involved.

    python -m unittest discover -s tests/parity
"""
import base64
import json
import struct
import threading
import unittest
import zlib
from http.server import BaseHTTPRequestHandler, HTTPServer

import compare


def png(width: int, height: int) -> bytes:
    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    raw = b"".join(b"\x00" + b"\x00\x00\x00" * width for _ in range(height))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def jpeg(width: int, height: int) -> bytes:
    # SOI, an APP0 segment, then SOF0 with the size; enough for a size reader
    app0 = b"\xff\xe0" + struct.pack(">H", 16) + b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00"
    sof = b"\xff\xc0" + struct.pack(">HBHHB", 11, 8, height, width, 1) + b"\x01\x11\x00"
    return b"\xff\xd8" + app0 + sof + b"\xff\xd9"


def mock_daemon(health: dict, answers: dict):
    """A server that answers /health and POST /computer from `answers[action]`."""

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def _send(self, code, payload):
            body = json.dumps(payload).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.headers.get("X-Winhands-Token") != "tok":
                return self._send(401, {"error": "unauthorized"})
            self._send(200, health)

        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            args = json.loads(self.rfile.read(length) or b"{}")
            answer = answers.get(args.get("action"), {"ok": False, "error": "unknown"})
            self._send(200 if answer.get("ok") else 400, answer)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def text_answer(text: str, *images: tuple[str, bytes]) -> dict:
    content = [{"type": "text", "text": text}]
    content += [{"type": "image", "data": base64.b64encode(data).decode(), "mimeType": mime} for mime, data in images]
    return {"ok": True, "result": text, "content": content}


HEALTH = {"elevated": True, "paused": False, "version": "0.1.0", "hotkey": None, "idle_remaining_seconds": 59.0,
          "idle_timeout_seconds": 60, "ocr": "winrt"}


def answers(shot=(1024, 576), windows='w1 "Notepad" notepad.exe 100x200', cursor="cursor at [10, 20]"):
    return {
        "screenshot": text_answer("screenshot; give coordinates in pixels of this image", ("image/jpeg", jpeg(*shot))),
        "list_windows": text_answer(windows),
        "cursor_position": text_answer(cursor),
        "list_files": text_answer("a.txt  12 bytes"),
        "read": text_answer('window "Notepad"\n1 Edit "Text" @5,5'),
        "find_text": text_answer("no text found"),
    }


class Shapes(unittest.TestCase):
    def test_numbers_and_quoted_strings_fold_away(self):
        self.assertEqual(compare.shape('w3 "Untitled - Notepad" 1200x800 @10,20'), 'wN "S" NxN @N,N')
        self.assertEqual(compare.shape("cursor at [640, 360]"), "cursor at [N, N]")
        self.assertEqual(compare.shape("it's 'x'"), "it's 'x'".replace("'x'", "'S'"))

    def test_image_sizes_of_png_and_jpeg(self):
        self.assertEqual(compare.image_size(png(7, 5)), (7, 5))
        self.assertEqual(compare.image_size(jpeg(1024, 576)), (1024, 576))
        self.assertIsNone(compare.image_size(b"GIF89a"))
        self.assertIsNone(compare.image_size(b""))

    def test_health_ignores_the_rust_extras_and_catches_the_rest(self):
        rust = dict(HEALTH, impl="winhands-rs", backend="windows")
        self.assertEqual(compare.compare_health(HEALTH, rust), [])
        missing = {k: v for k, v in rust.items() if k != "ocr"}
        self.assertTrue(any("'ocr' only in a" in d for d in compare.compare_health(HEALTH, missing)))
        retyped = dict(rust, paused="no")
        self.assertTrue(any("'paused'" in d for d in compare.compare_health(HEALTH, retyped)))
        self.assertTrue(compare.compare_health(None, rust))
        self.assertEqual(compare.compare_health(dict(HEALTH, idle_remaining_seconds=3), rust), [])


class AgainstMockDaemons(unittest.TestCase):
    def run_pair(self, a_answers, b_answers, a_health=HEALTH, b_health=HEALTH):
        sa, sb = mock_daemon(a_health, a_answers), mock_daemon(b_health, b_answers)
        self.addCleanup(sa.shutdown)
        self.addCleanup(sb.shutdown)
        a = compare.Daemon("a", f"http://127.0.0.1:{sa.server_address[1]}", "tok")
        b = compare.Daemon("b", f"http://127.0.0.1:{sb.server_address[1]}", "tok")
        return compare.run(a, b, ".", "File", "")

    def test_identical_daemons_have_no_differences(self):
        report = self.run_pair(answers(), answers())
        self.assertEqual(report["diffs"], [])
        self.assertEqual(report["checked"], ["health", "screenshot", "list_windows", "cursor_position",
                                             "list_files", "read", "find_text"])

    def test_a_moved_window_is_a_note_not_a_difference(self):
        report = self.run_pair(answers(), answers(windows='w1 "Notepad" notepad.exe 300x400'))
        self.assertEqual(report["diffs"], [])
        self.assertTrue(any("list_windows" in n for n in report["notes"]))

    def test_a_different_picture_size_is_a_difference(self):
        report = self.run_pair(answers(), answers(shot=(1280, 720)))
        self.assertTrue(any("image 1 is (1024, 576) in a and (1280, 720) in b" in d for d in report["diffs"]), report)

    def test_a_different_text_shape_is_a_difference(self):
        report = self.run_pair(answers(), answers(cursor="pointer somewhere"))
        self.assertTrue(any(d.startswith("cursor_position: text shape differs") for d in report["diffs"]), report)

    def test_a_missing_action_is_a_difference(self):
        broken = answers()
        del broken["read"]
        report = self.run_pair(answers(), broken)
        self.assertTrue(any(d.startswith("read:") for d in report["diffs"]), report)

    def test_the_command_line_exit_codes(self):
        sa, sb = mock_daemon(HEALTH, answers()), mock_daemon(HEALTH, answers(shot=(640, 360)))
        self.addCleanup(sa.shutdown)
        self.addCleanup(sb.shutdown)
        url = lambda s: f"http://127.0.0.1:{s.server_address[1]}"
        same = ["--a", url(sa), "--a-token", "tok", "--b", url(sa), "--b-token", "tok"]
        diff = ["--a", url(sa), "--a-token", "tok", "--b", url(sb), "--b-token", "tok"]
        self.assertEqual(compare.main(same), 0)
        self.assertEqual(compare.main(diff), 1)
        dead = ["--a", url(sa), "--a-token", "tok", "--b", "http://127.0.0.1:9", "--b-token", "tok"]
        self.assertEqual(compare.main(dead), 2)


if __name__ == "__main__":
    unittest.main()
