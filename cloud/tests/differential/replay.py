#!/usr/bin/env python3
"""Sends the requests of one corpus scenario to a stock server and stores the replies.

    replay.py --base URL --scenario DIR [--public-base URL] [--gap SECONDS]

DIR holds steps.json, a list of steps in the order they were captured:

    {"name": "03-sched", "kind": "sched", "request": "03-sched.request.xml"}
    {"name": "05-upload", "kind": "upload", "request": "05-upload.request"}
    {"name": "01-config", "kind": "web", "method": "GET", "url": "get_project_config.php"}

A sched step is a POST to the scheduler CGI, an upload step a POST to the
file upload handler, and a web step a request for a page of the project's web
directory. Each reply is written to DIR/<name>.reply, and the status, content
type and the time the request was sent to DIR/golden.json. A scheduler request
that follows another one is sent --gap seconds later, or the step's own "gap".
Every occurrence of --base in a reply becomes --public-base (default
http://project.test), so the golden files do not depend on the port the server
ran on.

Job names: requests name jobs in the order the host was sent them, and replay
writes the real names in (see names.py). The replies keep the real names.
"""
import argparse
import http.client
import json
import os
import time
import urllib.parse

from names import Names

ap = argparse.ArgumentParser()
ap.add_argument("--base", required=True)
ap.add_argument("--scenario", required=True)
ap.add_argument("--public-base", default="http://project.test")
ap.add_argument("--gap", type=float, default=7.0,
                help="seconds to wait before a scheduler request that follows another one, "
                     "unless the step has its own \"gap\"; the stock scheduler sends no work "
                     "to a host that asked less than min_sendwork_interval (6 s) ago")
args = ap.parse_args()
base = args.base.rstrip("/")
u = urllib.parse.urlparse(base)
steps = json.load(open(os.path.join(args.scenario, "steps.json")))
golden = []
names = Names()
last_sched = None
for s in steps:
    kind = s["kind"]
    if kind == "sched":
        # Wait until the gap since the previous scheduler request has passed.
        if last_sched is not None:
            time.sleep(max(0.0, last_sched + s.get("gap", args.gap) - time.time()))
    body = None
    if kind == "sched":
        method, path = "POST", "/diff_cgi/cgi"
    elif kind == "upload":
        method, path = "POST", "/diff_cgi/file_upload_handler"
    elif kind == "web":
        method, path = s.get("method", "GET"), "/diff/" + s["url"]
    else:
        raise SystemExit("unknown step kind %r" % kind)
    if "request" in s:
        body = open(os.path.join(args.scenario, s["request"]), "rb").read()
        if kind == "sched":
            # The corpus names jobs in the order they were sent; use the real names.
            body = names.to_actual(body.decode()).encode()
    headers = {"User-Agent": "BOINC client", "Accept": "*/*"}
    if body is not None:
        headers["Content-Type"] = s.get("content_type", "application/x-www-form-urlencoded")
    conn = http.client.HTTPConnection(u.hostname, u.port, timeout=120)
    sent = int(time.time())
    conn.request(method, path, body=body, headers=headers)
    r = conn.getresponse()
    data = r.read()
    data = data.replace(base.encode(), args.public_base.rstrip("/").encode())
    if kind == "sched":
        names.learn(data.decode())
    with open(os.path.join(args.scenario, s["name"] + ".reply"), "wb") as f:
        f.write(data)
    if kind == "sched":
        last_sched = time.time()
    golden.append({"name": s["name"], "status": r.status,
                   "content_type": r.getheader("Content-Type"), "time": sent})
    print("%s %s %d %d bytes" % (method, path, r.status, len(data)), flush=True)
with open(os.path.join(args.scenario, "golden.json"), "w") as f:
    json.dump(golden, f, indent=2)
    f.write("\n")
