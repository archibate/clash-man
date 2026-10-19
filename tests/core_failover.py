"""Optional real-core integration test. Uses loopback fixtures, never a live subscription.

CLASH_BIN=clash CLASH_MAN_BIN=target/debug/clash-man uv run --with pyyaml tests/core_failover.py
"""

import http.server
import json
import os
import pathlib
import select
import socket
import socketserver
import struct
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

import yaml


class Origin(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/sub":
            body = self.server.subscription.encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(204)
        self.send_header("Connection", "close")
        self.end_headers()

    do_HEAD = do_GET

    def log_message(self, *args):
        pass


class DnsAnswer(socketserver.BaseRequestHandler):
    def handle(self):
        data, connection = self.request
        end = 12
        while data[end]:
            end += data[end] + 1
        end += 5  # terminating label, question type and class
        is_a = data[end - 4 : end - 2] == b"\x00\x01"
        answer = data[:2] + struct.pack("!5H", 0x8180, 1, int(is_a), 0, 0)
        answer += data[12:end]
        if is_a:
            answer += b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 1, 4)
            answer += socket.inet_aton(self.server.answer)
        connection.sendto(answer, self.client_address)


def dns_server(answer):
    server = socketserver.UDPServer(("127.0.0.1", 0), DnsAnswer)
    server.answer = answer
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class Tunnel(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(5)
        try:
            header = b""
            while not header.endswith(b"\r\n\r\n") and len(header) < 8192:
                part = self.request.recv(1)
                if not part:
                    return
                header += part
            if not header.startswith(b"CONNECT "):
                return
            with socket.create_connection(self.server.origin, timeout=5) as upstream:
                self.request.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
                while not self.server.failed.is_set():
                    ready, _, _ = select.select([self.request, upstream], [], [], 0.1)
                    for source in ready:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is self.request else self.request).sendall(
                            data
                        )
        except OSError:
            pass


class Proxy(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, origin, listen_port=0):
        self.origin = origin
        self.failed = threading.Event()
        super().__init__(("127.0.0.1", listen_port), Tunnel)
        threading.Thread(target=self.serve_forever, daemon=True).start()

    def fail(self):
        self.failed.set()
        self.shutdown()
        self.server_close()


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def eventually(check, seconds=20):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            if result := check():
                return result
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.2)
    raise AssertionError("condition did not become true before deadline")


def main():
    core = os.environ.get("CLASH_BIN", "clash")
    manager = str(
        pathlib.Path(
            os.environ.get("CLASH_MAN_BIN", "target/debug/clash-man")
        ).resolve()
    )
    origin = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    threading.Thread(target=origin.serve_forever, daemon=True).start()
    primary, backup, relay = (Proxy(origin.server_address) for _ in range(3))
    old_dns, new_dns = (dns_server(ip) for ip in ["203.0.113.10", "203.0.113.20"])
    controller_port, proxy_port = port(), port()
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    health_url = f"http://127.0.0.1:{origin.server_port}/check"

    def api(path, data=None):
        request = urllib.request.Request(
            f"http://127.0.0.1:{controller_port}{path}",
            data=None if data is None else json.dumps(data).encode(),
            headers={
                "Authorization": "Bearer integration-test",
                "Content-Type": "application/json",
            },
            method="GET" if data is None else "PUT",
        )
        with opener.open(request, timeout=3) as response:
            raw = response.read()
            return json.loads(raw) if raw else response.status

    with tempfile.TemporaryDirectory(prefix="clash-man-core-") as temporary:
        directory = pathlib.Path(temporary)
        config = directory / "config.yaml"
        config.write_text(
            yaml.safe_dump(
                {
                    "port": proxy_port,
                    "allow-lan": False,
                    "external-controller": f"127.0.0.1:{controller_port}",
                    "secret": "integration-test",
                    "mode": "rule",
                    "log-level": "silent",
                    "dns": {
                        "enable": True,
                        "enhanced-mode": "redir-host",
                        "nameserver": [f"127.0.0.1:{old_dns.server_address[1]}"],
                        "fallback": [f"127.0.0.1:{old_dns.server_address[1]}"],
                        "fallback-filter": {"geoip": False, "ipcidr": ["0.0.0.0/32"]},
                    },
                    "proxies": [
                        {
                            "name": "pri`mary",
                            "type": "http",
                            "server": "127.0.0.1",
                            "port": primary.server_address[1],
                        },
                        {
                            "name": "backup",
                            "type": "http",
                            "server": "127.0.0.1",
                            "port": backup.server_address[1],
                        },
                    ],
                    "proxy-groups": [
                        {
                            "name": "Regional first",
                            "type": "fallback",
                            "proxies": ["backup"],
                            "url": health_url + "?regional=1",
                            "interval": 3600,
                            "expected-status": "200",
                        },
                        {
                            "name": "Proxy",
                            "type": "select",
                            "proxies": ["pri`mary", "backup"],
                        },
                        {
                            "name": "Backup only",
                            "type": "select",
                            "proxies": ["backup"],
                        },
                    ],
                    "rules": ["MATCH,Proxy"],
                }
            )
        )
        environment = dict(os.environ, XDG_STATE_HOME=str(directory / "state"))
        process = subprocess.Popen(
            [core, "-d", str(directory)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            eventually(lambda: api("/version"))
            saved = config.read_bytes()
            subscription = yaml.safe_load(saved)
            journal = directory / "config.yaml.clash-man/pending.json"
            journal.parent.mkdir(parents=True)
            journal.write_text(
                json.dumps(
                    {
                        "config_path": str(config),
                        "files": [[str(config), saved.decode()]],
                        "selected": [],
                        "reload": True,
                    }
                )
            )
            other = directory / "other.yaml"
            other.write_bytes(saved)
            wrong = subprocess.run(
                [manager, "--config", str(other), "recover"],
                env=environment,
                capture_output=True,
                check=False,
            )
            assert wrong.returncode == 0 and journal.exists()
            config.write_text("partial generation")
            subprocess.run(
                [manager, "--config", str(config), "recover"],
                env=environment,
                check=True,
            )
            assert config.read_bytes() == saved and not journal.exists()
            print(
                "PASS: CLI recovers invalid config and does not consume another config's journal",
                flush=True,
            )
            command = [manager, "--config", str(config), "auto"]
            subprocess.run(
                command
                + [
                    "--prefer",
                    "pri`mary",
                    "--interval",
                    "5s",
                    "--test-url",
                    health_url,
                ],
                env=environment,
                check=True,
            )
            group_path = "/proxies/" + urllib.parse.quote("Proxy Auto", safe="")
            assert api("/proxies/Proxy")["all"] == ["Proxy Auto"]
            assert api("/proxies/" + urllib.parse.quote("Backup only", safe=""))[
                "all"
            ] == ["backup"]

            def histories():
                proxies = api("/providers/proxies/clash-man-nodes")["proxies"]
                # Legacy Meta's global alive flag describes a different health
                # store. The automatic group consumes URL-specific results.
                histories = [
                    node.get("extra", {}).get(health_url, []) for node in proxies
                ]
                return (
                    histories
                    if all(
                        history and history[-1]["delay"] > 0 for history in histories
                    )
                    else None
                )

            first = eventually(histories)
            previous = first[0][-1]["time"]
            eventually(
                lambda: (nodes := histories()) and nodes[0][-1]["time"] != previous,
                seconds=12,
            )
            print("PASS: health checks advance with no business traffic", flush=True)
            assert api(group_path)["now"] == "pri`mary"
            try:
                api("/proxies/Proxy", {"name": "backup"})
                raise AssertionError("raw node selection unexpectedly accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 400
            print("PASS: legacy manual switch cannot replace Auto", flush=True)

            def dns_answer(name):
                query = urllib.parse.urlencode({"name": name, "type": "A"})
                return [x["data"] for x in api("/dns/query?" + query)["Answer"]]

            assert dns_answer("before.invalid") == ["203.0.113.10"]
            subscription["dns"] = {
                "nameserver": [f"127.0.0.1:{new_dns.server_address[1]}"],
                "listen": "0.0.0.0:53",
                "enable": False,
            }
            origin.subscription = yaml.safe_dump(subscription)
            (directory / "sublink.txt").write_text(
                f"http://127.0.0.1:{origin.server_port}/sub\n"
            )
            update = [manager, "--config", str(config), "update", "--force"]
            subprocess.run(update, env=environment, check=True)
            live_dns = yaml.safe_load(config.read_text())["dns"]
            assert live_dns["enable"] and "listen" not in live_dns
            assert "fallback" not in live_dns and "fallback-filter" not in live_dns
            assert dns_answer("after.invalid") == ["203.0.113.20"]
            print(
                "PASS: subscription replaces real DNS answers without exposing a DNS listener",
                flush=True,
            )

            healthy_config = config.read_bytes()
            # Native per-node checks remain healthy, but the actual proxy rule rejects
            # all traffic. The update must roll back both routing and DNS at runtime.
            subscription["rules"] = ["MATCH,REJECT"]
            subscription["dns"]["nameserver"] = [
                f"127.0.0.1:{old_dns.server_address[1]}"
            ]
            origin.subscription = yaml.safe_dump(subscription)
            failed = subprocess.run(
                update, env=environment, capture_output=True, text=True, check=False
            )
            assert failed.returncode != 0 and "traffic verification" in failed.stderr, (
                failed.stderr
            )
            assert config.read_bytes() == healthy_config and not journal.exists()
            assert dns_answer("rollback.invalid") == ["203.0.113.20"]
            print(
                "PASS: healthy nodes cannot hide broken proxy routing; failed update restores live DNS and rules",
                flush=True,
            )
            primary_delay = (
                "/proxies/"
                + urllib.parse.quote("pri`mary", safe="")
                + "/delay?"
                + urllib.parse.urlencode({"timeout": 3000, "url": health_url})
            )
            api(primary_delay)
            primary_port = primary.server_address[1]
            primary.fail()
            started = time.monotonic()
            eventually(lambda: api(group_path)["now"] == "backup", seconds=20)
            print(
                "PASS: failed primary switched to backup in %.2fs"
                % (time.monotonic() - started),
                flush=True,
            )
            request = subprocess.run(
                [
                    "curl",
                    "--noproxy",
                    "",
                    "--proxy",
                    f"http://127.0.0.1:{proxy_port}",
                    "-sS",
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{http_code}",
                    "--max-time",
                    "5",
                    health_url,
                ],
                capture_output=True,
                text=True,
                check=True,
            )
            assert request.stdout == "204"
            try:
                api(primary_delay)
                raise AssertionError("failed primary unexpectedly passed manual test")
            except urllib.error.HTTPError as error:
                assert error.code in (503, 504)
            primary = Proxy(origin.server_address, primary_port)
            eventually(lambda: api(group_path)["now"] == "pri`mary", seconds=20)
            print(
                "PASS: periodic health replaces stale successful and failed manual tests",
                flush=True,
            )
            assert "clash-man-direct-fallback" not in api(group_path)["all"]
            subprocess.run(
                command
                + [
                    "--interval",
                    "5s",
                    "--test-url",
                    health_url,
                    "--direct-fallback",
                    "--http-fallback",
                    f"127.0.0.1:{relay.server_address[1]}",
                ],
                env=environment,
                check=True,
            )
            assert api(group_path)["all"] == [
                "pri`mary",
                "backup",
                "clash-man-http-fallback",
                "clash-man-direct-fallback",
            ]
            eventually(lambda: api(group_path)["now"] == "pri`mary")
            primary.fail()
            backup.fail()
            eventually(lambda: api(group_path)["now"] == "clash-man-http-fallback")
            relay.fail()
            eventually(lambda: api(group_path)["now"] == "clash-man-direct-fallback")
            request = subprocess.run(
                [
                    "curl",
                    "--noproxy",
                    "",
                    "--proxy",
                    f"http://127.0.0.1:{proxy_port}",
                    "-sS",
                    "-o",
                    "/dev/null",
                    "-w",
                    "%{http_code}",
                    "--max-time",
                    "5",
                    health_url,
                ],
                capture_output=True,
                text=True,
                check=True,
            )
            assert request.stdout == "204"
            primary = Proxy(origin.server_address, primary_port)
            eventually(lambda: api(group_path)["now"] == "pri`mary")
            print(
                "PASS: failed nodes use relay, failed relay uses direct, recovered proxy takes priority",
                flush=True,
            )
            subprocess.run(command + ["--disable"], env=environment, check=True)
            assert api("/proxies/Proxy")["all"] == ["pri`mary", "backup"]
            print(
                "PASS: successful request after failover; disabling restores original groups",
                flush=True,
            )
        finally:
            primary.fail()
            backup.fail()
            relay.fail()
            origin.shutdown()
            origin.server_close()
            for resolver in [old_dns, new_dns]:
                resolver.shutdown()
                resolver.server_close()
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
