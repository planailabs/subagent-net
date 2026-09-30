"""Run a script in the running Blender through the Blender MCP add-on
(localhost:9876, the same socket protocol blmcp uses).

    python3 live.py build.py [args...]   # non-option args are paths, made absolute
"""

import json
import socket
import sys
from pathlib import Path

path = Path(sys.argv[1]).resolve()
code = f"""
import sys, runpy
sys.argv = {[str(path)] + [a if a.startswith("-") else str(Path(a).resolve()) for a in sys.argv[2:]]!r}
runpy.run_path({str(path)!r}, run_name="__main__")
"""
req = json.dumps({"type": "execute", "code": code, "strict_json": False}) + "\0"
with socket.create_connection(("localhost", 9876), timeout=600) as s:
    s.sendall(req.encode())
    buf = bytearray()
    while b"\0" not in buf:
        chunk = s.recv(65536)
        if not chunk:
            break
        buf.extend(chunk)
resp = json.loads(buf.partition(b"\0")[0])
for k in ("stdout", "stderr"):
    if resp.get(k):
        print(resp[k], end="", file=sys.stdout if k == "stdout" else sys.stderr)
if resp.get("status") != "ok":
    print(resp.get("message"), file=sys.stderr)
    sys.exit(1)
