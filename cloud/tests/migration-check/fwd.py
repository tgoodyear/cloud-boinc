#!/usr/bin/env python3
"""Forwards a TCP port on 127.0.0.1 to a MySQL socket for a limited time.

The stock server of the check listens on a socket inside its container only. This runs
inside the container for the length of an import (import-rows.sh) so that the importer,
which connects by TCP, can reach it from the host.

    fwd.py --listen-port N --socket PATH --seconds N
"""
import argparse
import socket
import threading
import time


def pump(a, b):
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            b.sendall(data)
    except OSError:
        pass
    finally:
        for s in (a, b):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen-port", type=int, required=True)
    ap.add_argument("--socket", required=True)
    ap.add_argument("--seconds", type=int, required=True)
    args = ap.parse_args()
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", args.listen_port))
    srv.listen(8)
    srv.settimeout(1.0)
    end = time.time() + args.seconds
    while time.time() < end:
        try:
            c, _ = srv.accept()
        except socket.timeout:
            continue
        u = socket.socket(socket.AF_UNIX)
        u.connect(args.socket)
        threading.Thread(target=pump, args=(c, u), daemon=True).start()
        threading.Thread(target=pump, args=(u, c), daemon=True).start()


main()
