#!/usr/bin/env python3
"""Minimal fixed-upstream DeepSeek gateway for an isolated SWE-bench solver."""

import http.client
import http.server
import os
import ssl


UPSTREAM_HOST = "api.deepseek.com"
API_KEY = os.environ["DEEPSEEK_API_KEY"]
HOP_BY_HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
}


class Gateway(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self._forward()

    def do_POST(self):
        self._forward()

    def _forward(self):
        content_length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(content_length) if content_length else None
        headers = {
            name: value
            for name, value in self.headers.items()
            if name.lower() not in HOP_BY_HOP_HEADERS
            and name.lower() not in {"host", "authorization"}
        }
        headers["Authorization"] = f"Bearer {API_KEY}"
        headers["Host"] = UPSTREAM_HOST

        upstream = http.client.HTTPSConnection(
            UPSTREAM_HOST,
            443,
            timeout=600,
            context=ssl.create_default_context(),
        )
        try:
            upstream.request(self.command, self.path, body=body, headers=headers)
            response = upstream.getresponse()
            self.send_response(response.status)
            for name, value in response.getheaders():
                if name.lower() not in HOP_BY_HOP_HEADERS and name.lower() != "content-length":
                    self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()
            while chunk := response.read(64 * 1024):
                self.wfile.write(chunk)
                self.wfile.flush()
        finally:
            upstream.close()
            self.close_connection = True

    def log_message(self, format, *args):
        # Method/path/status only. Request headers and bodies may contain secrets or prompts.
        print(f"provider_gateway client={self.client_address[0]} {format % args}", flush=True)


if __name__ == "__main__":
    http.server.ThreadingHTTPServer(("0.0.0.0", 8080), Gateway).serve_forever()
