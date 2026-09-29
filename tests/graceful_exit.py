import fcntl
import http.server
import json
import os
import pty
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time
from pathlib import Path

reload_entered = threading.Event()
release = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path == "/sub":
            body = b"proxies: [{name: b, type: ss}]\nproxy-groups: []\n"
        else:
            body = json.dumps(
                {
                    "/proxies": {"proxies": {}},
                    "/configs": {},
                    "/version": {"version": "audit"},
                    "/connections": {"connections": []},
                    "/rules": {"rules": []},
                }.get(self.path, {})
            ).encode()
        if self.path.split("?")[0] in ["/traffic", "/memory", "/logs"]:
            self.send_response(200)
            self.end_headers()
            time.sleep(8)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_PUT(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        reload_entered.set()
        release.wait(10)
        self.send_response(204)
        self.end_headers()


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix="clash-audit-cancel-") as d:
    root = Path(d)
    cfg = root / "config.yaml"
    original = f"external-controller: 127.0.0.1:{server.server_port}\nproxies: [{{name: a, type: ss}}]\nproxy-groups: []\n"
    cfg.write_text(original)
    (root / "sublink.txt").write_text(f"http://127.0.0.1:{server.server_port}/sub")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    env = os.environ.copy()
    env["XDG_STATE_HOME"] = str(root / "state")
    env["TERM"] = "xterm-256color"
    env.pop("CLASH_MAN_CONTROLLER", None)
    env.pop("CLASH_MAN_SECRET", None)
    p = subprocess.Popen(
        [
            os.environ.get("CLASH_MAN_BIN", "target/debug/clash-man"),
            "--config",
            str(cfg),
        ],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
    )
    os.close(slave)
    end = time.monotonic() + 10
    while not reload_entered.is_set() and time.monotonic() < end:
        if select.select([master], [], [], 0.1)[0]:
            os.read(master, 65536)
    assert reload_entered.is_set(), "no reload"
    os.write(master, b"q")
    time.sleep(0.3)
    assert p.poll() is None, "exit cancelled an in-flight update"
    release.set()
    p.wait(timeout=10)
    assert p.returncode == 0
    assert not (root / "config.yaml.clash-man/pending.json").exists()
    print(
        json.dumps(
            {
                "exit": p.returncode,
                "pending_after_normal_q": (
                    root / "config.yaml.clash-man/pending.json"
                ).exists(),
                "config_changed": cfg.read_text() != original,
            }
        )
    )
    os.close(master)
