import shutil
import socket
import ssl
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from benchlib.config import Probe
from benchlib.probes import http_call, run_probe, tls_info, wait_healthy


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/boom":
            self.send_response(503)
            self.end_headers()
            return
        body = b"a" * 1024
        self.send_response(401 if self.path == "/deny" else 200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Bench-Remove", "1")
        self.send_header("X-Bench-Echo-Key", self.headers.get("apikey", ""))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class ProbeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.port = cls.server.server_address[1]
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def probe(self, p, values=None):
        return run_probe(p, "http", "127.0.0.1", self.port, values or {})

    def test_passing_probe(self):
        r = self.probe(Probe(path="/bench/1k", expect_headers={"x-bench-remove": "1"}, expect_body_bytes=1024))
        self.assertTrue(r.ok, r.detail)

    def test_status_mismatch(self):
        r = self.probe(Probe(path="/deny"))
        self.assertFalse(r.ok)
        self.assertIn("status 401", r.detail)

    def test_reject_status_list(self):
        self.assertTrue(self.probe(Probe(path="/deny", expect_status=(401, 403))).ok)

    def test_absent_header_and_body_size(self):
        r = self.probe(Probe(path="/x", expect_absent=("x-bench-remove",), expect_body_bytes=10))
        self.assertFalse(r.ok)
        self.assertIn("x-bench-remove present", r.detail)
        self.assertIn("body 1024 bytes", r.detail)

    def test_header_tokens_rendered(self):
        r = self.probe(Probe(path="/x", headers={"apikey": "@@API_KEY@@"},
                             expect_headers={"x-bench-echo-key": "k-123"}), {"API_KEY": "k-123"})
        self.assertTrue(r.ok, r.detail)

    def test_refused_connection_fails_cleanly(self):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            free = s.getsockname()[1]
        r = run_probe(Probe(path="/"), "http", "127.0.0.1", free, {})
        self.assertFalse(r.ok)
        self.assertIn("Error", r.detail)

    def test_wait_healthy(self):
        self.assertTrue(wait_healthy("http", "127.0.0.1", self.port, "/", 2))
        t = iter(range(100))
        self.assertFalse(wait_healthy("http", "127.0.0.1", self.port, "/boom", 3,
                                      sleep=lambda s: None, clock=lambda: next(t)))


@unittest.skipUnless(shutil.which("openssl"), "openssl not on PATH")
class TlsProbeTests(unittest.TestCase):
    def test_tls13_and_alpn(self):
        with tempfile.TemporaryDirectory() as d:
            cert, key = Path(d, "c.pem"), Path(d, "k.pem")
            subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt",
                            "ec_paramgen_curve:prime256v1", "-nodes", "-keyout", str(key), "-out", str(cert),
                            "-days", "1", "-subj", "/CN=bench.local"], check=True, capture_output=True)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.minimum_version = ssl.TLSVersion.TLSv1_3
            ctx.load_cert_chain(cert, key)
            ctx.set_alpn_protocols(["h2", "http/1.1"])
            srv = socket.socket()
            srv.bind(("127.0.0.1", 0))
            srv.listen(4)
            port = srv.getsockname()[1]

            def serve():
                for _ in range(2):
                    conn, _ = srv.accept()
                    try:
                        with ctx.wrap_socket(conn, server_side=True):
                            pass
                    except (ssl.SSLError, OSError):
                        pass

            threading.Thread(target=serve, daemon=True).start()
            self.assertEqual(tls_info("127.0.0.1", port)[0], "TLSv1.3")
            self.assertEqual(tls_info("127.0.0.1", port, alpn=["h2", "http/1.1"])[1], "h2")
            srv.close()

    def test_probes_send_the_given_tls_server_name(self):
        # The load generator sends the load URL's host as SNI; probes must send the same
        # name, or a gateway that only serves the probe's name validates and then fails
        # every TLS measurement (APISIX with snis: [bench.local], found in a real run).
        with tempfile.TemporaryDirectory() as d:
            cert, key = Path(d, "c.pem"), Path(d, "k.pem")
            subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt",
                            "ec_paramgen_curve:prime256v1", "-nodes", "-keyout", str(key), "-out", str(cert),
                            "-days", "1", "-subj", "/CN=gateway"], check=True, capture_output=True)
            seen = []
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(cert, key)
            ctx.sni_callback = lambda sock, name, c: seen.append(name)
            srv = socket.socket()
            srv.bind(("127.0.0.1", 0))
            srv.listen(4)
            port = srv.getsockname()[1]

            def serve():
                for _ in range(2):
                    conn, _ = srv.accept()
                    try:
                        with ctx.wrap_socket(conn, server_side=True) as s:
                            s.recv(4096)
                            s.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    except (ssl.SSLError, OSError):
                        pass

            threading.Thread(target=serve, daemon=True).start()
            tls_info("127.0.0.1", port, sni="gateway")
            status, _, _ = http_call("GET", "https", "127.0.0.1", port, "/", sni="gateway")
            srv.close()
            self.assertEqual(status, 200)
            self.assertEqual(seen, ["gateway", "gateway"])
