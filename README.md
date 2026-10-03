# clash-man

A keyboard-first terminal dashboard for a Clash Meta or Mihomo core that also keeps its subscription
up to date.

![Rust](https://img.shields.io/badge/Rust-1.89%2B-orange)

## Features

- Opens on the proxy picker: groups in config order, cursor on the active node, latency tested
  automatically
- Always-visible header with mode, live speed, and subscription status and usage
- Connections sorted by recency or traffic, with a cursor that stays put while the list updates
- Searchable rules and a following, level-filtered log view
- Automatic controller discovery from Clash/Mihomo YAML
- Subscription auto-update with validation, rollback, and hot reload
- Persistent automatic failover with active native health checks, independent of the dashboard

## Install

```console
just install    # binary to ~/.local/bin, enables clash-man-update.timer
just uninstall
```

Discovery order:

1. `--controller` / `CLASH_MAN_CONTROLLER`
2. `--config` / `CLASH_MAN_CONFIG`
3. `~/.config/mihomo/config.yaml`
4. `~/.config/clash/config.yaml`
5. `http://127.0.0.1:9090`

The secret is loaded from `--secret-file`, `CLASH_MAN_SECRET`, or the discovered Clash YAML. A
plain `--secret` flag is deliberately omitted because command-line arguments are visible to other
processes.

```console
clash-man --controller http://127.0.0.1:9090
CLASH_MAN_SECRET=example clash-man
clash-man --config ~/.config/clash/config.yaml
```

## Subscription updates

`clash-man update` downloads the URL stored in `sublink.txt` beside the config (override with
`--sublink-file` / `CLASH_MAN_SUBLINK_FILE`) and:

1. rejects anything that is not a Clash YAML mapping with proxies or proxy-providers, so error pages
   and base64 node lists never replace a working config;
2. skips the write when only comment lines (the provider's timestamp) changed;
3. preserves host listeners, controller credentials, and the local automatic-routing policy;
4. saves the previous config to `config.yaml.clash-man/config.yaml.bak`, writes the new one
   atomically, and hot-reloads the core through `PUT /configs` — no service restart;
5. restores the previous files if the core rejects the new configuration.

It runs only when the last success is older than `--update-interval` (default `6h`,
`CLASH_MAN_UPDATE_INTERVAL`); failures retry after 10 minutes. `--force` skips the check. The
systemd timer invokes it hourly, and the dashboard checks every 5 minutes while open. Both share
`config.yaml.clash-man/subscription.json`, which also records the traffic and expiry the
provider reports in `subscription-userinfo`; the Overview page shows them.

## Automatic failover

```console
clash-man auto --prefer '美国 Premium 03 - 1倍率' --prefer '美国 02 - 1倍率' --interval 30s
clash-man auto --disable
```

`auto` manages `Proxy` by default (`--group` overrides it). Preferred nodes are tried first,
then the remaining candidates in subscription order. `Proxy` can select only `Proxy Auto`,
so an old script selecting a raw node cannot silently disable failover. The dashboard shows
the automatic group's current node and health measurements.

The core checks a shared file provider every 30 seconds, including while idle; the dashboard
does not need to stay open. `--test-url` defaults to `https://www.gstatic.com/generate_204`.
Groups register URL-specific checks with that provider; its default URL stays empty so old
Meta cores cannot let stale manual-test results override periodic health. Converted groups share
the policy URL, interval, and reachability criterion (any HTTP status), avoiding duplicate probes
and registration-order dependence. On those cores,
the node's global `alive` flag is not authoritative; use its URL-specific `extra` results.
Detection takes a check interval plus probe time; active connections are not transparently
retried, and an outage affecting all candidates still requires recovery upstream.
Use a representative destination with `--test-url` when availability differs by site.

`clash-man auto --direct-fallback` explicitly permits the host's own connection as the last
candidate when every proxy is unavailable. It exposes the host's IP and is off by default.
The direct candidate shares health checks; recovered proxies take priority again. This setting
persists through subscription updates. Run `auto` without the flag to remove it.

`--http-fallback 127.0.0.1:17890` adds a host-owned loopback HTTP proxy after subscription
nodes and before direct access. This can point to an SSH relay through another machine's
working proxy. Both optional fallbacks use the same periodic health checks.

For that relay, install `systemd/clash-man-relay-bridge.service` as a user service on the
receiving host (requires `socat`), and `systemd/clash-man-relay@.service` on the machine with
the working proxy. Set `CLASH_RELAY_SOCKET=/run/user/REMOTE_UID/clash-man-relay/upstream.sock`
in the latter machine's `~/.config/clash-man/relay-SSH_HOST.env`, then enable the bridge and
`clash-man-relay@SSH_HOST.service`. Use an existing SSH host alias with unattended key access.
The remote Unix socket is private and the bridge listens only on loopback, including on SSH
servers whose `GatewayPorts` setting would expose TCP reverse forwards. The relay depends on
the sending machine being online. Its unit reconnects automatically; concurrent bridge clients
are capped at 32, with bounded memory, descriptors, tasks and journal output.

Beside the canonical `config.yaml`, the manager creates a private `config.yaml.clash-man/`
directory containing `routing.json`, `source.yaml`, `nodes.yaml`, and an ignore-all `.gitignore`.
Locks, recovery journals and scheduling state live there too, independent of `XDG_STATE_HOME`.
Existing sidecar policies are read for migration; remove old credential-bearing sidecars only
after verifying migration. Recover pending transactions with the old binary before upgrading.
Subscription
updates regenerate the effective configuration without replacing the policy. Local and remote
hosts keep independent priorities and health results. Disabling restores the subscription's
groups. Currently, automatic mode requires an inline node list and a group of concrete nodes;
unsupported mixed provider/filter or directly node-targeted rule configurations are rejected.
Existing regional fallback groups retain their node order; preferences that would reorder
those groups are rejected. Omit `--prefer` to retain subscription ordering throughout.
Interrupted updates keep a durable undo journal. `clash-man recover` restores it, and the
systemd update service runs recovery automatically when its update process exits. The dashboard
waits for active updates before exiting. Lost reload responses are failures, not offline success.

Subscriptions can change routing sections only; listener, DNS, authentication and other host
settings remain local. Subscription/controller bodies are limited to 8 MiB, stream lines to
64 KiB, retained logs to 4 MiB/2,000 entries, and individual log payloads to 16 KiB.

Optional integration check with an installed core (isolated loopback fixtures):

```console
cargo build
CLASH_BIN=clash uv run --with pyyaml tests/core_failover.py
```

## Keys

The footer always lists the keys for what is focused; `?` shows all of them.

| Key | Action |
| --- | --- |
| `1`…`4`, `Tab` / `Shift-Tab` | Proxies, Connections, Rules, Logs |
| `j` / `k`, `↑` / `↓`, `PgUp` / `PgDn`, `g` / `G` | Move |
| `/` | Filter the list; `Esc` clears it |
| `m` | Cycle mode rule → global → direct |
| `u` | Update the subscription now |
| `r` | Refresh everything |
| `q` | Quit |

**Proxies.** Two panes; the focused one has the bright border. `←` / `→` (`h` / `l`) move between
groups and nodes, `Enter` opens a group or switches to the node under the cursor, `t` retests the
group, `s` toggles config order and fastest first. `Esc` steps back out of the node list.

**Connections.** `x` closes the selected connection, `D` closes all after a `y` confirmation, `s`
cycles the sort.

**Logs.** `v` cycles the minimum level, `c` clears, and scrolling up pauses following until `G`.
