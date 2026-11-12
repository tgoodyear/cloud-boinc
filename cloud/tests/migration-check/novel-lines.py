#!/usr/bin/env python3
"""Lists the lines a client logged after the switch that it did not log before it.

    novel-lines.py --log client.log --switch-line N

Lines are compared without the time, with digits and task, file and project names made
alike, so that "Starting task c804_b_0" and "Starting task c804_c_0" are one line. The HTTP
debug lines are left out. The first line of each kind that is new is printed in full, with
how many there were. The log holds the account key (http_debug); the output has every run of
32 or more hex digits replaced.
"""
import argparse
import re
import sys

ap = argparse.ArgumentParser()
ap.add_argument("--log", required=True)
ap.add_argument("--switch-line", required=True, type=int, help="lines in the log at the switch")
args = ap.parse_args()

SKIP = re.compile(r"\[http\]|http_op|Throughput|\[file_xfer\]|^\s*<|^\s*$|^\d+ ?$")


def shape(line):
    text = re.sub(r"^\d\d-\w{3}-\d{4} \d\d:\d\d:\d\d ", "", line)
    text = re.sub(r"\[[^\]]*\]", lambda m: m.group(0) if m.group(0) in ("[---]", "[sched_op]", "[fxd]", "[unparsed_xml]") else "[P]", text)
    text = re.sub(r"\b\w+_(?:[abcd]|\d+)_\d+(?:_r\d+_\d+)?\b", "TASK", text)
    text = re.sub(r"\b[\w.]+\.(?:txt|xml)\b", "FILE", text)
    text = re.sub(r"upper_\S+", "APP", text)
    return re.sub(r"\d+", "N", text)


before, after = {}, {}
with open(args.log, errors="replace") as f:
    for i, line in enumerate(f, 1):
        line = line.rstrip("\n")
        if SKIP.search(line):
            continue
        (before if i <= args.switch_line else after).setdefault(shape(line), [line, 0])[1] += 1
for key, (example, n) in after.items():
    if key not in before:
        print("%4d x  %s" % (n, re.sub(r"[0-9a-f]{32,}", "<redacted>", example)))
