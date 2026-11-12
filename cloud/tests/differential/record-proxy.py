#!/usr/bin/env python3
"""A recording reverse proxy for capturing what a stock client sends.

    record-proxy.py --listen-port N --backend-port N --out DIR

Every request is passed to 127.0.0.1:--backend-port and the reply returned
unchanged. Each exchange is written to DIR as NNNN.request (the request line,
the headers a client sets, a blank line, the body), NNNN.reply (status line,
Content-Type, blank line, the body) and an entry in DIR/exchanges.jsonl.
Stops on SIGTERM or SIGINT.
"""
import argparse
import http.client
import json
import os
import signal
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ap = argparse.ArgumentParser()
ap.add_argument("--listen-port", type=int, required=True)
ap.add_argument("--backend-port", type=int, required=True)
ap.add_argument("--out", required=True)
args = ap.parse_args()
os.makedirs(args.out, exist_ok=True)
lock = threading.Lock()
counter = [0]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def forward(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        conn = http.client.HTTPConnection("127.0.0.1", args.backend_port, timeout=300)
        headers = {k: v for k, v in self.headers.items()
                   if k.lower() not in ("host", "connection", "expect", "accept-encoding")}
        headers["Host"] = "127.0.0.1:%d" % args.listen_port
        conn.request(self.command, self.path, body=body, headers=headers)
        resp = conn.getresponse()
        data = resp.read()
        with lock:
            counter[0] += 1
            i = counter[0]
        with open(os.path.join(args.out, "%04d.request" % i), "wb") as f:
            f.write(("%s %s\n" % (self.command, self.path)).encode())
            for k in ("Content-Type", "Content-Length", "User-Agent"):
                if self.headers.get(k):
                    f.write(("%s: %s\n" % (k, self.headers[k])).encode())
            f.write(b"\n" + body)
        with open(os.path.join(args.out, "%04d.reply" % i), "wb") as f:
            f.write(("%d\nContent-Type: %s\n\n" % (resp.status, resp.getheader("Content-Type"))).encode())
            f.write(data)
        with lock, open(os.path.join(args.out, "exchanges.jsonl"), "a") as f:
            f.write(json.dumps({"n": i, "method": self.command, "path": self.path,
                                "status": resp.status, "request_bytes": len(body),
                                "reply_bytes": len(data)}) + "\n")
        self.send_response(resp.status)
        for k in ("Content-Type",):
            if resp.getheader(k):
                self.send_header(k, resp.getheader(k))
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    do_GET = do_POST = do_HEAD = forward


server = ThreadingHTTPServer(("127.0.0.1", args.listen_port), Handler)
server.daemon_threads = True


def stop(*_):
    threading.Thread(target=server.shutdown).start()


signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
server.serve_forever()
