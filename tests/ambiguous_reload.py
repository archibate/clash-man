"""An applied request with a lost response must not become offline success."""

import http.server
import json
import os
import socket
import subprocess
import tempfile
import threading
from pathlib import Path

reloads = []


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        body = (
            b"proxies: [{name: new, type: ss}]\nproxy-groups: []\n"
            if self.path == "/sub"
            else b'{"proxies":{}}'
        )
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_PUT(self):
        reloads.append(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        if len(reloads) == 1:
            self.close_connection = True
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
        else:
            self.send_response(204)
            self.end_headers()


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with tempfile.TemporaryDirectory(prefix="clash-ambiguous-") as directory:
        root = Path(directory)
        config = root / "config.yaml"
        original = f"external-controller: 127.0.0.1:{server.server_port}\nproxies: [{{name: old, type: ss}}]\nproxy-groups: []\n"
        config.write_text(original)
        (root / "sublink.txt").write_text(f"http://127.0.0.1:{server.server_port}/sub")
        environment = dict(os.environ, XDG_STATE_HOME=str(root / "state"))
        environment.pop("CLASH_MAN_CONTROLLER", None)
        environment.pop("CLASH_MAN_SECRET", None)
        result = subprocess.run(
            [
                os.environ.get("CLASH_MAN_BIN", "target/debug/clash-man"),
                "--config",
                str(config),
                "update",
                "--force",
            ],
            env=environment,
            capture_output=True,
            timeout=15,
            check=False,
        )
        state = root / "config.yaml.clash-man"
        assert result.returncode != 0, result.stdout
        assert len(reloads) == 2
        assert config.read_text() == original
        assert not (state / "pending.json").exists()
        assert (
            json.loads((state / "subscription.json").read_text())["last_success"]
            is None
        )
        print(
            "PASS: lost reload response fails and restores prior config/core; no false success"
        )
finally:
    server.shutdown()
    server.server_close()
