#!/usr/bin/env python3
"""Reads text on stdin and writes it with every run of 32 or more hex digits replaced by
`<hex:XXXXXX>`, where XXXXXX is the start of the SHA-256 of the run. Two runs that were
equal stay equal, and the value cannot be read from the output. Used on the replies that
carry account keys, so that the check can show that two servers gave the same key without
printing it."""
import hashlib
import re
import sys

print(re.sub(r"[0-9a-fA-F]{32,}",
             lambda m: "<hex:%s>" % hashlib.sha256(m.group(0).lower().encode()).hexdigest()[:6],
             sys.stdin.read()), end="")
