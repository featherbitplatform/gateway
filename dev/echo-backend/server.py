"""Minimal echo server that returns request headers as JSON."""
import json
import os
from http.server import HTTPServer, BaseHTTPRequestHandler


class EchoHandler(BaseHTTPRequestHandler):
    def _respond(self):
        headers = {k: v for k, v in self.headers.items()}
        body = None
        content_length = int(self.headers.get("Content-Length", 0))
        if content_length > 0:
            body = self.rfile.read(content_length).decode("utf-8", errors="replace")

        response = {
            "method": self.command,
            "path": self.path,
            "headers": headers,
        }
        if body is not None:
            response["body"] = body

        payload = json.dumps(response, indent=2).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        self._respond()

    def do_POST(self):
        self._respond()

    def do_PUT(self):
        self._respond()

    def do_DELETE(self):
        self._respond()

    def do_PATCH(self):
        self._respond()

    def log_message(self, format, *args):
        print(f"[echo-backend] {args[0]}")


if __name__ == "__main__":
    # PORT lets the e2e suite run its own backend on a spare port without
    # colliding with a dev/compose backend already holding 3000.
    port = int(os.environ.get("PORT", "3000"))
    server = HTTPServer(("0.0.0.0", port), EchoHandler)
    print(f"[echo-backend] Listening on :{port}")
    server.serve_forever()
