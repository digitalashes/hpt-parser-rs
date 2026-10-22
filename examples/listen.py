#!/usr/bin/env python3
"""Listens on a unix socket for the events of one `hpt-parser --notify-socket` run and
prints them: `python3 listen.py hpt.sock`, then the run with `--notify-socket hpt.sock`.
A chunk_ready file is complete when its event comes, the place to load, move or queue
it. Exits 0 on done, 1 on error or when the run ends without either.
"""

import json
import os
import socket
import sys

path = sys.argv[1] if len(sys.argv) > 1 else "hpt.sock"
if os.path.exists(path):  # left by an earlier run
    os.remove(path)
with socket.socket(socket.AF_UNIX) as server:
    server.bind(path)
    server.listen(1)
    conn, _ = server.accept()
os.remove(path)

with conn, conn.makefile(encoding="utf-8") as lines:
    for line in lines:
        event = json.loads(line)
        if event["event"] == "chunk_ready":
            print(f"chunk {event['index']}: {event['path']} ({event['records']} records)")
        elif event["event"] == "done":
            print(f"done: {event['chunks']} chunks, {event['records']} records")
            sys.exit(0)
        elif event["event"] == "error":
            sys.exit(f"error: {event['message']}")
sys.exit("the run ended without done or error")
