import base64
import hashlib
import hmac
import json
import tempfile
import unittest
from pathlib import Path

from benchlib.templating import TemplateError, expand_repeats, render_dir, render_text
from benchlib.tokens import JWT_SECRET, jwks_json, make_jwt, static_values, tamper


def b64url_decode(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


class RepeatTests(unittest.TestCase):
    def test_repeat_pads_index(self):
        out = expand_repeats("a\n@@REPEAT 12@@\n- r@@I@@\n@@END@@\nb\n")
        lines = out.splitlines()
        self.assertEqual(lines[0], "a")
        self.assertEqual(lines[1], "- r00")
        self.assertEqual(lines[12], "- r11")
        self.assertEqual(lines[13], "b")

    def test_repeat_1000_uses_three_digits(self):
        out = expand_repeats("@@REPEAT 1000@@\nr@@I@@\n@@END@@\n").splitlines()
        self.assertEqual((out[0], out[-1], len(out)), ("r000", "r999", 1000))

    def test_separator(self):
        out = expand_repeats("[\n@@REPEAT 3 SEP ,@@\n  {\"i\": \"@@I@@\"}\n@@END@@\n]\n")
        self.assertEqual(json.loads(out), [{"i": "0"}, {"i": "1"}, {"i": "2"}])

    def test_errors(self):
        for bad in ("@@REPEAT 2@@\nx\n", "x\n@@END@@\n", "@@REPEAT 2@@\n@@REPEAT 2@@\n@@END@@\n@@END@@\n"):
            with self.assertRaises(TemplateError):
                expand_repeats(bad)


class RenderTests(unittest.TestCase):
    def test_inline_tokens(self):
        self.assertEqual(render_text("host: @@H@@:@@P@@\n", {"H": "up", "P": "8080"}), "host: up:8080\n")

    def test_block_token_is_indented(self):
        out = render_text("cert: |\n      @@PEM@@\nnext: 1\n", {"PEM": "-----BEGIN-----\nAAA\n-----END-----\n"})
        self.assertEqual(out, "cert: |\n      -----BEGIN-----\n      AAA\n      -----END-----\nnext: 1\n")

    def test_multiline_token_inline_is_an_error(self):
        with self.assertRaisesRegex(TemplateError, "alone on its line"):
            render_text("x: @@PEM@@ tail\n", {"PEM": "a\nb"})

    def test_unknown_token(self):
        with self.assertRaisesRegex(TemplateError, "@@NOPE@@"):
            render_text("@@NOPE@@\n", {})

    def test_non_token_at_signs_pass_through(self):
        self.assertEqual(render_text("user@@host and {{error.code}}\n", {}), "user@@host and {{error.code}}\n")

    def test_render_dir_each_and_binary(self):
        with tempfile.TemporaryDirectory() as d:
            src, dst = Path(d, "src"), Path(d, "dst")
            (src / "apps").mkdir(parents=True)
            (src / "apps" / "r@@EACH_3@@.json").write_text('{"id": "r@@I@@", "up": "@@H@@"}', "utf-8")
            (src / "blob.bin").write_bytes(b"\xff\xfe\x00")
            render_dir(src, dst, {"H": "upstream"})
            self.assertEqual(json.loads((dst / "apps" / "r2.json").read_text("utf-8")),
                             {"id": "r2", "up": "upstream"})
            self.assertEqual(sorted(p.name for p in (dst / "apps").iterdir()), ["r0.json", "r1.json", "r2.json"])
            self.assertEqual((dst / "blob.bin").read_bytes(), b"\xff\xfe\x00")

    def test_render_dir_normalizes_crlf(self):
        with tempfile.TemporaryDirectory() as d:
            src, dst = Path(d, "src"), Path(d, "dst")
            src.mkdir()
            (src / "nginx.conf").write_bytes(b"a @@H@@;\r\nb;\r\n")
            render_dir(src, dst, {"H": "x"})
            self.assertEqual((dst / "nginx.conf").read_bytes(), b"a x;\nb;\n")

    def test_render_dir_names_failing_file(self):
        with tempfile.TemporaryDirectory() as d:
            src = Path(d, "src")
            src.mkdir()
            (src / "bad.yaml").write_text("@@MISSING@@\n", "utf-8")
            with self.assertRaisesRegex(TemplateError, "bad.yaml"):
                render_dir(src, Path(d, "dst"), {})


class TokenTests(unittest.TestCase):
    def test_jwt_verifies(self):
        tok = make_jwt()
        head, payload, sig = tok.split(".")
        expected = hmac.new(JWT_SECRET.encode(), f"{head}.{payload}".encode(), hashlib.sha256).digest()
        self.assertEqual(b64url_decode(sig), expected)
        self.assertEqual(json.loads(b64url_decode(head))["kid"], "bench")
        claims = json.loads(b64url_decode(payload))
        self.assertEqual((claims["iss"], claims["key"], claims["sub"]), ("bench-jwt", "bench-jwt", "bench"))

    def test_tamper_breaks_signature(self):
        tok = make_jwt()
        bad = tamper(tok)
        self.assertNotEqual(bad, tok)
        self.assertEqual(bad.rsplit(".", 1)[0], tok.rsplit(".", 1)[0])
        self.assertNotEqual(b64url_decode(bad.rsplit(".", 1)[1]), b64url_decode(tok.rsplit(".", 1)[1]))

    def test_jwks_key_is_the_secret(self):
        key = json.loads(jwks_json())["keys"][0]
        self.assertEqual((key["kty"], key["alg"], key["kid"]), ("oct", "HS256", "bench"))
        self.assertEqual(b64url_decode(key["k"]).decode(), JWT_SECRET)

    def test_static_values_are_single_line(self):
        for k, v in static_values().items():
            self.assertNotIn("\n", v, k)
