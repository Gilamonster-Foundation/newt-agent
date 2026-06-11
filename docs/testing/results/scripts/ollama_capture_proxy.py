#!/usr/bin/env python3
"""BASELINE SCRATCH HARNESS — capture/forward proxy for Ollama (issue #245).

Not wired into CI or the workspace build. Used to capture the EXACT request
bodies newt sends to /api/chat (and the backend's reported token counts) during
scripted live sessions, for the B3/B5/B6 baseline measurements in
docs/testing/context-memory-benchmark.md.

Usage:
    python3 ollama_capture_proxy.py --listen 18434 \
        --upstream https://gnuc-ollama.home.lab --log /tmp/newt-bench/capture.jsonl

Then point newt at it:  NEWT_DGX_OLLAMA_URL=http://127.0.0.1:18434

Each proxied request appends one JSONL record:
  {ts, method, path, status, request: <parsed JSON body or null>,
   response: <parsed JSON for non-streaming>,
   stream_final: <final NDJSON object for streaming responses>,
   stream_content_len: <chars of concatenated streamed message.content>}

Streaming responses are buffered upstream-side and relayed whole; that only
delays newt's token-by-token display, it does not change any measured number.
TLS verification is disabled (home-lab CA).
"""

import argparse
import json
import ssl
import sys
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from datetime import datetime, timezone
from threading import Lock

LOG_LOCK = Lock()


def log_record(path, record):
    with LOG_LOCK:
        with open(path, "a", encoding="utf-8") as f:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")


class ProxyHandler(BaseHTTPRequestHandler):
    upstream = None
    log_path = None
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quiet stderr
        sys.stderr.write("[proxy] %s\n" % (fmt % args))

    def _forward(self, body: bytes | None):
        url = self.upstream.rstrip("/") + self.path
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        req = urllib.request.Request(url, data=body, method=self.command)
        for h in ("Content-Type", "Accept", "Authorization"):
            v = self.headers.get(h)
            if v:
                req.add_header(h, v)
        try:
            with urllib.request.urlopen(req, context=ctx, timeout=600) as resp:
                return resp.status, dict(resp.headers), resp.read()
        except urllib.error.HTTPError as e:
            return e.code, dict(e.headers), e.read()

    def _handle(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        status, headers, resp_body = self._forward(body)

        record = {
            "ts": datetime.now(timezone.utc).isoformat(),
            "method": self.command,
            "path": self.path,
            "status": status,
        }
        try:
            record["request"] = json.loads(body) if body else None
        except (json.JSONDecodeError, UnicodeDecodeError):
            record["request"] = {"_unparsed_bytes": len(body or b"")}

        streaming = bool(record.get("request")) and record["request"].get("stream") is True
        if streaming:
            # NDJSON: log the final object (carries prompt_eval_count/eval_count)
            # and the total streamed content length.
            final = None
            content_len = 0
            for line in resp_body.decode("utf-8", "replace").splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError:
                    continue
                content_len += len(obj.get("message", {}).get("content", "") or "")
                final = obj
            record["stream_final"] = final
            record["stream_content_len"] = content_len
        else:
            try:
                record["response"] = json.loads(resp_body)
            except (json.JSONDecodeError, UnicodeDecodeError):
                record["response"] = {"_raw": resp_body.decode("utf-8", "replace")[:2000]}

        log_record(self.log_path, record)

        self.send_response(status)
        self.send_header("Content-Type", headers.get("Content-Type", "application/json"))
        self.send_header("Content-Length", str(len(resp_body)))
        self.end_headers()
        self.wfile.write(resp_body)

    def do_GET(self):
        self._handle()

    def do_POST(self):
        self._handle()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=int, default=18434)
    ap.add_argument("--upstream", default="https://gnuc-ollama.home.lab")
    ap.add_argument("--log", required=True)
    args = ap.parse_args()

    ProxyHandler.upstream = args.upstream
    ProxyHandler.log_path = args.log
    srv = ThreadingHTTPServer(("127.0.0.1", args.listen), ProxyHandler)
    print(f"capture proxy on 127.0.0.1:{args.listen} -> {args.upstream}, log={args.log}")
    srv.serve_forever()


if __name__ == "__main__":
    main()
