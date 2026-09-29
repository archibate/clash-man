import http.server
import json
import os
import subprocess
import tempfile
import threading
import time
from pathlib import Path

seen = []
fetches = []
gate = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path == "/sub":
            fetches.append(1)
            body = f"proxies: [{{name: node{len(fetches)}, type: ss}}]\nproxy-groups: []\n".encode()
        else:
            body = b'{"proxies":{}}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_PUT(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        seen.append(1)
        gate.wait(10)
        self.send_response(204)
        self.end_headers()


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix="clash-audit-lock-") as d:
    root = Path(d)
    cfg = root / "config.yaml"
    cfg.write_text(
        f"external-controller: 127.0.0.1:{server.server_port}\nproxies: [{{name: old, type: ss}}]\nproxy-groups: []\n"
    )
    (root / "sublink.txt").write_text(f"http://127.0.0.1:{server.server_port}/sub")
    ps = []
    for i in range(2):
        env = os.environ.copy()
        env["XDG_STATE_HOME"] = str(root / f"state{i}")
        env.pop("CLASH_MAN_CONTROLLER", None)
        env.pop("CLASH_MAN_SECRET", None)
        ps.append(
            subprocess.Popen(
                [
                    os.environ.get("CLASH_MAN_BIN", "target/debug/clash-man"),
                    "--config",
                    str(cfg),
                    "update",
                    "--force",
                ],
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
        )
        deadline = time.monotonic() + 3
        while len(seen) <= i and time.monotonic() < deadline:
            time.sleep(0.02)
    assert len(seen) == 1, seen
    assert (root / "config.yaml.clash-man/pending.json").exists()
    print(
        json.dumps(
            {
                "concurrent_reload_requests": len(seen),
                "independent_pending_journals": int(
                    (root / "config.yaml.clash-man/pending.json").exists()
                ),
                "config": cfg.read_text().split("proxies:")[1].strip(),
            }
        )
    )
    gate.set()
    results = [p.communicate(timeout=5) for p in ps]
    assert ps[0].returncode == 0 and ps[1].returncode != 0
    assert not (root / "config.yaml.clash-man/pending.json").exists()
