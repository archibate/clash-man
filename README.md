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
3. saves the previous config to `~/.local/state/clash-man/config.yaml.bak`, writes the new one
   atomically, and hot-reloads the core through `PUT /configs` — no service restart;
4. restores the previous config if the core rejects the new one.

It runs only when the last success is older than `--update-interval` (default `6h`,
`CLASH_MAN_UPDATE_INTERVAL`); failures retry after 10 minutes. `--force` skips the check. The
systemd timer invokes it hourly, and the dashboard checks every 5 minutes while open. Both share
`~/.local/state/clash-man/subscription.json`, which also records the traffic and expiry the
provider reports in `subscription-userinfo`; the Overview page shows them.

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
